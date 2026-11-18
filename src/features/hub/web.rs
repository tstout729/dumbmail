//! The hub's public routes: its pages and form posts (shoppers, not team members) and the
//! launcher script the website snippet loads.
//!
//! Every page and form loads the store from the key in the path and the shopper from the session
//! cookie ([`super::session::current`]), never from anything else in the request. The hub answers
//! only on its own host (the store's branded link domain, else the shared link host). Form posts
//! must come from the hub's own origin, and signed-in forms also carry the session's form token.

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::page::{self, Frame};
use super::session::{self, Requested, Shopper};
use super::settings::{self, Settings};
use crate::audience::{self, NewEvent};
use crate::error::AppResult;
use crate::models::Account;
use crate::state::AppState;

const LAUNCHER_JS: &str = include_str!("hub.js");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/hub/{key}", get(home))
        .route("/hub/{key}/orders/{order}", get(order_h))
        .route("/hub/{key}/code", post(code_h))
        .route("/hub/{key}/verify", post(verify_h))
        .route("/hub/{key}/prefs", post(prefs_h))
        .route("/hub/{key}/stock/{id}/cancel", post(cancel_h))
        .route("/hub/{key}/signout", post(signout_h))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .route("/client/hub.js", get(launcher_js))
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    embed: Option<String>,
    /// A signed link from an email.
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    orders: Option<String>,
    #[serde(default)]
    saved: Option<String>,
}

impl PageQuery {
    fn embed(&self) -> bool {
        self.embed.as_deref() == Some("1")
    }
}

/// The store a hub URL names, if the hub may be served for it on this host.
struct Store {
    account: Account,
    settings: Settings,
}

fn not_found() -> Response {
    secure(StatusCode::NOT_FOUND, "not found".to_string(), None)
}

async fn store_for(state: &AppState, key: &str, headers: &HeaderMap) -> AppResult<Option<Store>> {
    if key.len() > 100 {
        return Ok(None);
    }
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1 AND closed_at IS NULL").bind(key).fetch_optional(&state.db).await?;
    let Some(id) = id else { return Ok(None) };
    let account = crate::campaigns::load_account(&state.db, id).await?;
    // Only on the hub's own host, so its cookie is never set or read anywhere else.
    let host = crate::site::request_host(headers);
    if !host.is_empty() && host != session::hub_host(state, &account) {
        return Ok(None);
    }
    let settings = settings::load(&state.db, id).await?;
    Ok(Some(Store { account, settings }))
}

/// Pages: no caching, no referrer, and framing only by the store's own site when the hub can open
/// in a panel there.
fn secure(status: StatusCode, html: String, frame_site: Option<&str>) -> Response {
    let ancestors = match frame_site {
        Some(site) => format!("'self' https://{site} https://*.{site}"),
        None => "'none'".into(),
    };
    let csp = format!("default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors {ancestors}");
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

fn html(store: &Store, status: StatusCode, body: String) -> Response {
    let site = settings::embed_site(&store.account);
    secure(status, body, site.as_deref())
}

fn redirect(to: &str, cookie: Option<axum_extra::extract::cookie::Cookie<'static>>) -> Response {
    let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_string())]).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(c) = cookie {
        if let Ok(v) = HeaderValue::from_str(&c.to_string()) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

/// The hub's own address (path only), keeping the panel mode, plus a fragment or query.
fn home_path(store: &Store, embed: bool, extra: &str) -> String {
    let base = session::cookie_path(&store.account);
    let (query, frag) = extra.split_once('#').map(|(q, f)| (q, Some(f))).unwrap_or((extra, None));
    let mut params: Vec<&str> = Vec::new();
    if !query.is_empty() {
        params.push(query);
    }
    if embed {
        params.push("embed=1");
    }
    let q = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
    format!("{base}{q}{}", frag.map(|f| format!("#{f}")).unwrap_or_default())
}

fn fail(e: crate::error::AppError) -> Response {
    tracing::error!(error = %e.public_message(), "hub page failed");
    secure(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Please try again in a minute.".into(), None)
}

fn saved_notice(key: Option<&str>) -> Option<String> {
    Some(
        match key? {
            "prefs" => "Your choices are saved.",
            "unsubscribed" => "You're unsubscribed from our emails.",
            "subscribed" => "You'll get our emails.",
            "sms" => "You won't get our texts any more.",
            "stock" => "Request cancelled.",
            _ => return None,
        }
        .to_string(),
    )
}

async fn home(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap) -> Response {
    let store = match store_for(&state, &key, &headers).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return fail(e),
    };
    let mut f = Frame::new(&store.account, &store.settings, q.embed());
    if !store.settings.enabled {
        return html(&store, StatusCode::NOT_FOUND, page::unavailable_page(&f));
    }
    // A signed link from an email: sign in, then drop the token from the address.
    if let Some(t) = q.t.as_deref().filter(|t| !t.is_empty() && t.len() <= 300) {
        match session::profile_from_link(&state, store.account.id, t).await {
            Ok(Some((profile_id, email))) => match session::start(&state.db, store.account.id, profile_id, &email, "link").await {
                Ok(token) => return redirect(&home_path(&store, q.embed(), ""), Some(session::session_cookie(&state, &store.account, token))),
                Err(e) => return fail(e),
            },
            Ok(None) => f.error = Some("That link has expired. Enter your email and we'll send you a code.".into()),
            Err(e) => return fail(e),
        }
    }
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let Some(shopper) = shopper else { return html(&store, StatusCode::OK, page::sign_in_page(&f, "")) };
    f.token = Some(session::form_token(&state, shopper.session_id));
    f.notice = saved_notice(q.saved.as_deref());
    let all = q.orders.as_deref() == Some("all");
    let view = match super::data::view(&state.db, &shopper, &store.settings, all).await {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let day = super::data::date(&store.account, chrono::Utc::now());
    record(&state, &shopper, super::OPENED_METRIC, json!({"embed": q.embed()}), format!("hub-open:{}:{day}", shopper.session_id.simple())).await;
    html(&store, StatusCode::OK, page::hub_page_with(&f, &view, all))
}

/// A hub event (server-only metrics: nobody else can record them). Failing to record never fails
/// the page.
async fn record(state: &AppState, shopper: &Shopper, metric: &str, properties: serde_json::Value, unique: String) {
    let ev = NewEvent {
        account_id: shopper.account.id,
        profile_id: shopper.profile_id,
        metric,
        properties,
        value: None,
        occurred_at: None,
        unique_id: Some(unique.chars().take(200).collect()),
        message_id: None,
    };
    if let Err(e) = audience::record_event(&state.db, ev).await {
        tracing::warn!(error = %e.public_message(), metric, "hub event not recorded");
    }
}

async fn order_h(State(state): State<AppState>, Path((key, order)): Path<(String, String)>, Query(q): Query<PageQuery>, headers: HeaderMap) -> Response {
    let store = match store_for(&state, &key, &headers).await {
        Ok(Some(s)) if s.settings.enabled && s.settings.sections.orders => s,
        Ok(_) => return not_found(),
        Err(e) => return fail(e),
    };
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(Some(s)) => s,
        Ok(None) => return redirect(&home_path(&store, q.embed(), ""), None),
        Err(e) => return fail(e),
    };
    let mut f = Frame::new(&store.account, &store.settings, q.embed());
    f.token = Some(session::form_token(&state, shopper.session_id));
    let found = match super::data::order(&state.db, &shopper, &order).await {
        Ok(o) => o,
        Err(e) => return fail(e),
    };
    let Some(o) = found else {
        f.error = Some("We couldn't find that order.".into());
        return html(&store, StatusCode::NOT_FOUND, page::document_only(&f, "Order"));
    };
    let day = super::data::date(&store.account, chrono::Utc::now());
    record(
        &state,
        &shopper,
        super::VIEWED_ORDER_METRIC,
        json!({"order_id": o.id, "order_number": o.number, "status": o.status.key()}),
        format!("hub-order:{}:{day}:{}", shopper.session_id.simple(), o.id),
    )
    .await;
    html(&store, StatusCode::OK, page::order_page(&f, &o))
}

/// Form posts must come from the hub's own page (its origin).
fn same_origin(state: &AppState, headers: &HeaderMap, account: &Account) -> bool {
    let from = headers.get(header::ORIGIN).or_else(|| headers.get(header::REFERER)).and_then(|v| v.to_str().ok());
    let Some(from) = from else { return false };
    let Ok(u) = url::Url::parse(from) else { return false };
    let Ok(hub) = url::Url::parse(&session::hub_url(state, account)) else { return false };
    u.scheme() == hub.scheme() && u.host_str().map(str::to_lowercase) == hub.host_str().map(str::to_lowercase) && u.port_or_known_default() == hub.port_or_known_default()
}

struct Form {
    fields: Vec<(String, String)>,
}

impl Form {
    fn parse(body: &[u8]) -> Self {
        Form { fields: url::form_urlencoded::parse(body).take(200).map(|(k, v)| (k.into_owned(), v.chars().take(500).collect())).collect() }
    }
    fn get(&self, k: &str) -> &str {
        self.fields.iter().find(|f| f.0 == k).map(|f| f.1.as_str()).unwrap_or("")
    }
    fn all(&self, k: &str) -> impl Iterator<Item = &str> {
        self.fields.iter().filter(move |f| f.0 == k).map(|f| f.1.as_str())
    }
}

/// The store, for a post from the hub's own page; None answers 403/404.
async fn post_store(state: &AppState, key: &str, headers: &HeaderMap) -> Result<Store, Box<Response>> {
    let store = match store_for(state, key, headers).await {
        Ok(Some(s)) if s.settings.enabled => s,
        Ok(_) => return Err(Box::new(not_found())),
        Err(e) => return Err(Box::new(fail(e))),
    };
    if !same_origin(state, headers, &store.account) {
        return Err(Box::new(secure(StatusCode::FORBIDDEN, "This form must be sent from the account page.".into(), None)));
    }
    Ok(store)
}

async fn code_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let store = match post_store(&state, &key, &headers).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let form = Form::parse(&body);
    let email = form.get("email").trim().to_string();
    let mut f = Frame::new(&store.account, &store.settings, q.embed());
    let ip = crate::public::client_ip(&headers);
    match session::request_code(&state, &store.account, &email, ip.as_deref()).await {
        Ok(Requested::Sent) => html(&store, StatusCode::OK, page::code_page(&f, &email)),
        Ok(Requested::TooMany) => {
            f.error = Some("Too many codes asked for. Please wait a while and try again.".into());
            html(&store, StatusCode::TOO_MANY_REQUESTS, page::sign_in_page(&f, &email))
        }
        Err(crate::error::AppError::BadRequest(m)) => {
            f.error = Some(m);
            html(&store, StatusCode::BAD_REQUEST, page::sign_in_page(&f, &email))
        }
        Err(e) => fail(e),
    }
}

async fn verify_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let store = match post_store(&state, &key, &headers).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let form = Form::parse(&body);
    let email = form.get("email").trim().to_string();
    match session::verify_code(&state, &store.account, &email, form.get("code")).await {
        Ok(Some((profile_id, email))) => match session::start(&state.db, store.account.id, profile_id, &email, "code").await {
            Ok(token) => redirect(&home_path(&store, q.embed(), ""), Some(session::session_cookie(&state, &store.account, token))),
            Err(e) => fail(e),
        },
        Ok(None) => {
            let mut f = Frame::new(&store.account, &store.settings, q.embed());
            f.error = Some("That code didn't work or has expired. Check it, or ask for a new one.".into());
            html(&store, StatusCode::BAD_REQUEST, page::code_page(&f, &email))
        }
        Err(e) => fail(e),
    }
}

/// The signed-in shopper for a form post with a valid form token; else where to send them.
async fn post_shopper(state: &AppState, store: &Store, headers: &HeaderMap, form: &Form, embed: bool) -> Result<Shopper, Box<Response>> {
    match session::current(state, headers, &store.account).await {
        Ok(Some(s)) if session::form_token_ok(state, s.session_id, form.get("t")) => Ok(s),
        Ok(Some(_)) => Err(Box::new(secure(StatusCode::FORBIDDEN, "This form has expired. Go back and reload the page.".into(), None))),
        Ok(None) => Err(Box::new(redirect(&home_path(store, embed, ""), None))),
        Err(e) => Err(Box::new(fail(e))),
    }
}

async fn prefs_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let store = match post_store(&state, &key, &headers).await {
        Ok(s) if s.settings.sections.preferences => s,
        Ok(_) => return not_found(),
        Err(r) => return *r,
    };
    let form = Form::parse(&body);
    let shopper = match post_shopper(&state, &store, &headers, &form, q.embed()).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let res = match form.get("action") {
        "email_unsubscribe" => prefs::unsubscribe(&state, &shopper, prefs::Channel::Email).await.map(|_| "unsubscribed"),
        "sms_unsubscribe" => prefs::unsubscribe(&state, &shopper, prefs::Channel::Sms).await.map(|_| "sms"),
        "email_subscribe" => prefs::subscribe_email(&state, &shopper).await.map(|_| "subscribed"),
        "save_lists" => prefs::save_lists(&state, &shopper, form.all("list").filter_map(|v| v.parse().ok()).collect()).await.map(|_| "prefs"),
        _ => return secure(StatusCode::BAD_REQUEST, "Unknown choice.".into(), None),
    };
    match res {
        Ok(saved) => redirect(&home_path(&store, q.embed(), &format!("saved={saved}#preferences")), None),
        Err(e) => fail(e),
    }
}

async fn cancel_h(State(state): State<AppState>, Path((key, id)): Path<(String, String)>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let store = match post_store(&state, &key, &headers).await {
        Ok(s) if s.settings.sections.recommendations => s,
        Ok(_) => return not_found(),
        Err(r) => return *r,
    };
    let form = Form::parse(&body);
    let shopper = match post_shopper(&state, &store, &headers, &form, q.embed()).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let Ok(id) = id.parse::<Uuid>() else { return not_found() };
    match super::data::cancel_stock_request(&state.db, &shopper, id).await {
        Ok(true) => redirect(&home_path(&store, q.embed(), "saved=stock#picks"), None),
        Ok(false) => not_found(),
        Err(e) => fail(e),
    }
}

async fn signout_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap) -> Response {
    let store = match store_for(&state, &key, &headers).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return fail(e),
    };
    if !same_origin(&state, &headers, &store.account) {
        return secure(StatusCode::FORBIDDEN, "This form must be sent from the account page.".into(), None);
    }
    if let Err(e) = session::sign_out(&state, &headers, &store.account).await {
        return fail(e);
    }
    redirect(&home_path(&store, q.embed(), ""), Some(session::removal_cookie(&state, &store.account)))
}

#[derive(Deserialize)]
struct KeyQuery {
    k: String,
}

/// `GET /client/hub.js?k=`: the launcher (a "My account" button or panel) with this store's hub
/// settings, or a no-op when the hub is off.
async fn launcher_js(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let config = async {
        if q.k.len() > 100 {
            return None;
        }
        let id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1 AND closed_at IS NULL").bind(&q.k).fetch_optional(&state.db).await.ok()??;
        let s = settings::load(&state.db, id).await.ok()?;
        if !s.enabled {
            return None;
        }
        let a = crate::campaigns::load_account(&state.db, id).await.ok()?;
        Some(json!({
            "url": session::hub_url(&state, &a),
            "launcher": s.launcher.as_str(),
            "label": s.button_label,
            "color": crate::render::safe_color(s.accent_color.as_deref().unwrap_or(&a.brand_color), "#1f6f5c"),
            "font": s.font.css(),
            "embed_site": settings::embed_site(&a),
        }))
    }
    .await;
    // JSON inside a script: `<` is escaped so no value can close the script tag.
    let cfg = config.map(|c| c.to_string().replace('<', "\\u003c")).unwrap_or_else(|| "null".into());
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")],
        LAUNCHER_JS.replacen("__CONFIG__", &cfg, 1),
    )
        .into_response()
}

/// Email and text preferences, written the way the preference page writes them: the same
/// statements (a row lock on the person, only a real change recorded), the same events and the
/// same webhook notices, with `method: hub`. Unsubscribing always works; subscribing to email is the
/// shopper's own opt-in for the address they proved they own, and never for an address that bounced
/// or was reported. Texts can only be stopped here (the code proves the email, not the number).
pub mod prefs {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Channel {
        Email,
        Sms,
    }

    pub async fn unsubscribe(state: &AppState, s: &Shopper, ch: Channel) -> AppResult<bool> {
        let (col, metric, name) = match ch {
            Channel::Email => ("email_consent", "Unsubscribed", "email"),
            Channel::Sms => ("sms_consent", "Unsubscribed from SMS", "sms"),
        };
        let before: Option<String> = sqlx::query_scalar(&format!(
            "UPDATE profiles p SET {col} = 'unsubscribed', updated_at = now() FROM (SELECT id, {col} AS before FROM profiles WHERE id = $1 AND account_id = $2 FOR UPDATE) old
             WHERE p.id = old.id AND old.before <> 'unsubscribed' RETURNING old.before"
        ))
        .bind(s.profile_id)
        .bind(s.account.id)
        .fetch_optional(&state.db)
        .await?;
        let Some(before) = before else { return Ok(false) };
        audience::record_event(
            &state.db,
            NewEvent { account_id: s.account.id, profile_id: s.profile_id, metric, properties: json!({"method": "hub"}), value: None, occurred_at: None, unique_id: None, message_id: None },
        )
        .await?;
        crate::features::webhooks::consent_changed(&state.db, s.account.id, s.profile_id, name, &before, "unsubscribed", "hub", None).await;
        Ok(true)
    }

    pub async fn subscribe_email(state: &AppState, s: &Shopper) -> AppResult<bool> {
        // Only the address the session proved, and only while it's still the person's and isn't
        // suppressed.
        let before: Option<String> = sqlx::query_scalar(
            "UPDATE profiles p SET email_consent = 'subscribed', updated_at = now()
             FROM (SELECT id, email_consent AS before FROM profiles WHERE id = $1 AND account_id = $2 AND email = $3 AND email_suppressed_reason IS NULL FOR UPDATE) old
             WHERE p.id = old.id AND old.before <> 'subscribed' RETURNING old.before",
        )
        .bind(s.profile_id)
        .bind(s.account.id)
        .bind(&s.email)
        .fetch_optional(&state.db)
        .await?;
        let Some(before) = before else { return Ok(false) };
        audience::record_event(
            &state.db,
            NewEvent {
                account_id: s.account.id,
                profile_id: s.profile_id,
                metric: "Subscribed to Email Marketing",
                properties: json!({"method": "hub"}),
                value: None,
                occurred_at: None,
                unique_id: None,
                message_id: None,
            },
        )
        .await?;
        crate::features::webhooks::consent_changed(&state.db, s.account.id, s.profile_id, "email", &before, "subscribed", "hub", None).await;
        Ok(true)
    }

    /// The preference page's list choices (only while subscribed to email, as there).
    pub async fn save_lists(state: &AppState, s: &Shopper, keep: HashSet<Uuid>) -> AppResult<usize> {
        let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1 AND account_id = $2").bind(s.profile_id).bind(s.account.id).fetch_one(&state.db).await?;
        if consent != "subscribed" {
            return Ok(0);
        }
        crate::preferences::save(&state.db, s.account.id, s.profile_id, &keep, None).await
    }
}

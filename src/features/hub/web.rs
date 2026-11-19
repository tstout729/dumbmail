//! The hub's public routes: its pages and form posts (shoppers, not team members) and the
//! launcher script the website snippet loads.
//!
//! Every page and form loads the store from the key in the path and the shopper from the session
//! cookie ([`super::session::current`]), never from anything else in the request. The hub answers
//! only on its own host (the store's branded link domain, else the shared link host). Every form
//! post must come from the hub's exact origin and carry a form token: the session's once signed
//! in, else one tied to this browser by a cookie. Only pages opened in the panel may be framed, and
//! only by the origins the store approved.

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use axum_extra::extract::cookie::Cookie;
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
        .route("/hub/{key}/link", post(link_h))
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
    /// A sign-in link from an email.
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

/// The hub's own "not found" (also for any other path under `/hub/`, see `web::serve`).
pub fn not_found() -> Response {
    secure(StatusCode::NOT_FOUND, "not found".to_string(), &[])
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

/// Pages: no caching, no referrer to other sites, and framing only by `ancestors` (none: never).
fn secure(status: StatusCode, html: String, ancestors: &[String]) -> Response {
    let ancestors = if ancestors.is_empty() { "'none'".to_string() } else { ancestors.join(" ") };
    let csp = format!("default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors {ancestors}");
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // same-origin, not no-referrer: with no-referrer browsers send `Origin: null` on the hub's own
    // form posts, which the origin check would refuse. Nothing goes to other sites either way.
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

fn with_cookies(mut r: Response, cookies: Vec<Cookie<'static>>) -> Response {
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(&c.to_string()) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

/// A hub page. Only a page opened in the panel (`embed`) may be framed, and only by the store's
/// approved origins.
fn html(store: &Store, embed: bool, status: StatusCode, body: String, cookies: Vec<Cookie<'static>>) -> Response {
    let ancestors = if embed { settings::panel_origins(&store.account, &store.settings) } else { Vec::new() };
    with_cookies(secure(status, body, &ancestors), cookies)
}

fn redirect(to: &str, cookies: Vec<Cookie<'static>>) -> Response {
    let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_string())]).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    with_cookies(r, cookies)
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
    secure(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Please try again in a minute.".into(), &[])
}

fn forbidden(msg: &str) -> Response {
    secure(StatusCode::FORBIDDEN, msg.to_string(), &[])
}

/// What the page says after a change: exactly what happened.
fn saved_notice(key: Option<&str>) -> Option<String> {
    Some(
        match key? {
            "prefs" => "Your choices are saved.",
            "unsubscribed" => "You're unsubscribed from our emails.",
            "subscribed" => "You'll get our emails.",
            "suppressed" => "Nothing was changed: we can't email this address, because messages to it bounced or were reported.",
            "nochange" => "Nothing needed changing.",
            "sms" => "You won't get our texts any more.",
            "stock" => "Request cancelled.",
            _ => return None,
        }
        .to_string(),
    )
}

/// A page for someone not signed in: its forms carry a token tied to this browser (a cookie,
/// set here when it's new).
fn anon_frame<'a>(state: &AppState, store: &'a Store, headers: &HeaderMap, embed: bool) -> (Frame<'a>, Vec<Cookie<'static>>) {
    let mut f = Frame::new(&store.account, &store.settings, embed);
    let (nonce, cookies) = match session::form_nonce(headers) {
        Some(n) => (n, Vec::new()),
        None => {
            let (n, c) = session::new_form_nonce(state, &store.account);
            (n, vec![c])
        }
    };
    f.token = Some(session::anon_form_token(state, &nonce));
    (f, cookies)
}

fn signed_in_frame<'a>(state: &AppState, store: &'a Store, shopper: &Shopper, embed: bool) -> Frame<'a> {
    let mut f = Frame::new(&store.account, &store.settings, embed);
    f.token = Some(session::form_token(state, shopper.session_id));
    f.signed_in = true;
    f
}

async fn home(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap) -> Response {
    let store = match store_for(&state, &key, &headers).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return fail(e),
    };
    let embed = q.embed();
    if !store.settings.enabled {
        let f = Frame::new(&store.account, &store.settings, embed);
        return html(&store, embed, StatusCode::NOT_FOUND, page::unavailable_page(&f), Vec::new());
    }
    // A sign-in link from an email: a page with one button whose POST uses it, so a mail scanner
    // opening the link neither uses it up nor signs anyone in.
    if let Some(t) = q.t.as_deref().filter(|t| !t.is_empty()) {
        let (mut f, cookies) = anon_frame(&state, &store, &headers, embed);
        return match session::check_link(&state, store.account.id, t).await {
            Ok(Some(_)) => html(&store, embed, StatusCode::OK, page::link_page(&f, t), cookies),
            Ok(None) => {
                f.error = Some(LINK_GONE.into());
                html(&store, embed, StatusCode::OK, page::sign_in_page(&f, ""), cookies)
            }
            Err(e) => fail(e),
        };
    }
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let Some(shopper) = shopper else {
        let (f, cookies) = anon_frame(&state, &store, &headers, embed);
        return html(&store, embed, StatusCode::OK, page::sign_in_page(&f, ""), cookies);
    };
    let mut f = signed_in_frame(&state, &store, &shopper, embed);
    f.notice = saved_notice(q.saved.as_deref());
    f.chat = crate::features::agent::web::hub_chat_name(&state, &store.account).await;
    let all = q.orders.as_deref() == Some("all");
    let view = match super::data::view(&state.db, &shopper, &store.settings, all).await {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let day = super::data::date(&store.account, chrono::Utc::now());
    record(&state, &shopper, super::OPENED_METRIC, json!({"embed": embed}), format!("hub-open:{}:{day}", shopper.session_id.simple())).await;
    html(&store, embed, StatusCode::OK, page::hub_page_with(&f, &view, all), Vec::new())
}

const LINK_GONE: &str = "That sign-in link has expired or was already used. Enter your email and we'll send you a code.";

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
    let embed = q.embed();
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(Some(s)) => s,
        Ok(None) => return redirect(&home_path(&store, embed, ""), Vec::new()),
        Err(e) => return fail(e),
    };
    let mut f = signed_in_frame(&state, &store, &shopper, embed);
    let found = match super::data::order(&state.db, &shopper, &order).await {
        Ok(o) => o,
        Err(e) => return fail(e),
    };
    let Some(o) = found else {
        f.error = Some("We couldn't find that order.".into());
        return html(&store, embed, StatusCode::NOT_FOUND, page::document_only(&f, "Order"), Vec::new());
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
    html(&store, embed, StatusCode::OK, page::order_page(&f, &o), Vec::new())
}

/// Form posts must come from the hub's exact origin (scheme, host and port).
fn same_origin(state: &AppState, headers: &HeaderMap, account: &Account) -> bool {
    let from = headers.get(header::ORIGIN).or_else(|| headers.get(header::REFERER)).and_then(|v| v.to_str().ok());
    let Some(from) = from else { return false };
    let Ok(u) = url::Url::parse(from) else { return false };
    let Ok(hub) = url::Url::parse(&session::hub_url(state, account)) else { return false };
    u.origin() == hub.origin()
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

/// The store, for a post from the hub's own page; else the answer to send.
async fn post_store(state: &AppState, key: &str, headers: &HeaderMap) -> Result<Store, Box<Response>> {
    let store = match store_for(state, key, headers).await {
        Ok(Some(s)) if s.settings.enabled => s,
        Ok(_) => return Err(Box::new(not_found())),
        Err(e) => return Err(Box::new(fail(e))),
    };
    if !same_origin(state, headers, &store.account) {
        return Err(Box::new(forbidden("This form must be sent from the account page.")));
    }
    Ok(store)
}

/// A sign-in form post (code, link): from the hub's origin, with this browser's form token.
async fn anon_post(state: &AppState, key: &str, headers: &HeaderMap, form: &Form) -> Result<Store, Box<Response>> {
    let store = post_store(state, key, headers).await?;
    if !session::anon_form_token_ok(state, headers, form.get("t")) {
        return Err(Box::new(forbidden("This form has expired. Go back and reload the page.")));
    }
    Ok(store)
}

async fn code_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let store = match anon_post(&state, &key, &headers, &form).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let embed = q.embed();
    let email = form.get("email").trim().to_string();
    let (mut f, cookies) = anon_frame(&state, &store, &headers, embed);
    let ip = crate::public::client_ip(&headers);
    match session::request_code(&state, &store.account, &email, ip.as_deref()).await {
        Ok(Requested::Sent) => html(&store, embed, StatusCode::OK, page::code_page(&f, &email), cookies),
        Ok(Requested::TooMany) => {
            f.error = Some("Too many codes asked for. Please wait a while and try again.".into());
            html(&store, embed, StatusCode::TOO_MANY_REQUESTS, page::sign_in_page(&f, &email), cookies)
        }
        Err(crate::error::AppError::BadRequest(m)) => {
            f.error = Some(m);
            html(&store, embed, StatusCode::BAD_REQUEST, page::sign_in_page(&f, &email), cookies)
        }
        Err(e) => fail(e),
    }
}

async fn verify_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let store = match anon_post(&state, &key, &headers, &form).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let embed = q.embed();
    let email = form.get("email").trim().to_string();
    match session::verify_code(&state, &store.account, &email, form.get("code")).await {
        Ok(Some((profile_id, email))) => match session::start(&state.db, store.account.id, profile_id, &email, "code").await {
            Ok(token) => redirect(&home_path(&store, embed, ""), vec![session::session_cookie(&state, &store.account, token)]),
            Err(e) => fail(e),
        },
        Ok(None) => {
            let (mut f, cookies) = anon_frame(&state, &store, &headers, embed);
            f.error = Some("That code didn't work or has expired. Check it, or ask for a new one.".into());
            html(&store, embed, StatusCode::BAD_REQUEST, page::code_page(&f, &email), cookies)
        }
        Err(e) => fail(e),
    }
}

/// "Continue to your account" from a sign-in link: uses the link up (once) and signs in.
async fn link_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let store = match anon_post(&state, &key, &headers, &form).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let embed = q.embed();
    let link = match session::check_link(&state, store.account.id, form.get("link")).await {
        Ok(l) => l,
        Err(e) => return fail(e),
    };
    let redeemed = match &link {
        Some(l) => match session::redeem_link(&state.db, store.account.id, l).await {
            Ok(r) => r,
            Err(e) => return fail(e),
        },
        None => false,
    };
    match (link, redeemed) {
        (Some(l), true) => match session::start(&state.db, store.account.id, l.profile_id, &l.email, "link").await {
            Ok(token) => redirect(&home_path(&store, embed, ""), vec![session::session_cookie(&state, &store.account, token)]),
            Err(e) => fail(e),
        },
        _ => {
            let (mut f, cookies) = anon_frame(&state, &store, &headers, embed);
            f.error = Some(LINK_GONE.into());
            html(&store, embed, StatusCode::BAD_REQUEST, page::sign_in_page(&f, ""), cookies)
        }
    }
}

/// The signed-in shopper for a form post with the session's form token; else the answer to send.
async fn post_shopper(state: &AppState, store: &Store, headers: &HeaderMap, form: &Form, embed: bool) -> Result<Shopper, Box<Response>> {
    match session::current(state, headers, &store.account).await {
        Ok(Some(s)) if session::form_token_ok(state, s.session_id, form.get("t")) => Ok(s),
        Ok(Some(_)) => Err(Box::new(forbidden("This form has expired. Go back and reload the page."))),
        Ok(None) => Err(Box::new(redirect(&home_path(store, embed, ""), Vec::new()))),
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
        "email_unsubscribe" => prefs::unsubscribe(&state, &shopper, "email").await.map(|c| if c { "unsubscribed" } else { "nochange" }),
        "sms_unsubscribe" => prefs::unsubscribe(&state, &shopper, "sms").await.map(|c| if c { "sms" } else { "nochange" }),
        "email_subscribe" => prefs::subscribe_email(&state, &shopper).await,
        "save_lists" => prefs::save_lists(&state, &shopper, form.all("list").filter_map(|v| v.parse().ok()).collect()).await,
        _ => return secure(StatusCode::BAD_REQUEST, "Unknown choice.".into(), &[]),
    };
    match res {
        Ok(saved) => redirect(&home_path(&store, q.embed(), &format!("saved={saved}#preferences")), Vec::new()),
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
    // Requests are personal: only for an address the store gave this person, like orders.
    match super::data::trusted_email(&state.db, &shopper).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => return fail(e),
    }
    match super::data::cancel_stock_request(&state.db, &shopper, id).await {
        Ok(true) => redirect(&home_path(&store, q.embed(), "saved=stock#picks"), Vec::new()),
        Ok(false) => not_found(),
        Err(e) => fail(e),
    }
}

/// Sign out this browser, or (`everywhere`) end every session, link and code of this person.
async fn signout_h(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<PageQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let store = match store_for(&state, &key, &headers).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return fail(e),
    };
    if !same_origin(&state, &headers, &store.account) {
        return forbidden("This form must be sent from the account page.");
    }
    let form = Form::parse(&body);
    let embed = q.embed();
    let gone = session::removal_cookie(&state, &store.account);
    let shopper = match session::current(&state, &headers, &store.account).await {
        Ok(Some(s)) if session::form_token_ok(&state, s.session_id, form.get("t")) => s,
        Ok(Some(_)) => return forbidden("This form has expired. Go back and reload the page."),
        Ok(None) => return redirect(&home_path(&store, embed, ""), vec![gone]),
        Err(e) => return fail(e),
    };
    let res = if form.get("everywhere") == "1" { session::end_sessions(&state.db, shopper.profile_id).await.map_err(Into::into) } else { session::sign_out(&state, &headers, &store.account).await };
    match res {
        Ok(()) => redirect(&home_path(&store, embed, ""), vec![gone]),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct KeyQuery {
    k: String,
}

/// `GET /client/hub.js?k=`: the launcher (a "My account" button or panel) with this store's hub
/// settings, or a no-op when the hub is off. The snippet loads it only when its storefront config
/// says a launcher is on.
async fn launcher_js(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let config = async {
        if q.k.len() > 100 {
            return None;
        }
        let id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1 AND closed_at IS NULL").bind(&q.k).fetch_optional(&state.db).await.ok()??;
        let s = settings::load(&state.db, id).await.ok()?;
        if !s.enabled || s.launcher == settings::Launcher::Off {
            return None;
        }
        let a = crate::campaigns::load_account(&state.db, id).await.ok()?;
        Some(json!({
            "url": session::hub_url(&state, &a),
            "launcher": s.launcher.as_str(),
            "label": s.button_label,
            "color": crate::render::safe_color(s.accent_color.as_deref().unwrap_or(&a.brand_color), "#1f6f5c"),
            "font": s.font.css(),
            "panel_origins": settings::panel_origins(&a, &s),
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

/// Email and text preferences, through the preference page's own functions: opt-outs with
/// [`crate::preferences::opt_out`] (source `hub`), list choices with [`crate::preferences::save`].
/// Subscribing to email is the shopper's own opt-in for the address they proved they own, never
/// for an address that bounced or was reported. Texts can only be stopped here (the code proves the
/// email, not the number). Each returns what happened, for the page to say.
pub mod prefs {
    use super::*;

    pub async fn unsubscribe(state: &AppState, s: &Shopper, channel: &str) -> AppResult<bool> {
        let src = crate::preferences::OptOutSource { method: "hub", reason: "hub", message: None };
        crate::preferences::opt_out(&state.db, s.account.id, s.profile_id, channel, &src).await
    }

    pub async fn subscribe_email(state: &AppState, s: &Shopper) -> AppResult<&'static str> {
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
        let Some(before) = before else {
            let suppressed: bool = sqlx::query_scalar("SELECT email_suppressed_reason IS NOT NULL FROM profiles WHERE id = $1 AND account_id = $2")
                .bind(s.profile_id)
                .bind(s.account.id)
                .fetch_optional(&state.db)
                .await?
                .unwrap_or(false);
            return Ok(if suppressed { "suppressed" } else { "nochange" });
        };
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
        Ok("subscribed")
    }

    /// The preference page's list choices (only while subscribed to email, as there).
    pub async fn save_lists(state: &AppState, s: &Shopper, keep: HashSet<Uuid>) -> AppResult<&'static str> {
        let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1 AND account_id = $2").bind(s.profile_id).bind(s.account.id).fetch_one(&state.db).await?;
        if consent != "subscribed" {
            return Ok("nochange");
        }
        let changed = crate::preferences::save(&state.db, s.account.id, s.profile_id, &keep, None).await?;
        Ok(if changed > 0 { "prefs" } else { "nochange" })
    }
}

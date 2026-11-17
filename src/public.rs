//! Unauthenticated endpoints: tracking, unsubscribe, browser events, sign-up forms and provider webhooks.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::audience::{self, NewEvent, ProfileInput};
use crate::auth;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_phone, Account, FormRow};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/t/o/{token}", get(open_pixel))
        .route("/t/c/{token}", get(click))
        .route("/u/{token}", get(unsubscribe_page).post(unsubscribe_post))
        .route("/c/{token}", get(confirm_page).post(confirm_post))
        .route("/onsite.js", get(onsite_js))
        .route("/client/dumbmail.js", get(onsite_js))
        .route("/client/postlark.js", get(onsite_js))
        .route("/client/forms", get(client_forms))
        .route("/client/form-render.js", get(form_render_js))
        .route("/client/ping", post(client_ping).options(cors_preflight))
        .route("/client/events", post(client_event).options(cors_preflight))
        .route("/client/test", get(client_test_status).post(client_test).options(cors_preflight).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)))
        .route("/client/subscribe", post(client_subscribe).options(cors_preflight))
        .route("/client/forms/{id}/view", post(form_view).options(cors_preflight))
        .route("/f/{id}", get(form_page))
        .route("/f/{id}/embed.js", get(form_embed))
        .route("/i/{id}", get(image))
        .route("/webhooks/ses/{secret}", post(ses_webhook))
        .route("/webhooks/telnyx/{secret}", post(telnyx_webhook))
        .route("/webhooks/twilio/{secret}", post(twilio_webhook))
        .route("/webhooks/stripe", post(stripe_webhook))
        .route("/admin/credit", post(admin_credit))
        .route("/admin/unpause", post(admin_unpause))
        .route("/admin/sending-limit", post(admin_sending_limit))
        .route("/healthz", get(healthz))
}

const GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

fn message_id_from(state: &AppState, token: &str, prefix: &str) -> Option<Uuid> {
    let payload = auth::verify(&state.cfg.secret_key, token)?;
    let hex = payload.strip_prefix(prefix)?;
    Uuid::parse_str(hex).ok()
}

#[derive(sqlx::FromRow)]
struct Msg {
    id: Uuid,
    account_id: Uuid,
    profile_id: Uuid,
    channel: String,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    is_test: bool,
}

async fn load_msg(state: &AppState, id: Uuid) -> AppResult<Option<Msg>> {
    Ok(sqlx::query_as("SELECT id, account_id, profile_id, channel, campaign_id, flow_id, is_test FROM messages WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?)
}

/// Liveness, plus the commit this build came from (set by the deploy job; empty for local builds).
async fn healthz() -> Json<Value> {
    Json(json!({"ok": true, "git_sha": option_env!("GIT_SHA").unwrap_or("")}))
}

async fn msg_event(state: &AppState, m: &Msg, metric: &str, extra: Value, unique: Option<String>) -> AppResult<()> {
    if m.is_test {
        return Ok(());
    }
    let mut props = json!({"campaign_id": m.campaign_id, "flow_id": m.flow_id, "message_id": m.id});
    if let (Some(o), Value::Object(e)) = (props.as_object_mut(), extra) {
        o.extend(e);
    }
    audience::record_event(
        &state.db,
        NewEvent { account_id: m.account_id, profile_id: m.profile_id, metric, properties: props, value: None, occurred_at: None, unique_id: unique, message_id: Some(m.id) },
    )
    .await?;
    Ok(())
}

async fn open_pixel(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    if let Some(id) = message_id_from(&state, &token, "") {
        if let Ok(Some(m)) = load_msg(&state, id).await {
            let first: Result<Option<Uuid>, _> = sqlx::query_scalar("UPDATE messages SET opened_at = now() WHERE id = $1 AND opened_at IS NULL RETURNING id")
                .bind(id)
                .fetch_optional(&state.db)
                .await;
            if matches!(first, Ok(Some(_))) {
                let _ = msg_event(&state, &m, "Opened Email", json!({}), Some(format!("open:{id}"))).await;
            }
        }
    }
    (
        [(header::CONTENT_TYPE, "image/gif"), (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0")],
        GIF,
    )
        .into_response()
}

#[derive(Deserialize)]
struct ClickQuery {
    u: String,
    s: String,
}

async fn click(State(state): State<AppState>, Path(token): Path<String>, Query(q): Query<ClickQuery>) -> Response {
    let Some(id) = message_id_from(&state, &token, "") else {
        return (StatusCode::BAD_REQUEST, "invalid link").into_response();
    };
    // Only redirect to URLs we signed, so this can't be used as an open redirect.
    if auth::link_sig(&state.cfg.secret_key, id, &q.u) != q.s || !(q.u.starts_with("https://") || q.u.starts_with("http://")) {
        return (StatusCode::BAD_REQUEST, "invalid link").into_response();
    }
    if let Ok(Some(m)) = load_msg(&state, id).await {
        let _ = sqlx::query("INSERT INTO message_clicks (message_id, url) VALUES ($1, $2)").bind(id).bind(&q.u).execute(&state.db).await;
        let first: Result<Option<Uuid>, _> = sqlx::query_scalar(
            "UPDATE messages SET clicked_at = COALESCE(clicked_at, now()), opened_at = CASE WHEN channel = 'email' THEN COALESCE(opened_at, now()) ELSE opened_at END
             WHERE id = $1 AND clicked_at IS NULL RETURNING id",
        )
        .bind(id)
        .fetch_optional(&state.db)
        .await;
        let metric = if m.channel == "sms" { "Clicked SMS" } else { "Clicked Email" };
        let _ = msg_event(&state, &m, metric, json!({"url": q.u}), None).await;
        if matches!(first, Ok(Some(_))) && m.channel == "email" {
            let _ = msg_event(&state, &m, "Opened Email", json!({}), Some(format!("open:{id}"))).await;
        }
        if !m.is_test {
            // Lets the onsite script recognize the person on the brand's own site.
            if let (Ok(mut dest), Some(account)) = (url::Url::parse(&q.u), account_for_msg(&state, &m).await) {
                if is_own_site(&account, &dest) {
                    dest.query_pairs_mut().append_pair("_pl", &profile_token(&state, m.account_id, m.profile_id));
                    return Redirect::to(dest.as_str()).into_response();
                }
            }
        }
    }
    Redirect::to(&q.u).into_response()
}

fn page(title: &str, body: &str, color: &str) -> Html<String> {
    let color = crate::render::safe_color(color, "#1f6f5c");
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{t}</title>
<style>body{{margin:0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#f4f4f5;color:#111827;display:flex;min-height:100vh;align-items:center;justify-content:center}}
.c{{background:#fff;border-radius:12px;padding:32px;max-width:420px;width:calc(100% - 32px);box-shadow:0 1px 3px rgba(0,0,0,.08)}}h1{{font-size:22px;margin:0 0 12px}}p{{color:#4b5563;line-height:1.5}}
button,input[type=submit]{{background:{color};color:#fff;border:0;border-radius:8px;padding:12px 20px;font-size:16px;font-weight:600;cursor:pointer;width:100%}}
input[type=email],input[type=tel],input[type=text]{{width:100%;box-sizing:border-box;padding:12px;border:1px solid #d1d5db;border-radius:8px;font-size:16px;margin:0 0 10px}}label{{font-size:13px;color:#4b5563}}input[type=checkbox]{{accent-color:{color}}}</style></head><body><div class="c">{body}</div></body></html>"#,
        t = esc(title)
    ))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn account_for_msg(state: &AppState, m: &Msg) -> Option<Account> {
    crate::campaigns::load_account(&state.db, m.account_id).await.ok()
}

async fn unsubscribe_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some(id) = message_id_from(&state, &token, "u") else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let Ok(Some(m)) = load_msg(&state, id).await else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let a = account_for_msg(&state, &m).await;
    let (name, color) = a.map(|a| (a.name, a.brand_color)).unwrap_or(("this sender".into(), "#1f6f5c".into()));
    // Email with preference lists: let them choose lists instead of only leaving everything.
    if let Some(lists) = preference_lists(&state, &m).await {
        return page("Email preferences", &crate::preferences::form_html(&name, &lists, None), &color).into_response();
    }
    let what = if m.channel == "sms" { "text messages" } else { "marketing emails" };
    page(
        "Unsubscribe",
        &format!(r#"<h1>Unsubscribe</h1><p>Stop receiving {what} from {n}?</p><form method="post"><button type="submit">Unsubscribe</button></form>"#, n = esc(&name)),
        &color,
    )
    .into_response()
}

/// The preference-page lists for this email's recipient, when there are any and they haven't already
/// unsubscribed from all email. None means show the plain unsubscribe page.
async fn preference_lists(state: &AppState, m: &Msg) -> Option<Vec<crate::preferences::PrefList>> {
    if m.channel != "email" {
        return None;
    }
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1").bind(m.profile_id).fetch_optional(&state.db).await.ok()??;
    if consent == "unsubscribed" {
        return None;
    }
    let lists = crate::preferences::lists_for(&state.db, m.account_id, m.profile_id).await.ok()?;
    (!lists.is_empty()).then_some(lists)
}

async fn unsubscribe_post(State(state): State<AppState>, Path(token): Path<String>, body: Bytes) -> Response {
    let Some(id) = message_id_from(&state, &token, "u") else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let Ok(Some(m)) = load_msg(&state, id).await else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    if let (crate::preferences::Choice::Save(keep), "email") = (crate::preferences::parse_choice(&body), m.channel.as_str()) {
        let a = account_for_msg(&state, &m).await;
        let (name, color) = a.map(|a| (a.name, a.brand_color)).unwrap_or(("this sender".into(), "#1f6f5c".into()));
        let notice = if m.is_test {
            "This was a test email, so nothing changed."
        } else if crate::preferences::save(&state.db, m.account_id, m.profile_id, &keep, Some((m.id, m.campaign_id, m.flow_id))).await.is_ok() {
            "Saved. Your preferences are updated."
        } else {
            "Something went wrong. Please try again."
        };
        let lists = crate::preferences::lists_for(&state.db, m.account_id, m.profile_id).await.unwrap_or_default();
        return page("Email preferences", &crate::preferences::form_html(&name, &lists, Some(notice)), &color).into_response();
    }
    let col = if m.channel == "sms" { "sms_consent" } else { "email_consent" };
    let changed: Result<Option<Uuid>, _> = sqlx::query_scalar(&format!(
        "UPDATE profiles SET {col} = 'unsubscribed', updated_at = now() WHERE id = $1 AND {col} <> 'unsubscribed' RETURNING id"
    ))
    .bind(m.profile_id)
    .fetch_optional(&state.db)
    .await;
    let _ = sqlx::query("UPDATE messages SET unsubscribed_at = COALESCE(unsubscribed_at, now()) WHERE id = $1").bind(id).execute(&state.db).await;
    if matches!(changed, Ok(Some(_))) {
        let metric = if m.channel == "sms" { "Unsubscribed from SMS" } else { "Unsubscribed" };
        let _ = msg_event(&state, &m, metric, json!({"method": "link"}), None).await;
    }
    let a = account_for_msg(&state, &m).await;
    let (name, color) = a.map(|a| (a.name, a.brand_color)).unwrap_or(("this sender".into(), "#1f6f5c".into()));
    page("Unsubscribed", &format!("<h1>You're unsubscribed</h1><p>You won't get marketing messages from {} anymore.</p>", esc(&name)), &color).into_response()
}

/// The visitor's address as the hosting proxy saw it: X-Real-IP, else the last X-Forwarded-For entry (the
/// one the proxy added; earlier entries come from the visitor and can be made up). Only a valid IP counts.
/// The per-visitor limit is a second line of defense; the per-inbox and per-account limits don't use it.
fn client_ip(headers: &HeaderMap) -> Option<String> {
    let get = |h: &str| headers.get(h).and_then(|v| v.to_str().ok());
    let ip = |v: &str| v.trim().parse::<std::net::IpAddr>().ok().map(|a| a.to_string());
    get("x-real-ip").and_then(ip).or_else(|| get("x-forwarded-for").and_then(|v| v.rsplit(',').next()).and_then(ip))
}

/// Double opt-in link from a confirmation email. Confirming takes a button press (a POST), so link
/// scanners that open every link in an email can't subscribe anyone.
async fn confirm_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    confirm_response(&state, &token, false).await
}

async fn confirm_post(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    confirm_response(&state, &token, true).await
}

async fn confirm_response(state: &AppState, token: &str, confirm: bool) -> Response {
    use crate::optin::Confirmed;
    let Some(id) = crate::optin::id_from_token(state, token) else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let res = if confirm { crate::optin::confirm(state, id).await } else { crate::optin::status(state, id).await };
    let outcome = match res {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e.public_message(), "double opt-in confirmation failed");
            return page("Something went wrong", "<h1>Something went wrong</h1><p>Please try the link again in a minute.</p>", "#1f6f5c").into_response();
        }
    };
    let account_of = |a: Uuid| async move { crate::campaigns::load_account(&state.db, a).await.ok() };
    let (account, body) = match outcome {
        Confirmed::NotFound => return (StatusCode::NOT_FOUND, "link not found").into_response(),
        Confirmed::Waiting(a) => {
            let acct = account_of(a).await;
            let name = acct.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| "this sender".into());
            let body = format!(r#"<h1>Confirm your subscription</h1><p>Start getting emails from {}?</p><form method="post"><button type="submit">Yes, subscribe me</button></form>"#, esc(&name));
            (acct, body)
        }
        Confirmed::Joined(a) | Confirmed::Already(a) => {
            let acct = account_of(a).await;
            let name = acct.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| "this sender".into());
            (acct, format!("<h1>You're subscribed</h1><p>Thanks! You'll start getting emails from {}.</p>", esc(&name)))
        }
        Confirmed::Expired(a) => (account_of(a).await, "<h1>This link has expired</h1><p>Sign up again and we'll send you a new one.</p>".to_string()),
    };
    let color = account.map(|a| a.brand_color).unwrap_or_else(|| "#1f6f5c".into());
    page("Confirm your subscription", &body, &color).into_response()
}

// ---------- Browser tracking ----------

#[derive(Deserialize)]
struct KeyQuery {
    k: String,
}

async fn account_by_public_key(state: &AppState, k: &str) -> AppResult<Account> {
    sqlx::query_as("SELECT * FROM accounts WHERE public_key = $1")
        .bind(k)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::Unauthorized)
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type".parse().expect("static header"));
    r
}

async fn cors_preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

fn public_profile(state: &AppState, account_id: Uuid, v: &Value) -> AppResult<ProfileInput> {
    let mut p = v.clone();
    let token_id = v.get("$token").and_then(|t| t.as_str()).and_then(|t| profile_from_token(state, account_id, t));
    if let Some(o) = p.as_object_mut() {
        // Browsers may not change consent or pick profiles by internal id.
        for k in ["email_consent", "sms_consent", "id", "$token"] {
            o.remove(k);
        }
    }
    let mut input: ProfileInput = serde_json::from_value(p)?;
    // A valid link token identifies the person, but only when no other identifier is given:
    // a token plus a new email must not let a browser rewrite that profile.
    if let (Some(id), true) = (token_id, input.email.is_none() && input.phone_number.is_none() && input.external_id.is_none()) {
        input.id = Some(id);
    }
    Ok(input)
}

async fn client_event(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    let res: AppResult<Value> = async {
        let a = account_by_public_key(&state, &q.k).await?;
        let b: Value = serde_json::from_slice(&body)?;
        let metric = b["metric"].as_str().ok_or_else(|| AppError::bad("metric is required"))?;
        let input = public_profile(&state, a.id, &b["profile"])?;
        let p = match input.id {
            // Token-identified visitor: the profile must belong to this account.
            Some(id) => {
                let owned: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE id = $1 AND account_id = $2").bind(id).bind(a.id).fetch_optional(&state.db).await?;
                match owned {
                    Some(_) => crate::api::audience::load_profile(&state, a.id, id).await?,
                    None => return Err(AppError::bad("unknown visitor")),
                }
            }
            None => audience::upsert_profile_public(&state.db, a.id, &input).await?.profile,
        };
        let props = if b["properties"].is_object() { b["properties"].clone() } else { json!({}) };
        let value = b["value"].as_f64();
        audience::record_event(
            &state.db,
            NewEvent {
                account_id: a.id,
                profile_id: p.id,
                metric,
                properties: props,
                value,
                occurred_at: client_time(&b["time"], chrono::Utc::now()),
                unique_id: b["unique_id"].as_str().map(|u| u.chars().take(200).collect()),
                message_id: None,
            },
        )
        .await?;
        Ok(json!({"ok": true}))
    }
    .await;
    cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

/// When the browser held an event until it knew the visitor, it sends when it happened (ms since epoch).
/// Only the last 7 days count; anything else is recorded as now.
pub fn client_time(v: &Value, now: chrono::DateTime<chrono::Utc>) -> Option<chrono::DateTime<chrono::Utc>> {
    let t = chrono::DateTime::from_timestamp_millis(v.as_f64()? as i64)?;
    (t <= now && t > now - chrono::Duration::days(7)).then_some(t)
}

/// What the website script may report during a setup test.
const TEST_CHECKS: &[&str] = &[
    "loaded", "forms", "product", "cart", "checkout", "order", "identify", "pixel", "custom",
    "warn_duplicate", "warn_csp", "warn_key", "warn_network",
];

#[derive(Deserialize)]
struct TestStatusQuery {
    k: String,
    code: String,
}

/// The website script asks this before turning on test mode from a link, so a made-up code does nothing.
async fn client_test_status(State(state): State<AppState>, Query(q): Query<TestStatusQuery>) -> Response {
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT true FROM site_tests t JOIN accounts a ON a.id = t.account_id WHERE t.code = $1 AND a.public_key = $2 AND t.expires_at > now()",
    )
    .bind(&q.code)
    .bind(&q.k)
    .fetch_optional(&state.db)
    .await
    .unwrap_or_default();
    let mut r = cors(Json(json!({"active": active.unwrap_or(false)})).into_response());
    r.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().expect("static header"));
    r
}

/// Keep only the fields the dashboard shows, as plain short strings, numbers and booleans.
fn test_detail(v: &Value) -> Value {
    const TEXT: &[&str] = &["summary", "message", "platform", "tracked_by", "email", "metric", "name", "url", "image_url", "order_id", "how"];
    const NUMBER: &[&str] = &["count", "value", "price", "item_count", "version"];
    const FLAG: &[&str] = &["ok", "auto_track", "capture_emails", "identified"];
    let mut out = serde_json::Map::new();
    for k in TEXT {
        if let Some(s) = v[*k].as_str() {
            out.insert((*k).into(), json!(s.chars().take(300).collect::<String>()));
        }
    }
    for k in NUMBER {
        if let Some(n) = v[*k].as_f64().filter(|n| n.is_finite()) {
            out.insert((*k).into(), json!(n));
        }
    }
    for k in FLAG {
        if let Some(b) = v[*k].as_bool() {
            out.insert((*k).into(), json!(b));
        }
    }
    Value::Object(out)
}

/// The website script (or the Shopify checkout pixel) reporting what it found during "Test your setup".
async fn client_test(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    let res: AppResult<()> = async {
        let b: Value = serde_json::from_slice(&body)?;
        let code = b["code"].as_str().unwrap_or_default();
        let check = b["check"].as_str().unwrap_or_default();
        if !TEST_CHECKS.contains(&check) {
            return Err(AppError::bad("unknown check"));
        }
        let test: Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM site_tests WHERE code = $1 AND expires_at > now()").bind(code).fetch_optional(&state.db).await?;
        let Some(account_id) = test else { return Err(AppError::not_found("test")) };
        let key_account: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1").bind(&q.k).fetch_optional(&state.db).await?;
        let (check, mut detail) = if key_account == Some(account_id) {
            (check.to_string(), test_detail(&b["detail"]))
        } else {
            // The snippet on the site belongs to a different account (or an old key): say so instead of recording its results.
            ("wrong_key".to_string(), json!({"key": q.k.chars().take(12).collect::<String>()}))
        };
        detail["at"] = json!(chrono::Utc::now());
        let url = b["url"].as_str().and_then(|u| url::Url::parse(u).ok()).filter(|u| matches!(u.scheme(), "http" | "https")).map(|u| u.to_string().chars().take(500).collect::<String>());
        sqlx::query("UPDATE site_tests SET checks = checks || jsonb_build_object($2::text, $3::jsonb), last_seen_at = now(), last_url = COALESCE($4, last_url) WHERE code = $1")
            .bind(code)
            .bind(&check)
            .bind(&detail)
            .bind(&url)
            .execute(&state.db)
            .await?;
        if check == "loaded" {
            sqlx::query("UPDATE accounts SET onsite_last_seen_at = now(), onsite_last_url = COALESCE($2, onsite_last_url) WHERE id = $1").bind(account_id).bind(&url).execute(&state.db).await?;
        }
        Ok(())
    }
    .await;
    cors(match res {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    })
}

const ONSITE_JS: &str = include_str!("onsite.js");
/// The shared form renderer used by the website script, the hosted page and the dashboard preview.
const FORM_RENDER_JS: &str = include_str!("form_render.js");
const SHOPIFY_PIXEL_JS: &str = include_str!("shopify_pixel.js");

/// The Shopify custom pixel code for an account, ready to paste.
pub fn shopify_pixel(state: &AppState, key: &str) -> String {
    SHOPIFY_PIXEL_JS
        .replacen("__BASE__", &serde_json::to_string(state.cfg.public_url()).unwrap_or_default(), 1)
        .replacen("__KEY__", &serde_json::to_string(key).unwrap_or_default(), 1)
}

/// The renderer file as one JavaScript expression: trimmed, without its trailing semicolon.
fn renderer_expr() -> &'static str {
    let s = FORM_RENDER_JS.trim();
    s.strip_suffix(';').unwrap_or(s).trim_end()
}

/// `window.DumbMailFormRenderer` for the hosted page (and anything else that wants it on its own).
async fn form_render_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        format!("window.DumbMailFormRenderer = {};\n", renderer_expr()),
    )
        .into_response()
}

fn onsite_script(state: &AppState, key: &str, only_form: Option<Uuid>) -> Response {
    let fill = |s: &str| {
        s.replacen("__BASE__", &serde_json::to_string(state.cfg.public_url()).unwrap_or_default(), 1)
            .replacen("__KEY__", &serde_json::to_string(key).unwrap_or_default(), 1)
            .replacen("__FORM__", &only_form.map(|f| serde_json::to_string(&f.to_string()).unwrap_or_default()).unwrap_or_else(|| "null".into()), 1)
    };
    // The renderer goes in last, through a NUL marker that no JSON-encoded value can contain, so
    // neither the renderer's code nor the visitor-supplied key is ever read as a placeholder.
    let js = match ONSITE_JS.split_once("__RENDERER__") {
        Some((head, tail)) => fill(&format!("{head}\u{0}{tail}")).replacen('\u{0}', renderer_expr(), 1),
        None => fill(ONSITE_JS),
    };
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], js).into_response()
}

async fn onsite_js(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    onsite_script(&state, &q.k, None)
}

/// Live forms for the onsite script (forms scheduled outside now are left out).
async fn client_forms(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let res: AppResult<Value> = async {
        let a = account_by_public_key(&state, &q.k).await?;
        let forms: Vec<FormRow> = sqlx::query_as("SELECT * FROM forms WHERE account_id = $1 AND status = 'live' ORDER BY created_at").bind(a.id).fetch_all(&state.db).await?;
        let now = chrono::Utc::now();
        let data: Vec<Value> = forms
            .iter()
            .filter(|f| crate::forms::is_open(&f.config, now))
            .map(|f| {
                let mut c = crate::forms::public_config(&f.config);
                if c.get("brand_color").and_then(|v| v.as_str()).is_none_or(|v| v.is_empty()) {
                    c["brand_color"] = json!(a.brand_color);
                }
                json!({"id": f.id, "kind": f.kind, "config": c})
            })
            .collect();
        Ok(json!({"data": data}))
    }
    .await;
    let mut r = cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    });
    r.headers_mut().insert(header::CACHE_CONTROL, "public, max-age=60".parse().expect("static header"));
    r
}

/// The script reports where it runs, so the dashboard can show "Installed".
async fn client_ping(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    let url: String = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["url"].as_str().map(String::from)).unwrap_or_default();
    let url: String = url.chars().take(500).collect();
    let _ = sqlx::query(
        "UPDATE accounts SET onsite_last_seen_at = now(), onsite_last_url = $2
         WHERE public_key = $1 AND (onsite_last_seen_at IS NULL OR onsite_last_seen_at < now() - interval '2 minutes')",
    )
    .bind(&q.k)
    .bind(url::Url::parse(&url).ok().filter(|u| matches!(u.scheme(), "http" | "https")).map(|u| u.to_string()))
    .execute(&state.db)
    .await;
    cors(StatusCode::NO_CONTENT.into_response())
}

/// Signed token identifying a profile to the onsite script (added to tracked links as ?_pl=).
/// Bound to the account and valid for 60 days.
pub fn profile_token(state: &AppState, account_id: Uuid, profile_id: Uuid) -> String {
    let exp = chrono::Utc::now().timestamp() / 86400 + 60;
    auth::sign(&state.cfg.secret_key, &format!("p{}{}{exp:x}", profile_id.simple(), &account_id.simple().to_string()[..8]))
}

fn profile_from_token(state: &AppState, account_id: Uuid, token: &str) -> Option<Uuid> {
    let payload = auth::verify(&state.cfg.secret_key, token)?;
    let rest = payload.strip_prefix('p')?;
    if rest.len() < 41 {
        return None;
    }
    let (pid, tail) = rest.split_at(32);
    let (acct, exp) = tail.split_at(8);
    let exp = i64::from_str_radix(exp, 16).ok()?;
    if acct != &account_id.simple().to_string()[..8] || exp < chrono::Utc::now().timestamp() / 86400 {
        return None;
    }
    Uuid::parse_str(pid).ok()
}

/// Only add the identity token to links that go to the brand's own website.
fn is_own_site(account: &Account, dest: &url::Url) -> bool {
    let Some(host) = dest.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()) else { return false };
    let site = account.website.as_deref().and_then(|w| url::Url::parse(w).ok()).and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()));
    site.iter().chain(account.verified_domains.iter()).any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

// ---------- Forms ----------

async fn load_form(state: &AppState, id: Uuid) -> AppResult<(FormRow, Account)> {
    let f: FormRow = sqlx::query_as("SELECT * FROM forms WHERE id = $1").bind(id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("form"))?;
    let a = crate::campaigns::load_account(&state.db, f.account_id).await?;
    Ok((f, a))
}

#[derive(Deserialize)]
pub struct SubscribeBody {
    form_id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    #[serde(default)]
    sms_consent: bool,
    /// The A/B test version the visitor saw.
    #[serde(default)]
    variant: Option<String>,
    /// The browser's IANA time zone; fills the profile's `timezone` and `country` when they're missing.
    #[serde(default)]
    timezone: Option<String>,
    /// Answers to birthday and custom fields by field key (the standard keys may be sent here too).
    #[serde(default)]
    fields: Option<Map<String, Value>>,
    /// "phone" for the form's optional second screen that adds a phone number.
    #[serde(default)]
    step: Option<String>,
    /// From the first screen's response: lets the phone step add the number to that same person.
    #[serde(default)]
    phone_token: Option<String>,
}

impl SubscribeBody {
    /// Every submitted answer by field key.
    fn given(&self) -> Map<String, Value> {
        let mut m = self.fields.clone().unwrap_or_default();
        for (k, v) in [("email", &self.email), ("phone_number", &self.phone_number), ("first_name", &self.first_name), ("last_name", &self.last_name)] {
            if let Some(v) = v.as_ref().filter(|v| !v.trim().is_empty()) {
                m.insert(k.into(), json!(v));
            }
        }
        m
    }
}

/// Count a view or sign-up on the form's all-time totals and in its hourly stats.
async fn count_form(state: &AppState, form_id: Uuid, submit: bool) -> AppResult<()> {
    sqlx::query(
        "WITH f AS (UPDATE forms SET views = views + $2, submits = submits + $3 WHERE id = $1 RETURNING id, account_id)
         INSERT INTO form_stats_hourly (form_id, account_id, hour, views, submits)
         SELECT id, account_id, date_trunc('hour', now()), $2, $3 FROM f
         ON CONFLICT (form_id, hour) DO UPDATE SET views = form_stats_hourly.views + EXCLUDED.views, submits = form_stats_hourly.submits + EXCLUDED.submits",
    )
    .bind(form_id)
    .bind(i64::from(!submit))
    .bind(i64::from(submit))
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Count a view or sign-up for a form's A/B test version. Returns the version when the form is testing it.
async fn count_variant(state: &AppState, form: &FormRow, variant: Option<&str>, submit: bool) -> AppResult<Option<String>> {
    let Some(v) = variant.filter(|v| crate::api::messaging::form_versions(&form.config).contains(v)) else { return Ok(None) };
    sqlx::query(
        "INSERT INTO form_variant_stats (form_id, variant, views, submits) VALUES ($1, $2, $3, $4)
         ON CONFLICT (form_id, variant) DO UPDATE SET views = form_variant_stats.views + EXCLUDED.views, submits = form_variant_stats.submits + EXCLUDED.submits",
    )
    .bind(form.id)
    .bind(v)
    .bind(i64::from(!submit))
    .bind(i64::from(submit))
    .execute(&state.db)
    .await?;
    Ok(Some(v.to_string()))
}

/// A form sign-up. `ip` is the visitor's address (for the double opt-in per-visitor limit).
pub async fn subscribe(state: &AppState, account: &Account, b: &SubscribeBody, ip: Option<&str>) -> AppResult<Value> {
    let (form, _) = load_form(state, b.form_id).await?;
    if form.account_id != account.id || form.status != "live" {
        return Err(AppError::not_found("form"));
    }
    // Outside the form's start/end window sign-ups are refused, with two hours' grace after the end
    // for someone who opened the form just before it closed.
    let now = chrono::Utc::now();
    if !crate::forms::is_open(&form.config, now) && !crate::forms::is_open(&form.config, now - chrono::Duration::hours(2)) {
        return Err(AppError::Conflict("This form isn't open right now.".into()));
    }
    match b.step.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => {}
        Some("phone") => return phone_step(state, account, &form, b).await,
        Some(other) => return Err(AppError::bad(format!("step must be \"phone\" or left out (got {other:?})"))),
    }
    let defs = crate::forms::field_defs(&form.config);
    let answers = crate::forms::answers(&defs, &b.given())?;
    let email = answers.email.clone();
    let phone = answers.phone_number.clone();
    if email.is_none() && phone.is_none() {
        return Err(AppError::bad(if defs.iter().any(|d| d.key == "email") { "enter your email" } else { "enter your mobile number" }));
    }
    // A double opt-in list: the email address joins only after confirming by email.
    let double_opt_in = match (form.list_id, &email) {
        (Some(l), Some(_)) => sqlx::query_scalar::<_, bool>("SELECT double_opt_in FROM lists WHERE id = $1").bind(l).fetch_optional(&state.db).await?.unwrap_or(false),
        _ => false,
    };
    let input = ProfileInput {
        email: email.clone(),
        phone_number: phone.clone(),
        first_name: answers.first_name,
        last_name: answers.last_name,
        properties: (!answers.properties.is_empty()).then_some(answers.properties),
        email_consent: email.as_ref().filter(|_| !double_opt_in).map(|_| "subscribed".to_string()),
        sms_consent: if b.sms_consent && phone.is_some() { Some("subscribed".into()) } else { None },
        ..Default::default()
    };
    let upserted = audience::upsert_profile_public(&state.db, account.id, &input).await?;
    let p = upserted.profile;
    if let Some(tz) = &b.timezone {
        audience::fill_location_from_timezone(&state.db, p.id, tz).await?;
    }
    // Someone new on a double opt-in form gets no email at all, from any campaign or flow, until they confirm.
    if double_opt_in && upserted.created {
        sqlx::query("UPDATE profiles SET email_consent = 'pending', updated_at = now() WHERE id = $1 AND email_consent = 'never_subscribed'").bind(p.id).execute(&state.db).await?;
    }
    count_form(state, form.id, true).await?;
    let mut props = json!({"form_id": form.id, "form": form.name, "list_id": form.list_id});
    if let Some(v) = count_variant(state, &form, b.variant.as_deref(), true).await? {
        props["form_version"] = json!(v);
    }
    // Already subscribed to email (they confirmed or opted in before): no need to ask again.
    if let (true, Some(list_id), Some(addr)) = (double_opt_in && p.email_consent != "subscribed", form.list_id, p.email.as_deref()) {
        match crate::optin::start(state, account, p.id, addr, list_id, form.id, props.clone(), ip).await? {
            crate::optin::Started::Pending => {
                let msg = form.config["confirm_message"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("Almost done! Check your inbox and tap the link to confirm.");
                return Ok(json!({"ok": true, "confirm": true, "message": msg}));
            }
            // Never let someone onto a double opt-in list unconfirmed: ask them to try again.
            crate::optin::Started::Unavailable(why) => {
                tracing::warn!(account = %account.id, form = %form.id, reason = %why, "double opt-in confirmation couldn't be sent; sign-up refused");
                return Err(AppError::Conflict("We couldn't send your confirmation email right now. Please try again later.".into()));
            }
        }
    }
    audience::record_event(
        &state.db,
        NewEvent { account_id: account.id, profile_id: p.id, metric: "Subscribed to List", properties: props, value: None, occurred_at: None, unique_id: None, message_id: None },
    )
    .await?;
    if let Some(l) = form.list_id {
        audience::add_to_list(&state.db, account.id, l, &[p.id]).await?;
    }
    let mut out = json!({"ok": true, "message": form.config["success_message"].as_str().unwrap_or("Thanks for subscribing!")});
    // Only a profile this sign-up created gets a token: anyone can type an existing person's email,
    // and the token must not let them add a number to that person. Others' numbers stand alone.
    if form.config["phone_step"]["enabled"].as_bool() == Some(true) && upserted.created {
        out["phone_token"] = json!(phone_step_token(state, form.id, p.id));
    }
    // The code is only handed over here, after a real sign-up (public configs leave it out).
    let version = b.variant.as_deref().filter(|v| crate::api::messaging::form_versions(&form.config).contains(v));
    if let Some(code) = crate::forms::discount_code(&form.config, version) {
        out["discount_code"] = json!(code);
    }
    Ok(out)
}

/// A token, good for an hour, that lets this form's phone step add a number to `profile`.
fn phone_step_token(state: &AppState, form: Uuid, profile: Uuid) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
    crate::auth::sign(&state.cfg.secret_key, &format!("pstep:{}:{}:{exp:x}", form.simple(), profile.simple()))
}

/// The profile a phone-step token names, if it's genuine, for this form and not expired.
fn phone_step_profile(state: &AppState, form: Uuid, token: &str) -> Option<Uuid> {
    let payload = crate::auth::verify(&state.cfg.secret_key, token)?;
    let mut parts = payload.strip_prefix("pstep:")?.split(':');
    let (f, p, exp) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || f != form.simple().to_string() || i64::from_str_radix(exp, 16).ok()? < chrono::Utc::now().timestamp() {
        return None;
    }
    p.parse().ok()
}

/// The second screen after an email sign-up: adds a phone number and SMS consent to the same person.
/// It isn't a second sign-up: no submit count, list join, confirmation email or "Subscribed to List".
async fn phone_step(state: &AppState, account: &Account, form: &FormRow, b: &SubscribeBody) -> AppResult<Value> {
    if form.config["phone_step"]["enabled"].as_bool() != Some(true) {
        return Err(AppError::bad("this form doesn't have a phone step"));
    }
    let given = b.given();
    let text = |k: &str| given.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    let raw = text("phone_number").ok_or_else(|| AppError::bad("enter your mobile number"))?;
    let phone = normalize_phone(&raw).ok_or_else(|| AppError::bad("enter a valid mobile number"))?;
    if !b.sms_consent {
        return Err(AppError::bad("tick the box to agree to texts"));
    }
    // The number joins the person who just signed up only with the token from that sign-up (an email
    // alone proves nothing), only when they have no number yet, and only when nobody else has this one.
    // Otherwise the number and its consent stand alone, so they can't land on someone else's profile.
    let owner = b.phone_token.as_deref().and_then(|t| phone_step_profile(state, form.id, t));
    let by_phone = audience::find_profile(&state.db, account.id, &ProfileInput { phone_number: Some(phone.clone()), ..Default::default() }).await?;
    let target = match owner {
        Some(id) if by_phone.is_none() => audience::find_profile(&state.db, account.id, &ProfileInput { id: Some(id), ..Default::default() }).await?.filter(|p| p.id == id && p.phone_number.is_none()),
        _ => None,
    };
    let input = ProfileInput { id: target.map(|p| p.id), phone_number: Some(phone), sms_consent: Some("subscribed".into()), ..Default::default() };
    audience::upsert_profile_public(&state.db, account.id, &input).await?;
    Ok(json!({"ok": true, "message": form.config["success_message"].as_str().unwrap_or("Thanks for subscribing!")}))
}

async fn client_subscribe(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let is_form = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    let res: AppResult<Value> = async {
        let a = account_by_public_key(&state, &q.k).await?;
        let b: SubscribeBody = if is_form {
            let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
            let one = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            // Everything else is a field answer; a key sent more than once (ticked checkboxes) becomes a list.
            let mut fields = Map::new();
            for (k, v) in &pairs {
                if matches!(k.as_str(), "form_id" | "email" | "phone_number" | "first_name" | "last_name" | "sms_consent" | "variant" | "step" | "phone_token" | "timezone") {
                    continue;
                }
                match fields.get_mut(k) {
                    Some(Value::Array(a)) => a.push(json!(v)),
                    Some(prev) => *prev = json!([prev.take(), v]),
                    None => {
                        fields.insert(k.clone(), json!(v));
                    }
                }
            }
            SubscribeBody {
                form_id: one("form_id").and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("missing form"))?,
                email: one("email"),
                phone_number: one("phone_number"),
                first_name: one("first_name"),
                last_name: one("last_name"),
                sms_consent: one("sms_consent").is_some(),
                variant: one("variant"),
                fields: Some(fields),
                step: one("step"),
                phone_token: one("phone_token"),
                timezone: one("timezone"),
            }
        } else {
            serde_json::from_slice(&body)?
        };
        subscribe(&state, &a, &b, client_ip(&headers).as_deref()).await
    }
    .await;
    if is_form {
        // Hosted page fallback without JavaScript.
        return match res {
            Ok(v) => {
                let code = v["discount_code"].as_str().map(|c| format!("<p>Your code: <strong>{}</strong></p>", esc(c))).unwrap_or_default();
                page("Thanks", &format!("<h1>{}</h1>{code}", esc(v["message"].as_str().unwrap_or("Thanks!"))), "#1f6f5c").into_response()
            }
            Err(e) => page("Oops", &format!("<h1>Something's off</h1><p>{}</p>", esc(&e.public_message())), "#1f6f5c").into_response(),
        };
    }
    cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

#[derive(Deserialize)]
struct ViewQuery {
    /// The A/B test version shown.
    v: Option<String>,
}

async fn form_view(State(state): State<AppState>, Path(id): Path<Uuid>, Query(q): Query<ViewQuery>) -> Response {
    if let Ok((f, _)) = load_form(&state, id).await {
        if f.status == "live" {
            let _ = count_form(&state, id, false).await;
            let _ = count_variant(&state, &f, q.v.as_deref(), false).await;
        }
    }
    cors(StatusCode::NO_CONTENT.into_response())
}

const SMS_CONSENT: &str = "By checking this box you agree to receive recurring automated marketing texts at this number. Consent is not a condition of purchase. Msg & data rates may apply. Reply STOP to opt out.";

/// The plain HTML fields for the hosted page without JavaScript.
fn form_fields_html(config: &Value) -> String {
    let mut h = String::new();
    for d in crate::forms::field_defs(config) {
        let key = esc(&d.key);
        let req = if d.required { " required" } else { "" };
        let label = d.label.as_deref().filter(|l| !l.trim().is_empty());
        if let Some(l) = label {
            h.push_str(&format!(r#"<label class="l">{}</label>"#, esc(l)));
        }
        let ph = |default: &str| esc(d.placeholder.as_deref().or(if label.is_some() { None } else { Some(default) }).unwrap_or(""));
        let name = label.map(String::from).unwrap_or_else(|| d.key.replace('_', " "));
        match (d.key.as_str(), d.kind.as_str()) {
            ("email", _) => h.push_str(&format!(r#"<input type="email" name="email" placeholder="{}" autocomplete="email"{req}>"#, ph("Email address"))),
            ("phone_number", _) => {
                let consent = config["sms_consent_text"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or(SMS_CONSENT);
                h.push_str(&format!(
                    r#"<input type="tel" name="phone_number" placeholder="{}" autocomplete="tel"{req}><label style="display:flex;gap:8px;align-items:flex-start;margin:0 0 12px"><input type="checkbox" name="sms_consent" value="1" style="margin-top:3px"><span>{}</span></label>"#,
                    ph("Mobile number"),
                    esc(consent)
                ))
            }
            ("first_name", _) => h.push_str(&format!(r#"<input type="text" name="first_name" placeholder="{}" autocomplete="given-name"{req}>"#, ph("First name"))),
            ("last_name", _) => h.push_str(&format!(r#"<input type="text" name="last_name" placeholder="{}" autocomplete="family-name"{req}>"#, ph("Last name"))),
            (_, "date") => h.push_str(&format!(r#"<input type="date" name="{key}" aria-label="{}"{req}>"#, esc(&name))),
            (_, "number") => h.push_str(&format!(r#"<input type="number" step="any" name="{key}" placeholder="{}"{req}>"#, ph(&name))),
            (_, "select") => {
                h.push_str(&format!(r#"<select name="{key}"{req}><option value="">{}</option>"#, ph("Choose one")));
                for o in &d.options {
                    h.push_str(&format!(r#"<option>{}</option>"#, esc(o)));
                }
                h.push_str("</select>");
            }
            (_, "radio" | "checkboxes") => {
                if label.is_none() {
                    h.push_str(&format!(r#"<label class="l">{}</label>"#, esc(&name)));
                }
                let t = if d.kind == "radio" { "radio" } else { "checkbox" };
                for o in &d.options {
                    h.push_str(&format!(r#"<label class="o"><input type="{t}" name="{key}" value="{v}"> {v}</label>"#, v = esc(o)));
                }
            }
            (_, "checkbox") => {
                h.push_str(&format!(r#"<label class="o"><input type="checkbox" name="{key}" value="1"{req}> {}</label>"#, esc(&name)));
            }
            _ => h.push_str(&format!(r#"<input type="text" name="{key}" maxlength="{}" placeholder="{}"{req}>"#, crate::forms::MAX_ANSWER, ph(&name))),
        }
    }
    h
}

#[derive(Deserialize)]
struct FormPageQuery {
    /// The token from the form's `preview_url`.
    preview: Option<String>,
    /// In a preview, the A/B test version to show (default a).
    v: Option<String>,
}

/// The hosted sign-up page. The shared renderer draws it; the noscript copy keeps sign-ups working
/// without JavaScript. With a valid preview token it also shows drafts and counts and saves nothing.
async fn form_page(State(state): State<AppState>, Path(id): Path<Uuid>, Query(q): Query<FormPageQuery>) -> Response {
    let Ok((f, a)) = load_form(&state, id).await else { return (StatusCode::NOT_FOUND, "form not found").into_response() };
    let preview = q.preview.as_deref().is_some_and(|t| crate::forms::preview_ok(&state.cfg.secret_key, f.id, t));
    let color = f.config["brand_color"].as_str().filter(|c| !c.is_empty()).unwrap_or(&a.brand_color).to_string();
    if !preview {
        if f.status != "live" {
            return (StatusCode::NOT_FOUND, "this form isn't published yet").into_response();
        }
        if !crate::forms::is_open(&f.config, chrono::Utc::now()) {
            return page(&a.name, "<h1>This form isn't open right now</h1><p>Please check back later.</p>", &color).into_response();
        }
    }
    let versions = crate::api::messaging::form_versions(&f.config);
    let version = if versions.is_empty() {
        None
    } else if preview {
        Some(q.v.filter(|v| versions.contains(&v.as_str())).unwrap_or_else(|| "a".into()))
    } else {
        // A/B test: each page load shows a random version.
        Some(versions[rand::Rng::gen_range(&mut rand::thread_rng(), 0..versions.len())].to_string())
    };
    if !preview {
        let _ = count_form(&state, id, false).await;
        let _ = count_variant(&state, &f, version.as_deref(), false).await;
    }
    let mut config = version.as_deref().map(|v| crate::api::messaging::form_version_config(&f.config, v)).unwrap_or_else(|| f.config.clone());
    // A preview shows the discount code so it can be checked; visitors only get it by signing up.
    if !preview {
        config = crate::forms::public_config(&config);
    }
    if config["brand_color"].as_str().is_none_or(|c| c.is_empty()) {
        config["brand_color"] = json!(a.brand_color);
    }
    let base = state.cfg.public_url();
    let submit_url = format!("{base}/client/subscribe?k={}", a.public_key);
    let variant_input = version.as_deref().map(|v| format!(r#"<input type="hidden" name="variant" value="{}">"#, esc(v))).unwrap_or_default();
    let img = config["image_url"].as_str().filter(|s| !s.is_empty()).map(|u| format!(r#"<img src="{}" alt="{}">"#, esc(u), esc(config["image_alt"].as_str().unwrap_or("")))).unwrap_or_default();
    let (action, note, disabled) = if preview {
        (String::new(), r#"<p class="n">Preview: sign-ups here aren't saved.</p>"#, " disabled")
    } else {
        (format!(r#" method="post" action="{}""#, esc(&submit_url)), "", "")
    };
    let static_form = format!(
        r#"{img}<h1>{h}</h1><p>{b}</p>{note}<form{action}><input type="hidden" name="form_id" value="{id}">{variant_input}{fields}<input type="submit" value="{btn}"{disabled}></form>"#,
        h = esc(config["headline"].as_str().unwrap_or("")),
        b = esc(config["body"].as_str().unwrap_or("")),
        fields = form_fields_html(&config),
        btn = esc(config["button_text"].as_str().unwrap_or("Sign up")),
    );
    let c = crate::render::safe_color(&color, "#1f6f5c");
    let noscript_css = format!(
        ".dm-ns{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#f4f4f5;color:#111827;display:flex;min-height:100vh;align-items:center;justify-content:center}}\
.dm-ns .c{{background:#fff;border-radius:12px;padding:32px;max-width:420px;width:calc(100% - 32px);box-shadow:0 1px 3px rgba(0,0,0,.08);box-sizing:border-box}}.dm-ns img{{width:100%;border-radius:8px;margin-bottom:16px}}\
.dm-ns h1{{font-size:22px;margin:0 0 12px}}.dm-ns p{{color:#4b5563;line-height:1.5;white-space:pre-line}}.dm-ns .n{{font-size:13px;font-style:italic}}\
.dm-ns input[type=submit]{{background:{c};color:#fff;border:0;border-radius:8px;padding:12px 20px;font-size:16px;font-weight:600;cursor:pointer;width:100%}}\
.dm-ns input[type=email],.dm-ns input[type=tel],.dm-ns input[type=text],.dm-ns input[type=number],.dm-ns input[type=date],.dm-ns select{{width:100%;box-sizing:border-box;padding:12px;border:1px solid #d1d5db;border-radius:8px;font-size:16px;margin:0 0 10px;background:#fff}}\
.dm-ns label{{font-size:13px;color:#4b5563}}.dm-ns .l{{display:block;margin:0 0 4px}}.dm-ns .o{{display:block;margin:0 0 8px}}"
    );
    let data = json!({
        "form": {"id": f.id, "kind": "page", "config": config, "version": version},
        "submit": if preview { Value::Null } else { json!(submit_url) },
    });
    // JSON inside a script element: escaping "<" means nothing in it can close the element.
    let data = data.to_string().replace('<', "\\u003c");
    let robots = if preview { r#"<meta name="robots" content="noindex">"# } else { "" };
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title>{robots}<style>body{{margin:0}}</style></head><body>
<div id="dm-form"></div>
<noscript><style>{noscript_css}</style><div class="dm-ns"><div class="c">{static_form}</div></div></noscript>
<script type="application/json" id="dm-form-data">{data}</script>
<script src="{base}/client/form-render.js"></script>
<script>{MOUNT_JS}</script>
</body></html>"#,
        title = esc(&a.name),
    ))
    .into_response()
}

/// Mounts the renderer on the hosted page and posts sign-ups as JSON. Without the renderer (blocked
/// or failed to load) it shows the plain form from the noscript copy instead.
const MOUNT_JS: &str = r#"(function () {
  var d = JSON.parse(document.getElementById("dm-form-data").textContent);
  var el = document.getElementById("dm-form"), R = window.DumbMailFormRenderer;
  if (!R || typeof R.mount !== "function") { var ns = document.querySelector("noscript"); if (ns) el.innerHTML = ns.textContent; return; }
  var opts = { container: el };
  if (d.submit) opts.submit = function (data) {
    var body = {}; for (var k in data) if (Object.prototype.hasOwnProperty.call(data, k)) body[k] = data[k];
    body.form_id = d.form.id; if (d.form.version && !body.variant) body.variant = d.form.version;
    return fetch(d.submit, { method: "POST", credentials: "omit", headers: { "content-type": "application/json" }, body: JSON.stringify(body) })
      .then(function (r) { return r.json().catch(function () { return { error: { message: "Something went wrong. Please try again." } }; }); });
  };
  R.mount(d.form, opts);
})();"#;

async fn form_embed(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let Ok((f, a)) = load_form(&state, id).await else { return (StatusCode::NOT_FOUND, "// form not found").into_response() };
    if f.status != "live" {
        return ([(header::CONTENT_TYPE, "application/javascript")], "/* form not published */").into_response();
    }
    onsite_script(&state, &a.public_key, Some(f.id))
}

/// A public uploaded image. Served with the type it was sniffed as and locked down so it can only ever be an image.
async fn image(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "image not found").into_response();
    let Ok(id) = Uuid::parse_str(&id) else { return not_found() };
    let row: Option<(String, Vec<u8>)> = sqlx::query_as("SELECT content_type, bytes FROM images WHERE id = $1").bind(id).fetch_optional(&state.db).await.unwrap_or_default();
    let Some((content_type, bytes)) = row else { return not_found() };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'".to_string()),
        ],
        bytes,
    )
        .into_response()
}

// ---------- Provider webhooks ----------

fn check_secret(state: &AppState, s: &str) -> AppResult<()> {
    let a = state.cfg.webhook_secret.as_bytes();
    let b = s.as_bytes();
    if a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 {
        Ok(())
    } else {
        Err(AppError::NotFound("not found".into()))
    }
}

/// Inbound SMS webhooks: the platform secret covers every account; a customer's own token
/// (for bring-your-own providers) only covers that customer's account.
async fn webhook_scope(state: &AppState, s: &str) -> AppResult<Option<Uuid>> {
    if check_secret(state, s).is_ok() {
        return Ok(None);
    }
    let acct: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE webhook_token = $1").bind(s).fetch_optional(&state.db).await?;
    acct.map(Some).ok_or_else(|| AppError::NotFound("not found".into()))
}

async fn msg_by_provider_id(state: &AppState, pid: &str) -> AppResult<Option<Msg>> {
    Ok(sqlx::query_as("SELECT id, account_id, profile_id, channel, campaign_id, flow_id, is_test FROM messages WHERE provider_message_id = $1")
        .bind(pid)
        .fetch_optional(&state.db)
        .await?)
}

/// Amazon SES events delivered through an SNS HTTPS subscription.
async fn ses_webhook(State(state): State<AppState>, Path(secret): Path<String>, body: Bytes) -> AppResult<StatusCode> {
    check_secret(&state, &secret)?;
    let env: Value = serde_json::from_slice(&body)?;
    match env["Type"].as_str() {
        Some("SubscriptionConfirmation") => {
            let url = env["SubscribeURL"].as_str().unwrap_or_default();
            let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
            if url.starts_with("https://") && host.starts_with("sns.") && host.ends_with(".amazonaws.com") {
                state.http.get(url).send().await.map_err(|e| AppError::Internal(e.into()))?;
            }
            return Ok(StatusCode::OK);
        }
        Some("Notification") => {}
        _ => return Ok(StatusCode::OK),
    }
    let msg: Value = env["Message"].as_str().map(serde_json::from_str).transpose()?.unwrap_or(Value::Null);
    let kind = msg["eventType"].as_str().or(msg["notificationType"].as_str()).unwrap_or("");
    let Some(pid) = msg["mail"]["messageId"].as_str() else { return Ok(StatusCode::OK) };
    // Hard bounces and complaints count toward the automatic sending pause through the send record,
    // which outlives the message (deleting a profile deletes its messages). Addresses already on the
    // SES account suppression list don't count toward AWS's bounce rate.
    let strike = match kind {
        "Bounce" => (msg["bounce"]["bounceType"].as_str() == Some("Permanent") && msg["bounce"]["bounceSubType"].as_str() != Some("OnAccountSuppressionList")).then_some(false),
        "Complaint" => Some(true),
        _ => None,
    };
    if let Some(complaint) = strike {
        if let Some((account_id, campaign_id)) = crate::deliverability::record_strike(&state.db, pid, complaint).await? {
            crate::deliverability::note_outcome(&state, account_id, campaign_id).await;
        }
    }
    let Some(m) = msg_by_provider_id(&state, pid).await? else { return Ok(StatusCode::OK) };
    match kind {
        "Delivery" => {
            sqlx::query("UPDATE messages SET status = CASE WHEN status = 'sent' THEN 'delivered' ELSE status END, delivered_at = now() WHERE id = $1").bind(m.id).execute(&state.db).await?;
        }
        "Bounce" => {
            let permanent = msg["bounce"]["bounceType"].as_str() == Some("Permanent");
            sqlx::query("UPDATE messages SET status = 'bounced', error = $2, bounce_type = $3 WHERE id = $1")
                .bind(m.id)
                .bind(msg["bounce"]["bounceSubType"].as_str())
                .bind(msg["bounce"]["bounceType"].as_str())
                .execute(&state.db)
                .await?;
            if permanent {
                sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce', updated_at = now() WHERE id = $1").bind(m.profile_id).execute(&state.db).await?;
            }
            msg_event(&state, &m, "Bounced Email", json!({"bounce_type": msg["bounce"]["bounceType"]}), Some(format!("bounce:{}", m.id))).await?;
        }
        "Complaint" => {
            sqlx::query("UPDATE messages SET status = 'complained' WHERE id = $1").bind(m.id).execute(&state.db).await?;
            sqlx::query("UPDATE profiles SET email_suppressed_reason = 'complaint', email_consent = 'unsubscribed', updated_at = now() WHERE id = $1").bind(m.profile_id).execute(&state.db).await?;
            msg_event(&state, &m, "Marked Email as Spam", json!({}), Some(format!("complaint:{}", m.id))).await?;
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

const STOP_WORDS: &[&str] = &["stop", "stopall", "unsubscribe", "cancel", "end", "quit", "optout", "revoke"];
const START_WORDS: &[&str] = &["start", "unstop", "yes", "subscribe"];

/// Inbound text: STOP/START keywords update SMS consent for the matching profile.
async fn inbound_sms(state: &AppState, scope: Option<Uuid>, from: &str, to: &str, text: &str) -> AppResult<()> {
    let (Some(from), Some(to)) = (normalize_phone(from), normalize_phone(to)) else { return Ok(()) };
    let accounts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT a.id FROM accounts a LEFT JOIN phone_numbers n ON n.account_id = a.id AND n.status <> 'released'
         WHERE (a.sms_from = $1 OR n.phone_number = $1) AND ($2::uuid IS NULL OR a.id = $2)",
    )
    .bind(&to)
    .bind(scope)
    .fetch_all(&state.db)
    .await?;
    let word: String = text.trim().to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    for account_id in accounts {
        let pid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(&from).fetch_optional(&state.db).await?;
        let Some(pid) = pid else { continue };
        let (consent, metric) = if STOP_WORDS.contains(&word.as_str()) {
            ("unsubscribed", "Unsubscribed from SMS")
        } else if START_WORDS.contains(&word.as_str()) {
            ("subscribed", "Subscribed to SMS")
        } else {
            audience::record_event(&state.db, NewEvent { account_id, profile_id: pid, metric: "Received SMS Reply", properties: json!({"text": text}), value: None, occurred_at: None, unique_id: None, message_id: None }).await?;
            continue;
        };
        sqlx::query("UPDATE profiles SET sms_consent = $2, updated_at = now() WHERE id = $1").bind(pid).bind(consent).execute(&state.db).await?;
        audience::record_event(&state.db, NewEvent { account_id, profile_id: pid, metric, properties: json!({"keyword": word}), value: None, occurred_at: None, unique_id: None, message_id: None }).await?;
    }
    Ok(())
}

async fn sms_status(state: &AppState, scope: Option<Uuid>, provider_id: &str, delivered: bool) -> AppResult<()> {
    let status = if delivered { "delivered" } else { "failed" };
    sqlx::query("UPDATE messages SET status = $2, delivered_at = CASE WHEN $2 = 'delivered' THEN now() ELSE delivered_at END WHERE provider_message_id = $1 AND channel = 'sms' AND ($3::uuid IS NULL OR account_id = $3)")
        .bind(provider_id)
        .bind(status)
        .bind(scope)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn telnyx_webhook(State(state): State<AppState>, Path(secret): Path<String>, Json(v): Json<Value>) -> AppResult<StatusCode> {
    let scope = webhook_scope(&state, &secret).await?;
    let p = &v["data"]["payload"];
    match v["data"]["event_type"].as_str().unwrap_or("") {
        "message.received" => {
            let to = p["to"][0]["phone_number"].as_str().unwrap_or_default();
            inbound_sms(&state, scope, p["from"]["phone_number"].as_str().unwrap_or_default(), to, p["text"].as_str().unwrap_or_default()).await?;
        }
        "message.finalized" => {
            let st = p["to"][0]["status"].as_str().unwrap_or_default();
            if let Some(id) = p["id"].as_str() {
                sms_status(&state, scope, id, st == "delivered").await?;
            }
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

async fn twilio_webhook(State(state): State<AppState>, Path(secret): Path<String>, body: Bytes) -> AppResult<Response> {
    let scope = webhook_scope(&state, &secret).await?;
    let f: std::collections::HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    if let (Some(from), Some(to), Some(text)) = (f.get("From"), f.get("To"), f.get("Body")) {
        inbound_sms(&state, scope, from, to, text).await?;
    } else if let (Some(sid), Some(st)) = (f.get("MessageSid"), f.get("MessageStatus")) {
        match st.as_str() {
            "delivered" => sms_status(&state, scope, sid, true).await?,
            "failed" | "undelivered" => sms_status(&state, scope, sid, false).await?,
            _ => {}
        }
    }
    Ok(([(header::CONTENT_TYPE, "text/xml")], "<Response></Response>").into_response())
}

async fn stripe_webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> AppResult<StatusCode> {
    let Some(secret) = state.cfg.stripe_webhook_secret.as_deref() else { return Err(AppError::NotFound("not found".into())) };
    let sig = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !crate::billing::verify_stripe_signature(secret, sig, &body, chrono::Utc::now().timestamp()) {
        return Err(AppError::BadRequest("bad signature".into()));
    }
    let ev: Value = serde_json::from_slice(&body)?;
    crate::billing::handle_stripe_event(&state, &ev).await?;
    Ok(StatusCode::OK)
}

/// Operator endpoints need `Authorization: Bearer ADMIN_TOKEN`; they don't exist when it's unset or short.
fn check_admin(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    let want = state.cfg.admin_token.as_deref().filter(|t| t.len() >= 32).ok_or_else(|| AppError::NotFound("not found".into()))?;
    let got = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if got.len() != want.len() || got.bytes().zip(want.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

/// The account (id, name) whose earliest member has this email.
async fn account_by_owner_email(state: &AppState, b: &Value) -> AppResult<(Uuid, String)> {
    let email = b["email"].as_str().unwrap_or("").trim().to_lowercase();
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT a.id, a.name FROM accounts a JOIN memberships m ON m.account_id = a.id JOIN users u ON u.id = m.user_id WHERE u.email = $1 ORDER BY m.created_at LIMIT 1",
    )
    .bind(&email)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| AppError::not_found("account"))
}

/// Operator: credit (or debit) an account by its owner's email. Bearer ADMIN_TOKEN.
async fn admin_credit(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    check_admin(&state, &headers)?;
    let amount = b["amount"].as_f64().ok_or_else(|| AppError::bad("amount (USD) is required"))?;
    if amount.abs() > 10_000.0 {
        return Err(AppError::bad("amount too large"));
    }
    let (id, name) = account_by_owner_email(&state, &b).await?;
    let note = b["note"].as_str().unwrap_or("Credit added by DumbMail");
    crate::billing::post_entry(&state.db, id, "adjustment", crate::billing::to_micros(amount), note, &Uuid::now_v7().to_string()).await?;
    let bal = crate::billing::balance(&state.db, id).await?;
    tracing::info!(account = %id, amount, "admin credit");
    Ok(Json(json!({"account": name, "balance": crate::billing::to_usd(bal)})))
}

/// Operator: lift an automatic email sending pause, by account id or the owner's email. Also restarts the window
/// the bounce and complaint rates are measured over, so bounces from before don't pause it again.
async fn admin_unpause(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    check_admin(&state, &headers)?;
    let (id, name) = match b["account_id"].as_str() {
        Some(raw) => {
            let id: Uuid = raw.trim().parse().map_err(|_| AppError::bad("account_id must be a UUID"))?;
            let name: Option<String> = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
            (id, name.ok_or_else(|| AppError::not_found("account"))?)
        }
        None => account_by_owner_email(&state, &b).await?,
    };
    let was_paused = crate::deliverability::unpause(&state.db, id).await?;
    tracing::warn!(account = %id, was_paused, "admin lifted email sending pause");
    Ok(Json(json!({"account": name, "was_paused": was_paused, "sending_paused": false})))
}

/// Operator: give an account a fixed 24-hour email limit (`daily_limit`, a number) in place of the automatic
/// warm-up, or return it to the warm-up (`daily_limit: null`). By account id or the owner's email.
async fn admin_sending_limit(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    check_admin(&state, &headers)?;
    let (id, name) = match b["account_id"].as_str() {
        Some(raw) => {
            let id: Uuid = raw.trim().parse().map_err(|_| AppError::bad("account_id must be a UUID"))?;
            let name: Option<String> = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
            (id, name.ok_or_else(|| AppError::not_found("account"))?)
        }
        None => account_by_owner_email(&state, &b).await?,
    };
    let fixed: Option<i32> = match &b["daily_limit"] {
        Value::Null => None,
        v => Some(v.as_i64().filter(|n| (0..=10_000_000).contains(n)).ok_or_else(|| AppError::bad("daily_limit must be a whole number from 0 to 10,000,000, or null for the automatic warm-up"))? as i32),
    };
    sqlx::query("UPDATE accounts SET email_daily_limit_override = $2 WHERE id = $1").bind(id).bind(fixed).execute(&state.db).await?;
    crate::deliverability::forget_daily_limit(&state, id);
    let l = crate::deliverability::daily_limit(&state, id).await?;
    tracing::warn!(account = %id, daily_limit = ?fixed, "admin set the email sending limit");
    Ok(Json(json!({"account": name, "daily_limit": l.limit, "fixed": l.fixed, "sent_last_24h": l.sent_24h})))
}

#[cfg(test)]
mod tests {
    use super::client_time;
    use serde_json::json;

    #[test]
    fn test_details_keep_only_plain_known_fields() {
        let d = super::test_detail(&json!({"summary": {"bad": true}, "message": "x".repeat(400), "count": 2, "ok": "yes", "other": 1, "value": 18.5}));
        assert_eq!(d, json!({"message": "x".repeat(300), "count": 2.0, "value": 18.5}));
    }

    #[test]
    fn client_time_accepts_only_the_last_week() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let ms = |d: chrono::Duration| json!((now - d).timestamp_millis());
        assert_eq!(client_time(&ms(chrono::Duration::hours(5)), now), Some(now - chrono::Duration::hours(5)));
        assert_eq!(client_time(&ms(chrono::Duration::days(8)), now), None, "too old");
        assert_eq!(client_time(&ms(-chrono::Duration::minutes(1)), now), None, "in the future");
        assert_eq!(client_time(&json!("yesterday"), now), None);
        assert_eq!(client_time(&json!(null), now), None);
    }
}

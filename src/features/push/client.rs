//! Public push endpoints (no sign-in; the site's public key at most): the service worker file, the
//! snippet's push add-on, browser and app device registration, delivery and open reports, and the
//! tracked link a tap opens.
//!
//! Nothing here trusts a profile identifier from a browser or app on its own. A device is linked to
//! a person only with a signed identity: the link token the snippet keeps from a tracked link
//! (`$token`), or a `user_hash` the customer's own server computed with the account's identity
//! secret. Otherwise it's kept unlinked until one arrives. Delivery and open reports carry a token
//! DumbMail signed for that one delivery, so they can't be forged for another.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::crypto;
use super::settings::{self, Row};
use crate::audience::{record_event, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

/// Most devices one person keeps linked; registering another turns off their least recent.
pub const MAX_DEVICES_PER_PROFILE: i64 = 10;
/// Registrations per client address per hour, per account.
pub const MAX_REGISTRATIONS_PER_HOUR: i32 = 30;
/// Unlinked devices an account keeps at most.
pub const MAX_UNLINKED: i64 = 200_000;
const MAX_BODY: usize = 8 * 1024;

const SW_JS: &str = include_str!("sw.js");
const PUSH_JS: &str = include_str!("push.js");

#[derive(Deserialize)]
pub struct KeyQuery {
    k: String,
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type".parse().expect("static header"));
    r
}

pub async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

fn answer(res: AppResult<Value>) -> Response {
    cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

async fn account_by_key(state: &AppState, k: &str) -> AppResult<Account> {
    if k.len() > 100 {
        return Err(AppError::Unauthorized);
    }
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1").bind(k).fetch_optional(&state.db).await?;
    crate::campaigns::load_account(&state.db, id.ok_or(AppError::Unauthorized)?).await
}

fn js(body: String, extra: Option<(&'static str, &'static str)>) -> Response {
    let mut r = ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], body).into_response();
    if let Some((k, v)) = extra {
        r.headers_mut().insert(k, v.parse().expect("static header"));
    }
    r
}

fn fill(src: &str, base: &str, key: &str) -> String {
    src.replacen("__BASE__", &serde_json::to_string(base).unwrap_or_default(), 1).replacen("__KEY__", &serde_json::to_string(key).unwrap_or_default(), 1)
}

/// `GET /push/sw.js?k=`: the service worker for the customer to host at their site's root.
pub async fn service_worker(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let key: String = q.k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(100).collect();
    let mut r = js(fill(SW_JS, state.cfg.public_url(), &key), Some(("service-worker-allowed", "/")));
    r.headers_mut().insert(header::CONTENT_DISPOSITION, "inline; filename=\"dumbmail-sw.js\"".parse().expect("static header"));
    r
}

/// `GET /client/push.js?k=`: the snippet's push add-on with this account's settings, or a no-op
/// when web push is off.
pub async fn push_js(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let config = async {
        let a = account_by_key(&state, &q.k).await.ok()?;
        let row = settings::load(&state.db, a.id).await.ok()??;
        if !row.web_ready() {
            return None;
        }
        Some(json!({"key": row.vapid_public_key, "sw": row.sw_path, "prompt": row.prompt(), "color": crate::render::safe_color(&a.brand_color, "#1f6f5c")}))
    }
    .await;
    let src = fill(PUSH_JS, state.cfg.public_url(), &q.k).replacen("__CONFIG__", &config.map(|c| c.to_string()).unwrap_or_else(|| "null".into()), 1);
    js(src, None)
}

/// One more registration from this client address this hour, if under the cap (one write, so two
/// at once can't both pass the last slot).
async fn take_registration(state: &AppState, account_id: Uuid, headers: &HeaderMap) -> AppResult<()> {
    let ip = crate::public::client_ip(headers).unwrap_or_default();
    let bucket = crypto::hmac_hex(&state.cfg.secret_key, &format!("push-ip:{ip}"))[..24].to_string();
    let ok: Option<i32> = sqlx::query_scalar(
        "INSERT INTO push_registration_counts (account_id, bucket, hour, n) VALUES ($1, $2, date_trunc('hour', now()), 1)
         ON CONFLICT (account_id, bucket, hour) DO UPDATE SET n = push_registration_counts.n + 1 WHERE push_registration_counts.n < $3
         RETURNING n",
    )
    .bind(account_id)
    .bind(bucket)
    .bind(MAX_REGISTRATIONS_PER_HOUR)
    .fetch_optional(&state.db)
    .await?;
    if ok.is_none() {
        return Err(AppError::Busy("too many push registrations from here; try again later".into()));
    }
    Ok(())
}

/// The person a signed identity names (see the module docs). Errors when a user_hash is given and
/// doesn't verify; None when there's no identity.
pub async fn resolve_identity(state: &AppState, account_id: Uuid, row: &Row, v: &Value) -> AppResult<Option<Uuid>> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 320);
    if let Some(hash) = text("user_hash") {
        let secret = settings::identity_secret(state, row).ok_or_else(|| AppError::bad("identity verification isn't set up for this account"))?;
        let (input, message) = match (text("external_id"), text("email")) {
            (Some(x), _) => (ProfileInput { external_id: Some(x.to_string()), ..Default::default() }, x.to_string()),
            (None, Some(e)) => {
                let e = crate::models::normalize_email(e).ok_or_else(|| AppError::bad("that email isn't valid"))?;
                (ProfileInput { email: Some(e.clone()), ..Default::default() }, e)
            }
            (None, None) => return Err(AppError::bad("a user_hash needs the external_id or email it was made from")),
        };
        if !crypto::hmac_hex_matches(&secret, &message, hash) {
            return Err(AppError::Unauthorized);
        }
        // A verified identity may create the profile; it never changes consent.
        let p = match crate::audience::find_profile(&state.db, account_id, &input).await? {
            Some(p) => p,
            None => crate::audience::upsert_profile_public(&state.db, account_id, &input).await?.profile,
        };
        return Ok(Some(p.id));
    }
    if let Some(t) = text("$token") {
        if let Some(id) = crate::public::profile_from_token(state, account_id, t) {
            let owned: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(&state.db).await?;
            return Ok(owned);
        }
    }
    Ok(None)
}

/// Link a device, or keep it unlinked. A device already linked keeps its person unless a new
/// signed identity names someone else. Over the per-person cap, their least recent devices are
/// turned off in the same transaction.
#[allow(clippy::too_many_arguments)]
async fn save_device(state: &AppState, account_id: Uuid, kind: &str, address: &str, keys: Option<(&str, &str)>, profile_id: Option<Uuid>, user_agent: Option<&str>, app_version: Option<&str>) -> AppResult<(Uuid, bool)> {
    let mut tx = state.db.begin().await?;
    if profile_id.is_none() {
        let unlinked: i64 = sqlx::query_scalar("SELECT count(*) FROM push_devices WHERE account_id = $1 AND profile_id IS NULL AND test_user_id IS NULL").bind(account_id).fetch_one(&mut *tx).await?;
        if unlinked >= MAX_UNLINKED {
            return Err(AppError::Busy("this account has too many unlinked push devices; they are removed after 180 days unused".into()));
        }
    }
    let (id, linked): (Uuid, Option<Uuid>) = sqlx::query_as(
        "INSERT INTO push_devices (id, account_id, profile_id, kind, address, p256dh, auth_secret, user_agent, app_version)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (account_id, kind, address) DO UPDATE SET
            profile_id = COALESCE(EXCLUDED.profile_id, push_devices.profile_id),
            p256dh = COALESCE(EXCLUDED.p256dh, push_devices.p256dh), auth_secret = COALESCE(EXCLUDED.auth_secret, push_devices.auth_secret),
            user_agent = COALESCE(EXCLUDED.user_agent, push_devices.user_agent), app_version = COALESCE(EXCLUDED.app_version, push_devices.app_version),
            status = 'active', failures = 0, disabled_at = NULL, last_seen_at = now()
         RETURNING id, profile_id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(profile_id)
    .bind(kind)
    .bind(address)
    .bind(keys.map(|k| k.0))
    .bind(keys.map(|k| k.1))
    .bind(user_agent.map(|u| u.chars().take(300).collect::<String>()))
    .bind(app_version.map(|u| u.chars().take(40).collect::<String>()))
    .fetch_one(&mut *tx)
    .await?;
    if let Some(pid) = linked {
        sqlx::query(
            "UPDATE push_devices SET status = 'disabled', disabled_at = now()
             WHERE id IN (SELECT id FROM push_devices WHERE profile_id = $1 AND status = 'active' ORDER BY last_seen_at DESC, id DESC OFFSET $2)",
        )
        .bind(pid)
        .bind(MAX_DEVICES_PER_PROFILE)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok((id, linked.is_some()))
}

fn body_json(body: &Bytes) -> AppResult<Value> {
    if body.len() > MAX_BODY {
        return Err(AppError::bad("request too large"));
    }
    Ok(serde_json::from_slice(body)?)
}

/// `POST /client/push/subscribe?k=`: a browser's push subscription, with the snippet's identity.
pub async fn subscribe(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let row = settings::load(&state.db, a.id).await?.filter(Row::web_ready).ok_or_else(|| AppError::bad("web push isn't on for this site"))?;
            let b = body_json(&body)?;
            let endpoint = b["subscription"]["endpoint"].as_str().unwrap_or_default();
            let p256dh = b["subscription"]["keys"]["p256dh"].as_str().unwrap_or_default();
            let auth = b["subscription"]["keys"]["auth"].as_str().unwrap_or_default();
            if !super::providers::endpoint_allowed(&state, endpoint) {
                return Err(AppError::bad("that isn't a browser push service address"));
            }
            if !crypto::check_subscription_keys(p256dh, auth) {
                return Err(AppError::bad("the subscription's keys are invalid"));
            }
            take_registration(&state, a.id, &headers).await?;
            let mut profile_id = resolve_identity(&state, a.id, &row, &b["identity"]).await?;
            // A replaced subscription (from the service worker) keeps the old one's person: holding
            // the old endpoint and its auth secret shows it's the same browser.
            if let (None, Some(old), Some(old_auth)) = (profile_id, b["previous"]["endpoint"].as_str(), b["previous"]["auth"].as_str()) {
                let prev: Option<Option<Uuid>> = sqlx::query_scalar("DELETE FROM push_devices WHERE account_id = $1 AND kind = 'web' AND address = $2 AND auth_secret = $3 RETURNING profile_id")
                    .bind(a.id)
                    .bind(old)
                    .bind(old_auth)
                    .fetch_optional(&state.db)
                    .await?;
                profile_id = prev.flatten();
            }
            let (_, linked) = save_device(&state, a.id, "web", endpoint, Some((p256dh, auth)), profile_id, headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()), None).await?;
            Ok(json!({"ok": true, "linked": linked}))
        }
        .await,
    )
}

/// `POST /client/push/unsubscribe?k=`: a browser took permission back. The endpoint and its auth
/// secret show it's that browser.
pub async fn unsubscribe(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let (Some(endpoint), Some(auth)) = (b["endpoint"].as_str(), b["auth"].as_str()) else { return Err(AppError::bad("endpoint and auth are required")) };
            sqlx::query("DELETE FROM push_devices WHERE account_id = $1 AND kind = 'web' AND address = $2 AND auth_secret = $3").bind(a.id).bind(endpoint).bind(auth).execute(&state.db).await?;
            Ok(json!({"ok": true}))
        }
        .await,
    )
}

fn check_token(platform: &str, token: &str) -> AppResult<()> {
    let ok = match platform {
        // APNs device tokens are hex (32 bytes today; Apple says not to assume the length).
        "ios" => (32..=200).contains(&token.len()) && token.chars().all(|c| c.is_ascii_hexdigit()),
        "android" => (20..=4096).contains(&token.len()) && token.chars().all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c)),
        _ => return Err(AppError::bad("platform must be ios or android")),
    };
    if ok { Ok(()) } else { Err(AppError::bad("that isn't a device token for this platform")) }
}

/// `POST /client/push/devices?k=`: an app registers its device token (and, when signed in, its
/// verified identity). Call it on every launch and after sign-in.
pub async fn register_device(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let platform = b["platform"].as_str().unwrap_or_default();
            let token = b["token"].as_str().unwrap_or_default().trim();
            check_token(platform, token)?;
            let row = settings::load(&state.db, a.id).await?.ok_or_else(|| AppError::bad("push isn't set up for this account"))?;
            let ready = if platform == "ios" { row.apns_ready() } else { row.fcm_ready() };
            if !ready {
                return Err(AppError::bad(format!("add your {} credentials in Settings → Push first", if platform == "ios" { "Apple (APNs)" } else { "Firebase" })));
            }
            take_registration(&state, a.id, &headers).await?;
            let profile_id = resolve_identity(&state, a.id, &row, &b["identity"]).await?;
            let token = if platform == "ios" { token.to_ascii_lowercase() } else { token.to_string() };
            let (id, linked) = save_device(&state, a.id, platform, &token, None, profile_id, None, b["app_version"].as_str()).await?;
            Ok(json!({"ok": true, "device_id": id, "linked": linked}))
        }
        .await,
    )
}

/// `POST /client/push/devices/remove?k=`: the app signed out or turned notifications off.
/// Holding the device token shows it's that device.
pub async fn remove_device(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let platform = b["platform"].as_str().unwrap_or_default();
            let token = b["token"].as_str().unwrap_or_default().trim();
            check_token(platform, token)?;
            let token = if platform == "ios" { token.to_ascii_lowercase() } else { token.to_string() };
            sqlx::query("DELETE FROM push_devices WHERE account_id = $1 AND kind = $2 AND address = $3").bind(a.id).bind(platform).bind(&token).execute(&state.db).await?;
            Ok(json!({"ok": true}))
        }
        .await,
    )
}

#[derive(sqlx::FromRow)]
struct Delivery {
    id: Uuid,
    message_id: Uuid,
    account_id: Uuid,
    profile_id: Uuid,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    is_test: bool,
}

async fn load_delivery(state: &AppState, id: Uuid) -> AppResult<Option<Delivery>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.message_id, d.account_id, m.profile_id, m.campaign_id, m.flow_id, m.is_test FROM push_deliveries d JOIN messages m ON m.id = d.message_id WHERE d.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?)
}

/// The service worker showed a notification. Only the first report for a delivery counts.
pub async fn delivered(State(state): State<AppState>, body: Bytes) -> Response {
    answer(
        async {
            let b = body_json(&body)?;
            let Some(id) = b["t"].as_str().and_then(|t| super::send::delivery_from_token(&state, t)) else { return Err(AppError::bad("unknown notification")) };
            mark_delivered(&state, id).await?;
            Ok(json!({"ok": true}))
        }
        .await,
    )
}

pub async fn mark_delivered(state: &AppState, delivery_id: Uuid) -> AppResult<()> {
    let first: Option<Uuid> = sqlx::query_scalar("UPDATE push_deliveries SET delivered_at = now() WHERE id = $1 AND delivered_at IS NULL AND status = 'sent' RETURNING message_id")
        .bind(delivery_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(message_id) = first else { return Ok(()) };
    let changed: Option<(Uuid, bool)> = sqlx::query_as(
        "UPDATE messages SET status = CASE WHEN status = 'sent' THEN 'delivered' ELSE status END, delivered_at = now()
         WHERE id = $1 AND delivered_at IS NULL RETURNING account_id, is_test",
    )
    .bind(message_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((account_id, false)) = changed {
        crate::features::webhooks::message_delivered(&state.db, account_id, message_id).await;
    }
    Ok(())
}

/// Record a tap (or an app's open report) once per delivery: the delivery, the message, its
/// click list and the "Clicked Push" event that flows, segments and attribution read.
async fn mark_clicked(state: &AppState, d: &Delivery, url: Option<&str>) -> AppResult<()> {
    let first: Option<Uuid> = sqlx::query_scalar("UPDATE push_deliveries SET clicked_at = now(), delivered_at = COALESCE(delivered_at, now()) WHERE id = $1 AND clicked_at IS NULL RETURNING id")
        .bind(d.id)
        .fetch_optional(&state.db)
        .await?;
    if first.is_none() {
        return Ok(());
    }
    if let Some(u) = url {
        sqlx::query("INSERT INTO message_clicks (message_id, url) VALUES ($1, $2)").bind(d.message_id).bind(u).execute(&state.db).await?;
    }
    sqlx::query("UPDATE messages SET clicked_at = COALESCE(clicked_at, now()), delivered_at = COALESCE(delivered_at, now()) WHERE id = $1").bind(d.message_id).execute(&state.db).await?;
    if !d.is_test {
        record_event(
            &state.db,
            NewEvent {
                account_id: d.account_id,
                profile_id: d.profile_id,
                metric: "Clicked Push",
                properties: json!({"campaign_id": d.campaign_id, "flow_id": d.flow_id, "message_id": d.message_id, "url": url}),
                value: None,
                occurred_at: None,
                unique_id: Some(format!("click:{}", d.id)),
                message_id: Some(d.message_id),
            },
        )
        .await?;
    }
    Ok(())
}

/// `POST /client/push/opened?k=`: an app opened from a notification (send `dumbmail.t` from the payload).
pub async fn opened(State(state): State<AppState>, body: Bytes) -> Response {
    answer(
        async {
            let b = body_json(&body)?;
            let Some(id) = b["t"].as_str().and_then(|t| super::send::delivery_from_token(&state, t)) else { return Err(AppError::bad("unknown notification")) };
            if let Some(d) = load_delivery(&state, id).await? {
                mark_clicked(&state, &d, None).await?;
            }
            Ok(json!({"ok": true}))
        }
        .await,
    )
}

#[derive(Deserialize)]
pub struct ClickQuery {
    u: String,
    s: String,
}

fn own_site(account: &Account, dest: &url::Url) -> bool {
    let Some(host) = dest.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()) else { return false };
    let site = account.website.as_deref().map(|w| if w.contains("://") { w.to_string() } else { format!("https://{w}") }).and_then(|w| url::Url::parse(&w).ok()).and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()));
    site.iter().chain(account.verified_domains.iter()).any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// `GET /p/c/{token}?u=&s=`: a tap. Goes only to a link DumbMail signed for this delivery.
pub async fn click(State(state): State<AppState>, Path(token): Path<String>, Query(q): Query<ClickQuery>) -> Response {
    let Some(id) = super::send::delivery_from_token(&state, &token) else { return (StatusCode::BAD_REQUEST, "invalid link").into_response() };
    if crate::auth::link_sig(&state.cfg.secret_key, id, &q.u) != q.s || !(q.u.starts_with("https://") || q.u.starts_with("http://")) {
        return (StatusCode::BAD_REQUEST, "invalid link").into_response();
    }
    if let Ok(Some(d)) = load_delivery(&state, id).await {
        let _ = mark_clicked(&state, &d, Some(&q.u)).await;
        if !d.is_test {
            if let (Ok(mut dest), Ok(account)) = (url::Url::parse(&q.u), crate::campaigns::load_account(&state.db, d.account_id).await) {
                if own_site(&account, &dest) {
                    dest.query_pairs_mut().append_pair("_pl", &crate::public::profile_token(&state, d.account_id, d.profile_id));
                    return Redirect::to(dest.as_str()).into_response();
                }
            }
        }
    }
    Redirect::to(&q.u).into_response()
}

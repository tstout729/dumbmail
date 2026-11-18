//! Public push endpoints (no sign-in; the site's public key at most): the service worker and its
//! importable handler, the snippet's push add-on, browser and app device registration and sign-out,
//! delivery and open reports, and the tracked link a tap opens.
//!
//! Linking a device to a person needs an **enrollment grant** made by the customer's own server
//! (see [`crypto::verify_enroll_grant`]): an HMAC with the account's secret over the account, the
//! purpose, the identifier's type and value, and an expiry at most a day away. Nothing else names a
//! person here, not a tracked-link token and not a bare email. Without a grant a device belongs to an
//! anonymous profile of its own. Delivery and open reports carry a token DumbMail signed for that one
//! delivery, valid for [`super::send::TOKEN_DAYS`].

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
use crate::audience::ProfileInput;
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

/// Most devices one person keeps active; registering another turns off their least recent.
pub const MAX_DEVICES_PER_PROFILE: i64 = 10;
/// Registrations per client address per hour, per account.
pub const MAX_REGISTRATIONS_PER_HOUR: i32 = 30;
/// Anonymous devices an account keeps at most.
pub const MAX_ANONYMOUS: i64 = 200_000;
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

fn js(body: String) -> Response {
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], body).into_response()
}

fn fill(src: &str, base: &str, key: &str) -> String {
    src.replacen("__BASE__", &serde_json::to_string(base).unwrap_or_default(), 1).replacen("__KEY__", &serde_json::to_string(key).unwrap_or_default(), 1)
}

/// The service worker (or, for a site that already has a root worker, the handler it imports): it
/// opens only DumbMail's signed tap links, on the account's link domain or DumbMail's.
async fn worker(state: &AppState, k: &str) -> String {
    let key: String = k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(100).collect();
    let mut links = vec![format!("{}/p/c/", state.cfg.link_url().trim_end_matches('/'))];
    let mut vapid: Option<String> = None;
    if let Ok(a) = account_by_key(state, &key).await {
        let own = format!("{}/p/c/", crate::domains::link_base(state, &a).trim_end_matches('/'));
        if !links.contains(&own) {
            links.push(own);
        }
        vapid = settings::load(&state.db, a.id).await.ok().flatten().and_then(|r| r.vapid_public_key);
    }
    fill(SW_JS, state.cfg.public_url(), &key)
        .replacen("__LINKS__", &serde_json::to_string(&links).unwrap_or_else(|_| "[]".into()), 1)
        .replacen("__VAPID__", &serde_json::to_string(&vapid).unwrap_or_else(|_| "null".into()), 1)
}

/// `GET /push/sw.js?k=`: the service worker for the customer to host on their site.
pub async fn service_worker(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let mut r = js(worker(&state, &q.k).await);
    r.headers_mut().insert(header::CONTENT_DISPOSITION, "inline; filename=\"dumbmail-sw.js\"".parse().expect("static header"));
    r
}

/// `GET /push/handler.js?k=`: the same handlers, for `importScripts` in a site's own worker.
pub async fn handler(State(state): State<AppState>, Query(q): Query<KeyQuery>) -> Response {
    let mut r = js(worker(&state, &q.k).await);
    r.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().expect("static header"));
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
        Some(json!({"key": row.vapid_public_key, "sw": row.sw_path, "scope": row.sw_scope, "prompt": row.prompt(), "color": crate::render::safe_color(&a.brand_color, "#1f6f5c")}))
    }
    .await;
    js(fill(PUSH_JS, state.cfg.public_url(), &q.k).replacen("__CONFIG__", &config.map(|c| c.to_string()).unwrap_or_else(|| "null".into()), 1))
}

/// One more registration from this client address this hour, if under the cap (one conditional
/// upsert, so two at once can't both take the last slot).
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

/// The person an enrollment grant names (`{"type": "external_id" | "email", "value", "grant"}`),
/// created when the account has nobody with that identifier yet (consent unchanged). None without a
/// grant. A tracked-link token (`$token`) isn't a grant and is ignored.
pub async fn resolve_grant(state: &AppState, account_id: Uuid, row: &Row, v: &Value) -> AppResult<Option<Uuid>> {
    let Some(grant) = v.get("grant").and_then(Value::as_str).filter(|g| !g.is_empty()) else { return Ok(None) };
    if grant.len() > 200 {
        return Err(AppError::bad("that grant is too long"));
    }
    let secret = settings::identity_secret(state, row).ok_or_else(|| AppError::bad("enrollment grants aren't set up for this account"))?;
    let raw = v.get("value").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 320).ok_or_else(|| AppError::bad("a grant needs the value it was made for"))?;
    let (id_type, value, input) = match v.get("type").and_then(Value::as_str) {
        Some("external_id") => ("external_id", raw.to_string(), ProfileInput { external_id: Some(raw.to_string()), ..Default::default() }),
        Some("email") => {
            let e = crate::models::normalize_email(raw).ok_or_else(|| AppError::bad("that email isn't valid"))?;
            ("email", e.clone(), ProfileInput { email: Some(e), ..Default::default() })
        }
        _ => return Err(AppError::bad("a grant's type must be external_id or email")),
    };
    crypto::verify_enroll_grant(&secret, &account_id.to_string(), id_type, &value, grant, chrono::Utc::now().timestamp()).map_err(|why| {
        tracing::info!(account = %account_id, why, "push enrollment grant refused");
        AppError::Unauthorized
    })?;
    // Only a profile with that identifier, of that type (an email grant never names a profile whose
    // external id is the same string). Created when missing; consent never changes here.
    let p = match crate::audience::find_profile(&state.db, account_id, &input).await? {
        Some(p) => p,
        None => crate::audience::upsert_profile_public(&state.db, account_id, &input).await?.profile,
    };
    Ok(Some(p.id))
}

/// Who a registration belongs to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Owner {
    /// The person a grant named.
    Person(Uuid),
    /// A replaced browser subscription takes over its predecessor's owner (holding the old endpoint
    /// and its secret proves it's the same browser).
    Carry(Uuid, bool),
    /// Keep the current owner; a new device gets an anonymous profile of its own.
    Keep,
    /// Sign-out: a fresh anonymous profile, so the person signed out stops getting this device's pushes.
    Anonymous,
}

pub struct Registration<'a> {
    pub kind: &'a str,
    pub address: &'a str,
    pub keys: Option<(&'a str, &'a str)>,
    pub permission: &'a str,
    pub user_agent: Option<&'a str>,
    pub app_version: Option<&'a str>,
}

/// Save a registration in one transaction holding the account's push settings row (so the caps on
/// anonymous devices and on a person's devices hold under concurrent registrations). The version
/// goes up whenever the owner, the address or the keys change. Returns (device id, linked to a
/// known person).
pub async fn save_device(state: &AppState, account_id: Uuid, r: &Registration<'_>, owner: Owner) -> AppResult<(Uuid, bool)> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT 1 FROM push_settings WHERE account_id = $1 FOR UPDATE").bind(account_id).execute(&mut *tx).await?;
    type Existing = (Uuid, Option<Uuid>, bool, Option<String>, Option<String>, Option<Uuid>);
    let existing: Option<Existing> = sqlx::query_as(
        "SELECT id, profile_id, anonymous, p256dh, auth_secret, test_user_id FROM push_devices WHERE account_id = $1 AND kind = $2 AND address = $3 FOR UPDATE",
    )
    .bind(account_id)
    .bind(r.kind)
    .bind(r.address)
    .fetch_optional(&mut *tx)
    .await?;
    if existing.as_ref().is_some_and(|e| e.5.is_some()) {
        return Err(AppError::Conflict("this browser is a team member's test device".into()));
    }
    let (profile_id, anonymous) = match (owner, &existing) {
        (Owner::Person(p), _) => (p, false),
        (Owner::Carry(p, anon), _) => (p, anon),
        (Owner::Keep, Some((_, Some(p), anon, ..))) => (*p, *anon),
        (Owner::Keep, _) | (Owner::Anonymous, _) => {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM push_devices WHERE account_id = $1 AND anonymous").bind(account_id).fetch_one(&mut *tx).await?;
            if count >= MAX_ANONYMOUS {
                return Err(AppError::Busy("this account has too many anonymous push devices; unused ones are removed after 180 days".into()));
            }
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO profiles (id, account_id, properties) VALUES ($1, $2, '{\"anonymous_push_subscriber\": true}')").bind(id).bind(account_id).execute(&mut *tx).await?;
            (id, true)
        }
    };
    // The person's row, so concurrent registrations for them take turns with the cap below.
    sqlx::query("SELECT 1 FROM profiles WHERE id = $1 FOR UPDATE").bind(profile_id).execute(&mut *tx).await?;
    let keys = r.keys.map(|k| (k.0.to_string(), k.1.to_string()));
    let id = match &existing {
        Some((id, old_profile, _, p256dh, auth, _)) => {
            let changed = *old_profile != Some(profile_id) || (keys.is_some() && keys != p256dh.clone().zip(auth.clone()));
            sqlx::query(
                "UPDATE push_devices SET profile_id = $2, anonymous = $3, p256dh = COALESCE($4, p256dh), auth_secret = COALESCE($5, auth_secret), permission = $6,
                        status = 'active', failures = 0, disabled_at = NULL, last_seen_at = now(), version = version + CASE WHEN $7 THEN 1 ELSE 0 END,
                        user_agent = COALESCE($8, user_agent), app_version = COALESCE($9, app_version)
                 WHERE id = $1",
            )
            .bind(id)
            .bind(profile_id)
            .bind(anonymous)
            .bind(keys.as_ref().map(|k| k.0.clone()))
            .bind(keys.as_ref().map(|k| k.1.clone()))
            .bind(r.permission)
            .bind(changed)
            .bind(r.user_agent.map(|u| u.chars().take(300).collect::<String>()))
            .bind(r.app_version.map(|u| u.chars().take(40).collect::<String>()))
            .execute(&mut *tx)
            .await?;
            if let (Some(old), true) = (old_profile, *old_profile != Some(profile_id)) {
                forget_empty_anonymous(&mut tx, *old).await?;
            }
            *id
        }
        None => {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO push_devices (id, account_id, profile_id, anonymous, kind, address, p256dh, auth_secret, permission, user_agent, app_version)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(id)
            .bind(account_id)
            .bind(profile_id)
            .bind(anonymous)
            .bind(r.kind)
            .bind(r.address)
            .bind(keys.as_ref().map(|k| k.0.clone()))
            .bind(keys.as_ref().map(|k| k.1.clone()))
            .bind(r.permission)
            .bind(r.user_agent.map(|u| u.chars().take(300).collect::<String>()))
            .bind(r.app_version.map(|u| u.chars().take(40).collect::<String>()))
            .execute(&mut *tx)
            .await?;
            id
        }
    };
    // Over the per-person cap: their least recently seen devices are turned off.
    sqlx::query(
        "UPDATE push_devices SET status = 'disabled', disabled_at = now()
         WHERE id IN (SELECT id FROM push_devices WHERE profile_id = $1 AND status = 'active' ORDER BY last_seen_at DESC, id DESC OFFSET $2)",
    )
    .bind(profile_id)
    .bind(MAX_DEVICES_PER_PROFILE)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((id, !anonymous))
}

/// An anonymous profile left with no device, no message and no event is deleted.
async fn forget_empty_anonymous(tx: &mut sqlx::PgConnection, profile_id: Uuid) -> AppResult<()> {
    sqlx::query(
        "DELETE FROM profiles p WHERE p.id = $1 AND p.email IS NULL AND p.phone_number IS NULL AND p.external_id IS NULL
           AND p.properties ? 'anonymous_push_subscriber'
           AND NOT EXISTS (SELECT 1 FROM push_devices d WHERE d.profile_id = p.id)
           AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.profile_id = p.id)
           AND NOT EXISTS (SELECT 1 FROM events e WHERE e.profile_id = p.id)",
    )
    .bind(profile_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

fn body_json(body: &Bytes) -> AppResult<Value> {
    if body.len() > MAX_BODY {
        return Err(AppError::bad("request too large"));
    }
    Ok(serde_json::from_slice(body)?)
}

/// `POST /client/push/subscribe?k=`: a browser's push subscription (it exists only once the person
/// allowed notifications), with an enrollment grant when the site has one.
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
            let mut owner = match resolve_grant(&state, a.id, &row, &b["identity"]).await? {
                Some(p) => Owner::Person(p),
                None => Owner::Keep,
            };
            // A replaced subscription (from the service worker) keeps the old one's owner.
            if let (Owner::Keep, Some(old), Some(old_auth)) = (owner, b["previous"]["endpoint"].as_str(), b["previous"]["auth"].as_str()) {
                let prev: Option<(Option<Uuid>, bool)> =
                    sqlx::query_as("DELETE FROM push_devices WHERE account_id = $1 AND kind = 'web' AND address = $2 AND auth_secret = $3 RETURNING profile_id, anonymous")
                        .bind(a.id)
                        .bind(old)
                        .bind(old_auth)
                        .fetch_optional(&state.db)
                        .await?;
                if let Some((Some(p), anon)) = prev {
                    owner = Owner::Carry(p, anon);
                }
            }
            let reg = Registration { kind: "web", address: endpoint, keys: Some((p256dh, auth)), permission: "granted", user_agent: headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()), app_version: None };
            let (_, linked) = save_device(&state, a.id, &reg, owner).await?;
            Ok(json!({"ok": true, "linked": linked}))
        }
        .await,
    )
}

/// `POST /client/push/unsubscribe?k=`: the person turned push off in this browser, or took the
/// permission back. The endpoint and its auth secret show it's that browser.
pub async fn unsubscribe(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let (Some(endpoint), Some(auth)) = (b["endpoint"].as_str(), b["auth"].as_str()) else { return Err(AppError::bad("endpoint and auth are required")) };
            let n = sqlx::query("DELETE FROM push_devices WHERE account_id = $1 AND kind = 'web' AND address = $2 AND auth_secret = $3").bind(a.id).bind(endpoint).bind(auth).execute(&state.db).await?.rows_affected();
            // Confirmed either way: after this the server has no such registration.
            Ok(json!({"ok": true, "removed": n}))
        }
        .await,
    )
}

/// `POST /client/push/sign-out?k=`: the site's visitor signed out: this browser goes back to an
/// anonymous profile of its own.
pub async fn sign_out(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let (Some(endpoint), Some(auth)) = (b["endpoint"].as_str(), b["auth"].as_str()) else { return Err(AppError::bad("endpoint and auth are required")) };
            let found: Option<(Option<String>, String)> = sqlx::query_as("SELECT p256dh, permission FROM push_devices WHERE account_id = $1 AND kind = 'web' AND address = $2 AND auth_secret = $3")
                .bind(a.id)
                .bind(endpoint)
                .bind(auth)
                .fetch_optional(&state.db)
                .await?;
            let Some((_, permission)) = found else { return Ok(json!({"ok": true})) };
            let reg = Registration { kind: "web", address: endpoint, keys: None, permission: &permission, user_agent: None, app_version: None };
            save_device(&state, a.id, &reg, Owner::Anonymous).await?;
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

/// The app's report of the device's notification permission, as stored: only granted or (iOS)
/// provisional is ever sent to.
fn permission_of(platform: &str, v: &Value) -> AppResult<&'static str> {
    match (platform, v.as_str()) {
        (_, Some("granted" | "authorized")) => Ok("granted"),
        ("ios", Some("provisional" | "ephemeral")) => Ok("provisional"),
        (_, Some("denied" | "not_determined")) => Ok("denied"),
        _ => Err(AppError::bad("send the device's notification permission: granted, provisional (iOS), denied or not_determined")),
    }
}

/// `POST /client/push/devices?k=`: an app registers its device token with the notification
/// permission the person gave (and, when signed in, an enrollment grant). Call it on every launch,
/// after asking for permission, after sign-in, and whenever the permission changes.
pub async fn register_device(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let platform = b["platform"].as_str().unwrap_or_default();
            let token = b["token"].as_str().unwrap_or_default().trim();
            check_token(platform, token)?;
            let permission = permission_of(platform, &b["permission"])?;
            let row = settings::load(&state.db, a.id).await?.ok_or_else(|| AppError::bad("push isn't set up for this account"))?;
            let ready = if platform == "ios" { row.apns_ready() } else { row.fcm_ready() };
            if !ready {
                return Err(AppError::bad(format!("add your {} credentials in Settings → Push first", if platform == "ios" { "Apple (APNs)" } else { "Firebase" })));
            }
            take_registration(&state, a.id, &headers).await?;
            let owner = match resolve_grant(&state, a.id, &row, &b["identity"]).await? {
                Some(p) => Owner::Person(p),
                None => Owner::Keep,
            };
            let token = if platform == "ios" { token.to_ascii_lowercase() } else { token.to_string() };
            let reg = Registration { kind: platform, address: &token, keys: None, permission, user_agent: None, app_version: b["app_version"].as_str() };
            let (id, linked) = save_device(&state, a.id, &reg, owner).await?;
            Ok(json!({"ok": true, "device_id": id, "linked": linked, "sendable": permission != "denied"}))
        }
        .await,
    )
}

/// `POST /client/push/devices/sign-out?k=`: the app's user signed out; the device goes back to an
/// anonymous profile of its own. `POST /client/push/devices/remove?k=` forgets it entirely.
pub async fn device_sign_out(State(state): State<AppState>, Query(q): Query<KeyQuery>, body: Bytes) -> Response {
    answer(
        async {
            let a = account_by_key(&state, &q.k).await?;
            let b = body_json(&body)?;
            let platform = b["platform"].as_str().unwrap_or_default();
            let token = b["token"].as_str().unwrap_or_default().trim();
            check_token(platform, token)?;
            let token = if platform == "ios" { token.to_ascii_lowercase() } else { token.to_string() };
            let permission: Option<String> = sqlx::query_scalar("SELECT permission FROM push_devices WHERE account_id = $1 AND kind = $2 AND address = $3")
                .bind(a.id)
                .bind(platform)
                .bind(&token)
                .fetch_optional(&state.db)
                .await?;
            let Some(permission) = permission else { return Ok(json!({"ok": true})) };
            let reg = Registration { kind: platform, address: &token, keys: None, permission: &permission, user_agent: None, app_version: None };
            save_device(&state, a.id, &reg, Owner::Anonymous).await?;
            Ok(json!({"ok": true}))
        }
        .await,
    )
}

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

/// The service worker showed a notification.
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

/// A delivery was shown. A report that arrives before the hand-off's result was saved is kept and
/// reconciled: showing it proves the push service took it, so the delivery counts as sent (and the
/// message as sent and charged, once). Only the first report for a message tells webhooks.
pub async fn mark_delivered(state: &AppState, delivery_id: Uuid) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let d: Option<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT pd.message_id, pd.status, m.delivered_at IS NULL FROM push_deliveries pd JOIN messages m ON m.id = pd.message_id WHERE pd.id = $1 FOR UPDATE OF m, pd",
    )
    .bind(delivery_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((message_id, status, first)) = d else { return Ok(()) };
    if !matches!(status.as_str(), "sending" | "sent" | "unconfirmed") {
        return Ok(());
    }
    sqlx::query(
        "UPDATE push_deliveries SET delivered_at = COALESCE(delivered_at, now()), sent_at = COALESCE(sent_at, now()),
                status = CASE WHEN status IN ('sending', 'unconfirmed') THEN 'sent' ELSE status END, error = CASE WHEN status = 'unconfirmed' THEN NULL ELSE error END
         WHERE id = $1",
    )
    .bind(delivery_id)
    .execute(&mut *tx)
    .await?;
    super::send::accept_in(&mut tx, state, message_id).await?;
    let (account_id, is_test): (Uuid, bool) = sqlx::query_as("SELECT account_id, is_test FROM messages WHERE id = $1").bind(message_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    if first && !is_test {
        crate::features::webhooks::message_delivered(&state.db, account_id, message_id).await;
    }
    Ok(())
}

/// Record a tap (or an app's open report) for a delivery that went out: once per delivery for its
/// click list, and once per message (in the same transaction that claims the message's first click)
/// for the "Clicked Push" event that flows, segments and attribution read. A tap proves the delivery
/// arrived, so an unconfirmed one is reconciled as sent.
async fn mark_clicked(state: &AppState, delivery_id: Uuid, url: Option<&str>) -> AppResult<Option<(Uuid, Uuid, bool)>> {
    let mut tx = state.db.begin().await?;
    let d: Option<(Uuid, String, bool, Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT pd.message_id, pd.status, pd.delivered_at IS NOT NULL, m.account_id, m.profile_id, m.is_test
         FROM push_deliveries pd JOIN messages m ON m.id = pd.message_id WHERE pd.id = $1 FOR UPDATE OF m, pd",
    )
    .bind(delivery_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((message_id, status, shown, account_id, profile_id, is_test)) = d else { return Ok(None) };
    if !(matches!(status.as_str(), "sent" | "unconfirmed") || shown) {
        return Ok(None);
    }
    let first: Option<Uuid> = sqlx::query_scalar(
        "UPDATE push_deliveries SET clicked_at = now(), delivered_at = COALESCE(delivered_at, now()), sent_at = COALESCE(sent_at, now()),
                status = CASE WHEN status = 'unconfirmed' THEN 'sent' ELSE status END
         WHERE id = $1 AND clicked_at IS NULL RETURNING id",
    )
    .bind(delivery_id)
    .fetch_optional(&mut *tx)
    .await?;
    if first.is_some() {
        if let Some(u) = url {
            sqlx::query("INSERT INTO message_clicks (message_id, url) VALUES ($1, $2)").bind(message_id).bind(u).execute(&mut *tx).await?;
        }
    }
    let message_first: Option<Uuid> = sqlx::query_scalar("UPDATE messages SET clicked_at = now() WHERE id = $1 AND clicked_at IS NULL RETURNING id").bind(message_id).fetch_optional(&mut *tx).await?;
    super::send::accept_in(&mut tx, state, message_id).await?;
    if message_first.is_some() && !is_test {
        crate::jobs::enqueue_background(&mut *tx, "push_event", json!({"message_id": message_id, "account_id": account_id, "metric": "Clicked Push"}), None).await?;
    }
    tx.commit().await?;
    Ok(Some((account_id, profile_id, is_test)))
}

/// `POST /client/push/opened?k=`: an app opened from a notification (send `dumbmail.t` from the payload).
pub async fn opened(State(state): State<AppState>, body: Bytes) -> Response {
    answer(
        async {
            let b = body_json(&body)?;
            let Some(id) = b["t"].as_str().and_then(|t| super::send::delivery_from_token(&state, t)) else { return Err(AppError::bad("unknown notification")) };
            mark_clicked(&state, id, None).await?;
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

/// `GET /p/c/{token}?u=&s=`: a tap. Goes only to a link DumbMail signed for this delivery (MAC
/// compared in constant time), and only while the token hasn't expired.
pub async fn click(State(state): State<AppState>, Path(token): Path<String>, Query(q): Query<ClickQuery>) -> Response {
    let Some(id) = super::send::delivery_from_token(&state, &token) else { return (StatusCode::BAD_REQUEST, "this link has expired or isn't valid").into_response() };
    if !crypto::ct_eq(&crate::auth::link_sig(&state.cfg.secret_key, id, &q.u), &q.s) || !(q.u.starts_with("https://") || q.u.starts_with("http://")) {
        return (StatusCode::BAD_REQUEST, "invalid link").into_response();
    }
    if let Ok(Some((account_id, profile_id, false))) = mark_clicked(&state, id, Some(&q.u)).await {
        if let (Ok(mut dest), Ok(account)) = (url::Url::parse(&q.u), crate::campaigns::load_account(&state.db, account_id).await) {
            if own_site(&account, &dest) {
                dest.query_pairs_mut().append_pair("_pl", &crate::public::profile_token(&state, account_id, profile_id));
                return Redirect::to(dest.as_str()).into_response();
            }
        }
    }
    Redirect::to(&q.u).into_response()
}

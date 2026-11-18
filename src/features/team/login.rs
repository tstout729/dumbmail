//! Signing in: sessions, password sign-in, two-factor codes, password changes and resets.
//!
//! A session starts as soon as the password (or Google, or SSO) checks out, but until two-factor
//! is done it carries a [`Restriction`] and can only finish signing in (see `access`). That keeps
//! one code path for every sign-in method:
//! - password sign-ins need the person's two-factor code when they've turned it on;
//! - when an account requires two-factor for everyone, password and Google sign-ins need it too
//!   (people without it set it up first). Google's own sign-in doesn't count: we can't tell whether
//!   the Google account has two-step verification, so we ask for our own code;
//! - a single sign-on sign-in counts only when the ID token's `amr` claim shows a second factor,
//!   or the owner said the identity provider enforces one (see `sso`); otherwise it needs our code.
//!
//! Changing someone's factors (turning two-factor on or off, new recovery codes) signs out their
//! sessions that are still waiting on a code, so a half-finished sign-in can't become a full one.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::access::{self, Restriction, Role};
use super::audit::{audit, record, Actor};
use super::{mail, ratelimit};
use crate::api::ApiJson;
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::state::AppState;

/// Where a request came from, for sessions and the audit log.
#[derive(Debug, Clone, Default)]
pub struct ReqMeta {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl ReqMeta {
    pub fn from_headers(h: &HeaderMap) -> Self {
        ReqMeta { ip: client_ip(h), user_agent: h.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(300).collect()) }
    }
    pub fn actor(&self, user_id: Option<Uuid>) -> Actor {
        Actor { user_id, key_id: None, ip: self.ip.clone(), user_agent: self.user_agent.clone() }
    }
}

/// The caller's address, worked out the same way as for the public endpoints' limits
/// (`public::client_ip`).
pub fn client_ip(headers: &HeaderMap) -> Option<String> {
    crate::public::client_ip(headers)
}

/// Constant-time string comparison.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Sessions ----

/// How a new session was signed in.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStart {
    /// The login's `credential_version` the password was checked against. The session is only
    /// made if it's still current (a password change or reset in the meantime wins).
    pub credential_version: Option<i32>,
    /// A second factor is already done (an identity provider that reported one).
    pub mfa_ok: bool,
    /// The person just typed their password, which counts as confirming it's them.
    pub entered_password: bool,
    /// For single sign-on: the provider settings revision the sign-in ran under. The session is
    /// only made while it's still the current one (and single sign-on is still on).
    pub sso_revision: Option<i32>,
}

/// Start a browser session. `method` is password, google or sso (`sso_account` is the account
/// whose identity provider signed them in).
pub async fn start_session(state: &AppState, user_id: Uuid, account_id: Uuid, method: &str, sso_account: Option<Uuid>, meta: &ReqMeta) -> AppResult<String> {
    start_session_with(state, user_id, account_id, method, sso_account, meta, SessionStart::default()).await
}

pub async fn start_session_with(state: &AppState, user_id: Uuid, account_id: Uuid, method: &str, sso_account: Option<Uuid>, meta: &ReqMeta, how: SessionStart) -> AppResult<String> {
    let token = auth::random_token(32);
    // One statement, holding the login's row: a reset that changed the password after it was
    // checked (or is changing it right now) leaves no row to insert from.
    let n = sqlx::query(
        "INSERT INTO sessions (id, token_hash, user_id, account_id, expires_at, auth_method, sso_account_id, ip, user_agent, mfa_ok, reauth_at, sso_revision)
         SELECT $1, $2, u.id, $4, now() + interval '30 days', $5, $6, $7, $8, $9, CASE WHEN $10 THEN now() END, $12
         FROM users u WHERE u.id = $3 AND ($11::int IS NULL OR u.credential_version = $11)
           AND ($12::int IS NULL OR EXISTS (SELECT 1 FROM account_sso c WHERE c.account_id = $6 AND c.enabled AND c.revision = $12))
         FOR SHARE OF u",
    )
    .bind(Uuid::now_v7())
    .bind(auth::sha256_hex(&token))
    .bind(user_id)
    .bind(account_id)
    .bind(method)
    .bind(sso_account)
    .bind(&meta.ip)
    .bind(&meta.user_agent)
    .bind(how.mfa_ok)
    .bind(how.entered_password)
    .bind(how.credential_version)
    .bind(how.sso_revision)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n != 1 {
        if how.sso_revision.is_some() {
            return Err(AppError::Conflict("the single sign-on settings changed while you were signing in; sign in again".into()));
        }
        return Err(AppError::BadRequest("your password just changed; sign in again with the new one".into()));
    }
    sqlx::query("UPDATE users SET last_account_id = $2 WHERE id = $1").bind(user_id).bind(account_id).execute(&state.db).await?;
    Ok(token)
}

/// Sign out a login's sessions that are still waiting on a two-factor code (or would be after a
/// change to their factors), except `keep`. Called whenever someone's factors change.
pub async fn revoke_unverified_sessions(state: &AppState, user_id: Uuid, keep: Option<Uuid>) -> AppResult<u64> {
    Ok(sqlx::query(
        "DELETE FROM sessions s USING accounts a WHERE s.user_id = $1 AND a.id = s.account_id AND NOT s.mfa_ok AND s.id IS DISTINCT FROM $2
           AND (s.auth_method = 'password' OR a.require_two_factor)",
    )
    .bind(user_id)
    .bind(keep)
    .execute(&state.db)
    .await?
    .rows_affected())
}

/// The account a login opens: the one used last, else the first it joined.
pub async fn pick_account(state: &AppState, user_id: Uuid) -> AppResult<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT m.account_id FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.user_id = $1
         ORDER BY (m.account_id = u.last_account_id) DESC NULLS LAST, m.created_at LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?)
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Option<Uuid>,
    token_hash: String,
    user_id: Uuid,
    account_id: Uuid,
    auth_method: String,
    mfa_ok: bool,
    sso_account_id: Option<Uuid>,
    last_seen_at: DateTime<Utc>,
    reauth_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    sso_revision: Option<i32>,
    sso_current_revision: Option<i32>,
    role: Option<String>,
    require_two_factor: bool,
    totp_on: bool,
    email: String,
    sso_enforced: bool,
    sso_domains: Vec<String>,
}

fn email_domain(email: &str) -> &str {
    email.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
}

/// The signed-in context for a session cookie.
pub async fn session_ctx(state: &AppState, token: &str, meta: &ReqMeta) -> AppResult<Ctx> {
    load_session(state, Some(auth::sha256_hex(token)), None, meta).await
}

/// The context for a session id, read again after a change made during a request.
pub async fn ctx_for_session_id(state: &AppState, sid: Uuid, meta: &ReqMeta) -> AppResult<Ctx> {
    load_session(state, None, Some(sid), meta).await
}

async fn load_session(state: &AppState, token_hash: Option<String>, sid: Option<Uuid>, meta: &ReqMeta) -> AppResult<Ctx> {
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT s.id, s.token_hash, s.user_id, s.account_id, s.auth_method, s.mfa_ok, s.sso_account_id, s.last_seen_at, s.reauth_at, s.created_at,
                s.sso_revision, CASE WHEN sso.enabled THEN sso.revision END AS sso_current_revision, m.role, a.require_two_factor, (u.totp_enabled_at IS NOT NULL) AS totp_on, u.email,
                COALESCE(sso.enabled AND sso.enforce, false) AS sso_enforced, COALESCE(sso.domains, '{}') AS sso_domains
         FROM sessions s
         JOIN users u ON u.id = s.user_id
         JOIN accounts a ON a.id = s.account_id
         LEFT JOIN memberships m ON m.user_id = s.user_id AND m.account_id = s.account_id
         LEFT JOIN account_sso sso ON sso.account_id = s.account_id
         WHERE (s.token_hash = $1 OR s.id = $2) AND s.expires_at > now()",
    )
    .bind(token_hash)
    .bind(sid)
    .fetch_optional(&state.db)
    .await?;
    let r = row.ok_or(AppError::Unauthorized)?;
    let role = r.role.as_deref().and_then(Role::parse).ok_or(AppError::Unauthorized)?;
    let sso_session = r.auth_method == "sso";
    // A single sign-on session works only in its own account, and only while the provider
    // settings it started under are still the current ones (and still on).
    if sso_session && (r.sso_account_id != Some(r.account_id) || r.sso_revision.is_none() || r.sso_revision != r.sso_current_revision) {
        return Err(AppError::Unauthorized);
    }
    // Sessions from before session ids existed get one the first time they're used.
    let id = match r.id {
        Some(id) => id,
        None => sqlx::query_scalar("UPDATE sessions SET id = COALESCE(id, $2) WHERE token_hash = $1 RETURNING id").bind(&r.token_hash).bind(Uuid::now_v7()).fetch_one(&state.db).await?,
    };
    let restriction = if r.auth_method == "password" && r.totp_on && !r.mfa_ok {
        Some(Restriction::TwoFactorCode)
    } else if r.require_two_factor && !r.mfa_ok {
        Some(if r.totp_on { Restriction::TwoFactorCode } else { Restriction::TwoFactorSetup })
    } else if r.sso_enforced && !sso_session && role != Role::Owner && r.sso_domains.iter().any(|d| d.eq_ignore_ascii_case(email_domain(&r.email))) {
        // Owners keep their other sign-in so a broken identity provider can't lock everyone out.
        Some(Restriction::SsoRequired)
    } else {
        None
    };
    if r.last_seen_at < Utc::now() - chrono::Duration::minutes(5) {
        sqlx::query("UPDATE sessions SET last_seen_at = now(), ip = COALESCE($2, ip), user_agent = COALESCE($3, user_agent) WHERE id = $1")
            .bind(id)
            .bind(&meta.ip)
            .bind(&meta.user_agent)
            .execute(&state.db)
            .await?;
    }
    Ok(Ctx {
        account_id: r.account_id,
        user_id: Some(r.user_id),
        via_api_key: false,
        role,
        api_key_id: None,
        scopes: None,
        session_id: Some(id),
        sso_session,
        restriction,
        reauth_at: r.reauth_at,
        // A Google sign-in made this session; for accepting an owner invite right after it.
        google_sign_in_at: (r.auth_method == "google").then_some(r.created_at),
        ip: meta.ip.clone(),
        user_agent: meta.user_agent.clone(),
    })
}

/// The context for an API key: its account, its scopes, and the role of the member who made it
/// (keys made before teams existed act as an owner).
pub async fn key_ctx(state: &AppState, key: &str, meta: &ReqMeta) -> AppResult<Ctx> {
    if !key.starts_with(auth::API_KEY_PREFIX) {
        return Err(AppError::Unauthorized);
    }
    // A key made by another key works only while every key above it still does.
    let row: Option<(Uuid, Uuid, Vec<String>, Option<Uuid>)> = sqlx::query_as(
        "UPDATE api_keys k SET last_used_at = now() WHERE k.key_hash = $1 AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > now())
           AND NOT EXISTS (
             WITH RECURSIVE up(id, parent, revoked_at, expires_at, depth) AS (
               SELECT p.id, p.parent_key_id, p.revoked_at, p.expires_at, 1 FROM api_keys p WHERE p.id = k.parent_key_id
               UNION ALL
               SELECT p.id, p.parent_key_id, p.revoked_at, p.expires_at, up.depth + 1 FROM api_keys p JOIN up ON p.id = up.parent WHERE up.depth < 50
             )
             SELECT 1 FROM up WHERE up.revoked_at IS NOT NULL OR (up.expires_at IS NOT NULL AND up.expires_at <= now())
           )
         RETURNING k.id, k.account_id, k.scopes, k.created_by",
    )
    .bind(auth::sha256_hex(key))
    .fetch_optional(&state.db)
    .await?;
    let (key_id, account_id, scopes, created_by) = row.ok_or(AppError::Unauthorized)?;
    let role = match created_by {
        None => Role::Owner,
        Some(uid) => {
            let r: Option<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE user_id = $1 AND account_id = $2").bind(uid).bind(account_id).fetch_optional(&state.db).await?;
            r.as_deref().and_then(Role::parse).ok_or(AppError::Unauthorized)?
        }
    };
    Ok(Ctx {
        account_id,
        user_id: None,
        via_api_key: true,
        role,
        api_key_id: Some(key_id),
        scopes: Some(scopes),
        session_id: None,
        sso_session: false,
        restriction: None,
        reauth_at: None,
        google_sign_in_at: None,
        ip: meta.ip.clone(),
        user_agent: meta.user_agent.clone(),
    })
}

/// Everything the dashboard needs about the signed-in person (`GET /api/me`).
pub async fn me_json(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let user: Option<User> = match ctx.user_id {
        Some(uid) => sqlx::query_as("SELECT id, email, name FROM users WHERE id = $1").bind(uid).fetch_optional(&state.db).await?,
        None => None,
    };
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let accounts: Vec<Value> = match ctx.user_id {
        Some(uid) => {
            let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
                "SELECT a.id, a.name, a.brand_color, m.role FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = $1 ORDER BY lower(a.name), a.id LIMIT 200",
            )
            .bind(uid)
            .fetch_all(&state.db)
            .await?;
            rows.into_iter()
                .filter(|(id, ..)| !ctx.sso_session || *id == ctx.account_id)
                .map(|(id, name, color, role)| json!({"id": id, "name": name, "brand_color": color, "role": role, "current": id == ctx.account_id}))
                .collect()
        }
        None => Vec::new(),
    };
    let (two_factor, has_password, has_google): (bool, bool, bool) = match ctx.user_id {
        Some(uid) => sqlx::query_as("SELECT totp_enabled_at IS NOT NULL, password_hash IS NOT NULL, google_sub IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?,
        None => (false, false, false),
    };
    let key = match ctx.api_key_id {
        Some(id) => sqlx::query_as::<_, (String, String, Vec<String>)>("SELECT name, prefix, scopes FROM api_keys WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .map(|(name, prefix, scopes)| json!({"id": id, "name": name, "prefix": prefix, "scopes": scopes})),
        None => None,
    };
    // Until sign-in is finished, only the account's name is shown.
    let account_json = if ctx.restriction.is_some() { json!({"id": account.id, "name": account.name, "brand_color": account.brand_color}) } else { serde_json::to_value(&account)? };
    Ok(json!({
        "user": user,
        "account": account_json,
        "role": ctx.role.as_str(),
        "permissions": access::granted(ctx),
        "restriction": ctx.restriction,
        "restriction_message": ctx.restriction.map(|r| r.message()),
        "sign_in_method": if ctx.via_api_key { None } else if ctx.sso_session { Some("sso") } else { Some("other") },
        "two_factor_enabled": two_factor,
        "login_methods": {"password": has_password, "google": has_google},
        "accounts": accounts,
        "api_key": key,
        "support_email": state.cfg.support_email,
    }))
}

// ---- Password sign-in ----

/// Longest password a new or changed password may be. Signing in checks a password of any length,
/// because sign-up never had a limit.
const MAX_NEW_PASSWORD: usize = 1024;

/// A fixed hash to check against when the email is unknown, so both answers take as long.
fn dummy_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| auth::hash_password("dumbmail-not-a-real-password").unwrap_or_default())
}

/// Window for counting failed sign-ins.
const LOGIN_WINDOW: i64 = 900;

/// The wait before checking a password for an email that's had `failures` wrong guesses from
/// anywhere lately: nothing for the first few, then a little more each time, up to 3 seconds. A
/// wait, not a lockout, so strangers guessing can't keep the real person out.
pub fn login_delay(failures: i32) -> std::time::Duration {
    std::time::Duration::from_millis((failures.saturating_sub(5).max(0) as u64 * 200).min(3000))
}

/// The wait before checking a password from an address that's had `failures` wrong guesses
/// (across every email) lately: nothing for the first 30, then 200 ms more for each, up to 8
/// seconds. Shared addresses (an office, a phone network) are slowed down, never locked out.
pub fn ip_login_delay(failures: i32) -> std::time::Duration {
    std::time::Duration::from_millis((failures.saturating_sub(30).max(0) as u64 * 200).min(8000))
}

/// Password checks one address may have running at once, so waiting can't be used to pile up work.
const LOGIN_IN_FLIGHT_PER_IP: i32 = 4;
/// How long an in-flight count lasts if a server stops halfway; longer than any wait.
const IN_FLIGHT_WINDOW: i64 = 60;

/// Check an email and password and start a session. The answer is the same for an unknown email,
/// a wrong password and a login that only uses Google.
///
/// Throttling only ever counts failures. Only the pair (email, address) is refused: 10 wrong
/// passwords in 15 minutes. Failures per email from everywhere ([`login_delay`]) and per address
/// across emails ([`ip_login_delay`]) only slow the next check down, and an address can have only
/// a few checks running at once. So the right password always gets in once its wait is over,
/// whatever other people at the same address typed.
pub async fn password_login(state: &AppState, email: &str, password: &str, meta: &ReqMeta) -> AppResult<String> {
    let email = email.trim().to_lowercase();
    let ip = meta.ip.as_deref().unwrap_or("unknown");
    // Counted up front and given back on success, so parallel guesses can't slip past.
    let pair_bucket = format!("login:email-ip:{email}|{ip}");
    let ip_fail_bucket = format!("login:fail-ip:{ip}");
    let email_bucket = format!("login:email:{email}");
    let in_flight = format!("login:in-flight:{ip}");
    ratelimit::hit(state, &pair_bucket, 10, LOGIN_WINDOW).await?;
    let in_flight_limit = if meta.ip.is_some() { LOGIN_IN_FLIGHT_PER_IP } else { LOGIN_IN_FLIGHT_PER_IP * 20 };
    let slot = InFlight { state: state.clone(), bucket: in_flight };
    if ratelimit::hit(state, &slot.bucket, in_flight_limit, IN_FLIGHT_WINDOW).await.is_err() {
        slot.release().await;
        ratelimit::refund(state, &pair_bucket, LOGIN_WINDOW).await?;
        return Err(AppError::Busy("Too many sign-ins from your network at once. Try again in a moment.".into()));
    }
    let checked = check_password(state, &email, password, &email_bucket, &ip_fail_bucket, meta.ip.is_some()).await;
    slot.release().await;
    let row = checked?;
    let bad = || AppError::BadRequest("that email and password don't match (if you signed up with Google, use Continue with Google)".into());
    let (user_id, version) = match row {
        Ok((uid, version)) => {
            ratelimit::refund(state, &pair_bucket, LOGIN_WINDOW).await?;
            (uid, version)
        }
        Err(Some(uid)) => {
            for account_id in user_accounts(state, uid).await? {
                record(state, account_id, &meta.actor(Some(uid)), "login.failed", Some(&email), json!({"method": "password"})).await;
            }
            return Err(bad());
        }
        Err(None) => return Err(bad()),
    };
    let account_id = pick_account(state, user_id).await?.ok_or_else(|| AppError::Forbidden("this login isn't part of any account anymore; ask a teammate to invite you again".into()))?;
    let how = SessionStart { credential_version: Some(version), mfa_ok: false, entered_password: true, ..Default::default() };
    let token = start_session_with(state, user_id, account_id, "password", None, meta, how).await?;
    record(state, account_id, &meta.actor(Some(user_id)), "login", None, json!({"method": "password"})).await;
    Ok(token)
}

/// One running password check from an address, given back when it ends, even if the request is
/// dropped halfway (a closed connection).
struct InFlight {
    state: AppState,
    bucket: String,
}

impl InFlight {
    async fn release(mut self) {
        let bucket = std::mem::take(&mut self.bucket);
        if let Err(e) = ratelimit::refund(&self.state, &bucket, IN_FLIGHT_WINDOW).await {
            tracing::warn!(error = %e, "couldn't release a sign-in slot");
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.bucket.is_empty() {
            return;
        }
        let (state, bucket) = (self.state.clone(), std::mem::take(&mut self.bucket));
        tokio::spawn(async move {
            if let Err(e) = ratelimit::refund(&state, &bucket, IN_FLIGHT_WINDOW).await {
                tracing::warn!(error = %e, "couldn't release a sign-in slot");
            }
        });
    }
}

/// Wait out the delays, then check the password. `Ok`: the login and its credential version;
/// `Err(Some(user))`: a wrong password for a login; `Err(None)`: no such login (or no password).
/// Failures are counted for the email and the address.
async fn check_password(state: &AppState, email: &str, password: &str, email_bucket: &str, ip_bucket: &str, known_ip: bool) -> AppResult<Result<(Uuid, i32), Option<Uuid>>> {
    let email_failures = ratelimit::count(state, email_bucket, LOGIN_WINDOW).await?;
    let ip_failures = ratelimit::count(state, ip_bucket, LOGIN_WINDOW).await?;
    let ip_failures = if known_ip { ip_failures } else { ip_failures / 20 };
    tokio::time::sleep(login_delay(email_failures).max(ip_login_delay(ip_failures))).await;
    let row: Option<(Uuid, Option<String>, i32)> = sqlx::query_as("SELECT id, password_hash, credential_version FROM users WHERE email = $1").bind(email).fetch_optional(&state.db).await?;
    let ok = match &row {
        Some((_, Some(hash), _)) => auth::verify_password(password, hash),
        _ => {
            auth::verify_password(password, dummy_hash());
            false
        }
    };
    if !ok {
        ratelimit::hit(state, email_bucket, i32::MAX, LOGIN_WINDOW).await?;
        ratelimit::hit(state, ip_bucket, i32::MAX, LOGIN_WINDOW).await?;
    }
    Ok(match row {
        Some((uid, _, version)) if ok => Ok((uid, version)),
        Some((uid, _, _)) => Err(Some(uid)),
        None => Err(None),
    })
}

async fn user_accounts(state: &AppState, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar("SELECT account_id FROM memberships WHERE user_id = $1 ORDER BY created_at LIMIT 20").bind(user_id).fetch_all(&state.db).await?)
}

// ---- Two-factor codes (RFC 6238 TOTP) ----

const RECOVERY_CODES: usize = 10;

fn totp_for(secret: &[u8], email: &str) -> AppResult<totp_rs::Totp> {
    totp_rs::Builder::new()
        .with_algorithm(totp_rs::Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(secret.to_vec())
        .with_issuer(Some("DumbMail"))
        .with_account_name(email.replace(':', ""))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("totp: {e:?}")))
}

fn now_secs() -> u64 {
    Utc::now().timestamp().max(0) as u64
}

fn recovery_hash(user_id: Uuid, code: &str) -> String {
    auth::sha256_hex(&format!("recovery:{user_id}:{code}"))
}

fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Ten new one-time recovery codes (old ones stop working). Only their hashes are kept.
async fn new_recovery_codes(state: &AppState, user_id: Uuid) -> AppResult<Vec<String>> {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut codes = Vec::new();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = $1").bind(user_id).execute(&mut *tx).await?;
    for _ in 0..RECOVERY_CODES {
        let raw: String = {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            (0..10).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
        };
        sqlx::query("INSERT INTO recovery_codes (id, user_id, code_hash) VALUES ($1, $2, $3)").bind(Uuid::now_v7()).bind(user_id).bind(recovery_hash(user_id, &raw)).execute(&mut *tx).await?;
        codes.push(format!("{}-{}", &raw[..5], &raw[5..]));
    }
    tx.commit().await?;
    Ok(codes)
}

/// Check a six-digit code from the app, or a recovery code (used up on success). Each code works
/// once: the 30-second step it was accepted for is remembered.
pub async fn check_code(state: &AppState, user_id: Uuid, code: &str) -> AppResult<bool> {
    let code = code.trim();
    if code.is_empty() || code.len() > 40 {
        return Ok(false);
    }
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()) {
        let row: Option<(Option<String>, String)> = sqlx::query_as("SELECT totp_secret_enc, email FROM users WHERE id = $1").bind(user_id).fetch_optional(&state.db).await?;
        let Some((Some(enc), email)) = row else { return Ok(false) };
        let secret = decode_secret(state, &enc)?;
        let Some(step) = totp_for(&secret, &email)?.check(&digits, now_secs()) else { return Ok(false) };
        let n = sqlx::query("UPDATE users SET totp_last_step = $2 WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)")
            .bind(user_id)
            .bind(step as i64)
            .execute(&state.db)
            .await?
            .rows_affected();
        return Ok(n == 1);
    }
    let norm = normalize_code(code);
    if norm.len() != 10 {
        return Ok(false);
    }
    let n = sqlx::query("UPDATE recovery_codes SET used_at = now() WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL")
        .bind(user_id)
        .bind(recovery_hash(user_id, &norm))
        .execute(&state.db)
        .await?
        .rows_affected();
    Ok(n == 1)
}

fn decode_secret(state: &AppState, enc: &str) -> AppResult<Vec<u8>> {
    let b32 = crate::crypto::decrypt(&state.cfg.secret_key, enc)?;
    base32_decode(&b32).ok_or_else(|| AppError::Internal(anyhow::anyhow!("stored two-factor secret is unreadable")))
}

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = B32.iter().position(|&x| x == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

fn need_user(ctx: &Ctx) -> AppResult<Uuid> {
    ctx.user_id.ok_or_else(|| AppError::Forbidden("sign in to the dashboard to do this".into()))
}

/// Codes: 6 wrong ones per login per 10 minutes, and at most 60 tries per address. Every try
/// counts up front; a right code gives its try back.
async fn code_attempt(state: &AppState, ctx: &Ctx, user_id: Uuid) -> AppResult<()> {
    ratelimit::hit(state, &format!("2fa:user:{user_id}"), 6, 600).await?;
    ratelimit::hit_ip(state, "2fa", ctx.ip.as_deref(), 60, 600).await
}

async fn code_ok(state: &AppState, user_id: Uuid) -> AppResult<()> {
    ratelimit::refund(state, &format!("2fa:user:{user_id}"), 600).await
}

#[derive(Deserialize)]
pub struct CodeBody {
    pub code: String,
}

/// `POST /api/auth/two-factor`: finish signing in with a code.
pub async fn verify_session_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CodeBody>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    let sid = ctx.session_id.ok_or(AppError::Unauthorized)?;
    code_attempt(&state, &ctx, uid).await?;
    if !check_code(&state, uid, &b.code).await? {
        audit(&state, &ctx, "login.two_factor_failed", None, json!({})).await;
        return Err(AppError::bad("that code didn't work; check the time on your phone or use a recovery code"));
    }
    code_ok(&state, uid).await?;
    sqlx::query("UPDATE sessions SET mfa_ok = true, reauth_at = now() WHERE id = $1").bind(sid).execute(&state.db).await?;
    audit(&state, &ctx, "login.two_factor", None, json!({})).await;
    let fresh = reload(&state, &ctx).await?;
    Ok(Json(me_json(&state, &fresh).await?))
}

/// Re-read a session after changing it.
async fn reload(state: &AppState, ctx: &Ctx) -> AppResult<Ctx> {
    match ctx.session_id {
        Some(sid) => ctx_for_session_id(state, sid, &ReqMeta { ip: ctx.ip.clone(), user_agent: ctx.user_agent.clone() }).await,
        None => Ok(ctx.clone()),
    }
}

/// `GET /api/me/two-factor`
pub async fn status_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    let (enabled_at, pending): (Option<DateTime<Utc>>, bool) = sqlx::query_as("SELECT totp_enabled_at, totp_pending_enc IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM recovery_codes WHERE user_id = $1 AND used_at IS NULL").bind(uid).fetch_one(&state.db).await?;
    let required_by: Vec<String> = sqlx::query_scalar("SELECT a.name FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = $1 AND a.require_two_factor ORDER BY a.name LIMIT 50")
        .bind(uid)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(json!({"enabled": enabled_at.is_some(), "enabled_at": enabled_at, "setup_started": pending, "recovery_codes_left": left, "required_by": required_by})))
}

/// A single sign-on session is trusted for its own account only, so it may add a factor only to a
/// login that has no other way in (no password, no Google): otherwise it could lock the person
/// out of their other accounts.
async fn may_change_factors(state: &AppState, ctx: &Ctx, uid: Uuid) -> AppResult<()> {
    if !ctx.sso_session {
        return Ok(());
    }
    let other_way_in: bool = sqlx::query_scalar("SELECT password_hash IS NOT NULL OR google_sub IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    if other_way_in {
        return Err(AppError::Forbidden("set up two-factor login after signing in with your password or Google, then come back through single sign-on".into()));
    }
    Ok(())
}

/// `POST /api/me/two-factor/setup`: a new secret to scan. Nothing changes until it's confirmed.
pub async fn setup_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    may_change_factors(&state, &ctx, uid).await?;
    let (email, on): (String, bool) = sqlx::query_as("SELECT email, totp_enabled_at IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    if on {
        return Err(AppError::Conflict("two-factor login is already on; turn it off first to move it to a new device".into()));
    }
    let mut secret = [0u8; 20];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut secret);
    let b32 = base32_encode(&secret);
    sqlx::query("UPDATE users SET totp_pending_enc = $2 WHERE id = $1").bind(uid).bind(crate::crypto::encrypt(&state.cfg.secret_key, &b32)?).execute(&state.db).await?;
    let url = totp_for(&secret, &email)?.to_url().map_err(|e| AppError::Internal(anyhow::anyhow!("totp url: {e:?}")))?;
    let qr = qrcode::QrCode::new(url.as_bytes()).map_err(|e| AppError::Internal(anyhow::anyhow!("qr: {e}")))?;
    let svg = qr.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build();
    let data_uri = format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg));
    Ok(Json(json!({"secret": b32, "otpauth_url": url, "qr_code": data_uri})))
}

/// `POST /api/me/two-factor/enable`: confirm the new secret with a code. Returns recovery codes once.
pub async fn enable_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CodeBody>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    may_change_factors(&state, &ctx, uid).await?;
    code_attempt(&state, &ctx, uid).await?;
    let (pending, email, on): (Option<String>, String, bool) =
        sqlx::query_as("SELECT totp_pending_enc, email, totp_enabled_at IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    if on {
        return Err(AppError::Conflict("two-factor login is already on".into()));
    }
    let pending = pending.ok_or_else(|| AppError::bad("start the setup first"))?;
    let secret = decode_secret(&state, &pending)?;
    let digits: String = b.code.chars().filter(|c| c.is_ascii_digit()).collect();
    let Some(step) = totp_for(&secret, &email)?.check(&digits, now_secs()) else {
        return Err(AppError::bad("that code didn't work; scan the QR code again and enter the newest code"));
    };
    let n = sqlx::query(
        "UPDATE users SET totp_secret_enc = totp_pending_enc, totp_pending_enc = NULL, totp_enabled_at = now(), totp_last_step = $2
         WHERE id = $1 AND totp_enabled_at IS NULL AND totp_pending_enc = $3",
    )
    .bind(uid)
    .bind(step as i64)
    .bind(&pending)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(AppError::Conflict("the setup changed in another window; start again".into()));
    }
    code_ok(&state, uid).await?;
    let codes = new_recovery_codes(&state, uid).await?;
    // Setting up a factor finishes a sign-in that was waiting for one, but it doesn't confirm
    // it's you: nothing already known was checked (see `reauth_h`).
    if let Some(sid) = ctx.session_id {
        sqlx::query("UPDATE sessions SET mfa_ok = true WHERE id = $1").bind(sid).execute(&state.db).await?;
    }
    // Password sessions elsewhere haven't entered a code; they sign in again with one.
    let signed_out = revoke_unverified_sessions(&state, uid, ctx.session_id).await?;
    audit(&state, &ctx, "two_factor.enable", None, json!({"sessions_signed_out": signed_out})).await;
    Ok(Json(json!({"enabled": true, "recovery_codes": codes})))
}

/// `POST /api/me/two-factor/disable`
pub async fn disable_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CodeBody>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    code_attempt(&state, &ctx, uid).await?;
    let required: Option<String> = sqlx::query_scalar("SELECT a.name FROM memberships m JOIN accounts a ON a.id = m.account_id WHERE m.user_id = $1 AND a.require_two_factor LIMIT 1")
        .bind(uid)
        .fetch_optional(&state.db)
        .await?;
    if let Some(name) = required {
        return Err(AppError::Conflict(format!("{} requires two-factor login for all members, so it can't be turned off", mail::plain(&name, 80))));
    }
    if !check_code(&state, uid, &b.code).await? {
        return Err(AppError::bad("that code didn't work"));
    }
    code_ok(&state, uid).await?;
    // Sign out half-finished sign-ins first: with the factor gone they'd have nothing left to do.
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET totp_secret_enc = NULL, totp_pending_enc = NULL, totp_enabled_at = NULL, totp_last_step = NULL WHERE id = $1").bind(uid).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = $1").bind(uid).execute(&mut *tx).await?;
    let signed_out = sqlx::query(
        "DELETE FROM sessions s USING accounts a WHERE s.user_id = $1 AND a.id = s.account_id AND NOT s.mfa_ok AND s.id IS DISTINCT FROM $2
           AND (s.auth_method = 'password' OR a.require_two_factor)",
    )
    .bind(uid)
    .bind(ctx.session_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    audit(&state, &ctx, "two_factor.disable", None, json!({"sessions_signed_out": signed_out})).await;
    Ok(Json(json!({"enabled": false})))
}

/// `POST /api/me/two-factor/recovery-codes`: replace the recovery codes.
pub async fn recovery_codes_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CodeBody>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    code_attempt(&state, &ctx, uid).await?;
    if !check_code(&state, uid, &b.code).await? {
        return Err(AppError::bad("that code didn't work"));
    }
    code_ok(&state, uid).await?;
    let codes = new_recovery_codes(&state, uid).await?;
    let signed_out = revoke_unverified_sessions(&state, uid, ctx.session_id).await?;
    audit(&state, &ctx, "two_factor.recovery_codes_reset", None, json!({"sessions_signed_out": signed_out})).await;
    Ok(Json(json!({"recovery_codes": codes})))
}

// ---- Confirming it's you ----

#[derive(Deserialize)]
pub struct ReauthBody {
    pub password: Option<String>,
    pub code: Option<String>,
}

/// `POST /api/me/reauth`: re-enter your password or a two-factor code. For the next
/// `access::FRESH_SIGN_IN_MINUTES` minutes this session may make, remove or demote owners.
///
/// Only something that was already there counts: the current password, or a code from a factor
/// turned on before this session last confirmed it's you (signing in with a code counts). A
/// factor this session just set up proves nothing, so its codes don't count until the person
/// signs in again. People who sign in with Google can confirm with Google instead
/// (`GET /api/auth/google?reauth=1`).
pub async fn reauth_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ReauthBody>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    let sid = ctx.session_id.ok_or(AppError::Unauthorized)?;
    code_attempt(&state, &ctx, uid).await?;
    let ok = match (b.password.as_deref().filter(|p| !p.is_empty()), b.code.as_deref().filter(|c| !c.trim().is_empty())) {
        (Some(pw), _) => {
            let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
            match hash {
                Some(h) => auth::verify_password(pw, &h),
                None => return Err(AppError::bad("your login has no password; enter a two-factor code instead")),
            }
        }
        (_, Some(code)) => {
            let enabled_at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT totp_enabled_at FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
            let older = matches!((enabled_at, ctx.reauth_at), (Some(on), Some(last)) if on <= last);
            if !older {
                return Err(AppError::bad(
                    "two-factor login was turned on after this session last confirmed it's you, so its codes don't count here yet; use your password (or Continue with Google), or sign out and back in with a code",
                ));
            }
            check_code(&state, uid, code).await?
        }
        _ => return Err(AppError::bad("enter your password or a code from your authenticator app")),
    };
    if !ok {
        audit(&state, &ctx, "login.reauth_failed", None, json!({})).await;
        return Err(AppError::bad("that didn't match; try again"));
    }
    code_ok(&state, uid).await?;
    sqlx::query("UPDATE sessions SET reauth_at = now() WHERE id = $1").bind(sid).execute(&state.db).await?;
    audit(&state, &ctx, "login.reauth", None, json!({})).await;
    Ok(Json(json!({"ok": true, "valid_minutes": access::FRESH_SIGN_IN_MINUTES})))
}

// ---- Sessions ----

/// A short device name from a user agent ("Chrome on macOS").
pub fn device(ua: Option<&str>) -> String {
    let Some(ua) = ua.filter(|u| !u.is_empty()) else { return "Unknown device".into() };
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("OPR/") || ua.contains("Opera") {
        "Opera"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.contains("curl/") {
        "curl"
    } else {
        "Browser"
    };
    let os = if ua.contains("iPhone") || ua.contains("iPad") {
        "iOS"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("CrOS") {
        "ChromeOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() { browser.into() } else { format!("{browser} on {os}") }
}

/// `GET /api/me/sessions`
pub async fn sessions_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    // Older sessions get their id here, so each one can be signed out.
    sqlx::query("UPDATE sessions SET id = gen_random_uuid() WHERE user_id = $1 AND id IS NULL").bind(uid).execute(&state.db).await?;
    let rows: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>, Option<String>, Option<String>, String, String)> = sqlx::query_as(
        "SELECT s.id, s.created_at, s.last_seen_at, s.ip, s.user_agent, s.auth_method, a.name FROM sessions s JOIN accounts a ON a.id = s.account_id
         WHERE s.user_id = $1 AND s.expires_at > now() ORDER BY s.last_seen_at DESC LIMIT 100",
    )
    .bind(uid)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, created, seen, ip, ua, method, account)| {
            json!({"id": id, "created_at": created, "last_seen_at": seen, "ip": ip, "device": device(ua.as_deref()), "user_agent": ua, "sign_in_method": method, "account": account, "current": Some(id) == ctx.session_id})
        })
        .collect();
    Ok(Json(json!({"data": data})))
}

/// `DELETE /api/me/sessions/{id}`
pub async fn revoke_session_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    let n = sqlx::query("DELETE FROM sessions WHERE id = $1 AND user_id = $2").bind(id).bind(uid).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("session"));
    }
    audit(&state, &ctx, "session.revoke", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true, "current": Some(id) == ctx.session_id})))
}

/// `POST /api/me/sessions/revoke-others`
pub async fn revoke_others_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    let n = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id IS DISTINCT FROM $2").bind(uid).bind(ctx.session_id).execute(&state.db).await?.rows_affected();
    audit(&state, &ctx, "session.revoke_others", None, json!({"sessions": n})).await;
    Ok(Json(json!({"ok": true, "revoked": n})))
}

// ---- Passwords ----

/// The rule for a new password: 8 or more characters (as sign-up has always counted them).
pub fn check_new_password(p: &str) -> AppResult<()> {
    if p.len() < 8 {
        return Err(AppError::bad("use a password of at least 8 characters"));
    }
    if p.len() > MAX_NEW_PASSWORD {
        return Err(AppError::bad("use a shorter password"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct PasswordBody {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /api/me/password`: change it and sign out every other session.
pub async fn change_password_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<PasswordBody>) -> AppResult<Json<Value>> {
    let uid = need_user(&ctx)?;
    ratelimit::hit(&state, &format!("password:user:{uid}"), 10, 900).await?;
    check_new_password(&b.new_password)?;
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    let Some(hash) = hash else {
        return Err(AppError::bad("your login uses Google and has no password; use Forgot password on the sign-in page to add one"));
    };
    if !auth::verify_password(&b.current_password, &hash) {
        audit(&state, &ctx, "password.change_failed", None, json!({})).await;
        return Err(AppError::bad("your current password isn't right"));
    }
    let new_hash = auth::hash_password(&b.new_password)?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2, credential_version = credential_version + 1 WHERE id = $1").bind(uid).bind(new_hash).execute(&mut *tx).await?;
    let n = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id IS DISTINCT FROM $2").bind(uid).bind(ctx.session_id).execute(&mut *tx).await?.rows_affected();
    sqlx::query("DELETE FROM password_resets WHERE user_id = $1").bind(uid).execute(&mut *tx).await?;
    sqlx::query("UPDATE sessions SET reauth_at = now() WHERE id = $1").bind(ctx.session_id).execute(&mut *tx).await?;
    tx.commit().await?;
    audit(&state, &ctx, "password.change", None, json!({"other_sessions_revoked": n})).await;
    Ok(Json(json!({"ok": true, "other_sessions_revoked": n})))
}

#[derive(Deserialize)]
pub struct ForgotBody {
    pub email: String,
}

/// `POST /api/auth/password/forgot`: always the same answer; the email (if there's a login) goes
/// out in the background so the response time doesn't tell either.
pub async fn forgot_h(State(state): State<AppState>, headers: HeaderMap, ApiJson(b): ApiJson<ForgotBody>) -> AppResult<Json<Value>> {
    let meta = ReqMeta::from_headers(&headers);
    ratelimit::hit_ip(&state, "forgot", meta.ip.as_deref(), 10, 3600).await?;
    let email = b.email.trim().to_lowercase();
    if email.len() <= 254 && email.contains('@') && ratelimit::hit(&state, &format!("forgot:email:{email}"), 3, 3600).await.is_ok() {
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = send_reset(&st, &email, &meta).await {
                tracing::error!(error = %e, "password reset email failed");
            }
        });
    }
    Ok(Json(json!({"ok": true, "message": "If there's a DumbMail login for that email, we sent it a link to set a new password."})))
}

async fn send_reset(state: &AppState, email: &str, meta: &ReqMeta) -> AppResult<()> {
    let Some(uid) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1").bind(email).fetch_optional(&state.db).await? else { return Ok(()) };
    let token = auth::random_token(32);
    sqlx::query("INSERT INTO password_resets (token_hash, user_id, expires_at) VALUES ($1, $2, now() + interval '1 hour')").bind(auth::sha256_hex(&token)).bind(uid).execute(&state.db).await?;
    let url = format!("{}/reset-password?token={token}", state.cfg.public_url());
    mail::send(
        state,
        email,
        "Set a new DumbMail password",
        &["Someone asked to set a new password for your DumbMail login. If it was you, use the button below. The link works for one hour.".into()],
        ("Set a new password", &url),
        "If you didn't ask for this, ignore this email; your password stays the same.",
    )
    .await;
    for account_id in user_accounts(state, uid).await? {
        record(state, account_id, &meta.actor(Some(uid)), "password.reset_requested", None, json!({})).await;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct ResetBody {
    pub token: String,
    pub password: String,
}

/// `POST /api/auth/password/reset`: set the new password and sign out everywhere.
pub async fn reset_h(State(state): State<AppState>, headers: HeaderMap, ApiJson(b): ApiJson<ResetBody>) -> AppResult<Json<Value>> {
    let meta = ReqMeta::from_headers(&headers);
    ratelimit::hit_ip(&state, "reset", meta.ip.as_deref(), 20, 3600).await?;
    check_new_password(&b.password)?;
    let invalid = || AppError::bad("this reset link has expired or was already used; ask for a new one");
    if b.token.len() > 200 {
        return Err(invalid());
    }
    let mut tx = state.db.begin().await?;
    let uid: Option<Uuid> = sqlx::query_scalar("UPDATE password_resets SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() RETURNING user_id")
        .bind(auth::sha256_hex(b.token.trim()))
        .fetch_optional(&mut *tx)
        .await?;
    let uid = uid.ok_or_else(invalid)?;
    // The new version makes any sign-in that checked the old password before this commits fail
    // to get a session (see `start_session_with`).
    sqlx::query("UPDATE users SET password_hash = $2, credential_version = credential_version + 1 WHERE id = $1").bind(uid).bind(auth::hash_password(&b.password)?).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(uid).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM password_resets WHERE user_id = $1 AND used_at IS NULL").bind(uid).execute(&mut *tx).await?;
    tx.commit().await?;
    for account_id in user_accounts(&state, uid).await? {
        record(&state, account_id, &meta.actor(Some(uid)), "password.reset", None, json!({})).await;
    }
    Ok(Json(json!({"ok": true})))
}

/// Sign-up's session, and its rate limit.
pub async fn signup_guard(state: &AppState, meta: &ReqMeta) -> AppResult<()> {
    ratelimit::hit_ip(state, "signup", meta.ip.as_deref(), 10, 3600).await
}

/// Response body for a fresh session token (sign-up, sign-in, accepting an invite).
pub async fn me_for_token(state: &AppState, token: &str, meta: &ReqMeta) -> AppResult<Value> {
    let ctx = session_ctx(state, token, meta).await?;
    me_json(state, &ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trip() {
        let data = [1u8, 2, 3, 250, 99, 0, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20];
        let e = base32_encode(&data);
        assert_eq!(e.len(), 32);
        assert_eq!(base32_decode(&e).unwrap(), data);
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn devices() {
        assert_eq!(device(Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.1 Safari/605.1.15")), "Safari on macOS");
        assert_eq!(device(Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36")), "Chrome on Windows");
        assert_eq!(device(None), "Unknown device");
    }

    #[test]
    fn constant_time_compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
    }
}

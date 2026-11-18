//! Signing shoppers in: a 6-digit code emailed to them, or a signed link from an email the store
//! sent. Never from the website snippet's identity, a profile id or an email in the request.
//!
//! **Codes.** Valid for 10 minutes, at most 5 tries (only the newest code for an address counts, so
//! asking again doesn't add tries to an older one). The code is derived from the row's id with the
//! server's secret, and only a keyed hash of it is stored, with the address also kept only as a
//! keyed hash. Every request writes a row, known address or not, and the answer is the same either
//! way. Requests are limited per address, per visitor (IP) and per account, counted and written
//! under database locks in one transaction, so the limits hold across processes and restarts. The
//! email is sent by a job (the request only writes the row and the job, together), so a known
//! address doesn't answer slower than an unknown one.
//!
//! **Sessions.** An httpOnly, SameSite=Lax cookie on the hub's own host and path, good for 30 days.
//! A session is good only while the person's email is still the one it signed in with, so it ends
//! when the email changes; erasing the person deletes it; signing out revokes it.
//! [`end_sessions`] ends a person's sessions (for merges).

use axum::http::HeaderMap;
use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile};
use crate::state::AppState;

pub const SEND_CODE_JOB: &str = "hub_send_code";
pub const COOKIE: &str = "dm_hub";
pub const CODE_MINUTES: i32 = 10;
pub const MAX_ATTEMPTS: i32 = 5;
pub const SESSION_DAYS: i64 = 30;
/// How long a signed link in an email signs the person in.
pub const LINK_HOURS: i64 = 72;
/// Code requests for one address at one store, per hour.
pub const PER_ADDRESS_HOUR: i64 = 5;
/// Code requests from one visitor (IP address), across stores, per hour.
pub const PER_IP_HOUR: i64 = 20;
/// Code requests to one store, per hour.
pub const PER_ACCOUNT_HOUR: i64 = 500;

/// A signed-in shopper: who the session says it is, checked against the database on this request.
/// Everything the hub shows is read for `profile_id` and `account.id` from here, never from the
/// request.
#[derive(Debug, Clone)]
pub struct Shopper {
    pub account: Account,
    pub profile_id: Uuid,
    pub email: String,
    pub session_id: Uuid,
}

fn mac(state: &AppState, what: &str) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(state.cfg.secret_key.as_bytes()).expect("hmac key");
    m.update(what.as_bytes());
    m.finalize().into_bytes().into()
}

fn mac_hex(state: &AppState, what: &str) -> String {
    hex::encode(mac(state, what))
}

/// A keyed hash of an address for one store (rows never hold the address itself).
pub fn email_hash(state: &AppState, account_id: Uuid, email: &str) -> String {
    mac_hex(state, &format!("hub-email:{}:{}", account_id.simple(), email.trim().to_lowercase()))
}

fn ip_hash(state: &AppState, ip: &str) -> String {
    mac_hex(state, &format!("hub-ip:{ip}"))[..32].to_string()
}

/// The code for a code row: derived from its id and the server's secret, never stored.
fn code_for(state: &AppState, id: Uuid) -> String {
    let b = mac(state, &format!("hub-code:{}", id.simple()));
    format!("{:06}", u32::from_be_bytes([b[0], b[1], b[2], b[3]]) % 1_000_000)
}

fn code_hash(state: &AppState, id: Uuid, code: &str) -> String {
    mac_hex(state, &format!("hub-code-hash:{}:{code}", id.simple()))
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The hub's address for this store: on its branded link domain when that's live, else on the
/// shared link host.
pub fn hub_url(state: &AppState, account: &Account) -> String {
    format!("{}/hub/{}", crate::domains::link_base(state, account), account.public_key)
}

/// The host the hub is served on (lowercase, no port).
pub fn hub_host(state: &AppState, account: &Account) -> String {
    url::Url::parse(&hub_url(state, account)).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default()
}

/// Where the hub's cookie lives: only the hub's own path on its host.
pub fn cookie_path(account: &Account) -> String {
    format!("/hub/{}", account.public_key)
}

pub fn session_cookie(state: &AppState, account: &Account, token: String) -> Cookie<'static> {
    Cookie::build((COOKIE, token))
        .path(cookie_path(account))
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(SESSION_DAYS))
        .build()
}

pub fn removal_cookie(state: &AppState, account: &Account) -> Cookie<'static> {
    Cookie::build((COOKIE, "")).path(cookie_path(account)).http_only(true).secure(state.cfg.secure_cookies).same_site(SameSite::Lax).max_age(time::Duration::ZERO).build()
}

// ---------- Signed links ----------

fn email_tag(state: &AppState, email: &str) -> String {
    mac_hex(state, &format!("hub-link-email:{}", email.trim().to_lowercase()))[..8].to_string()
}

/// A link token for this person, bound to the store, their current email and an expiry
/// ([`LINK_HOURS`]).
pub fn link_token(state: &AppState, account_id: Uuid, profile_id: Uuid, email: &str, now: DateTime<Utc>) -> String {
    let exp = now.timestamp() / 3600 + LINK_HOURS;
    crate::auth::sign(&state.cfg.secret_key, &format!("h{}{}{exp:x}.{}", profile_id.simple(), &account_id.simple().to_string()[..8], email_tag(state, email)))
}

/// The person a link token names, when it's genuine, for this store, not expired, and their email
/// is still the one it was made for.
pub async fn profile_from_link(state: &AppState, account_id: Uuid, token: &str) -> AppResult<Option<(Uuid, String)>> {
    let Some(payload) = crate::auth::verify(&state.cfg.secret_key, token) else { return Ok(None) };
    let Some(rest) = payload.strip_prefix('h') else { return Ok(None) };
    let Some((head, tag)) = rest.split_once('.') else { return Ok(None) };
    if head.len() < 41 {
        return Ok(None);
    }
    let (pid, tail) = head.split_at(32);
    let (acct, exp) = tail.split_at(8);
    let Ok(exp) = i64::from_str_radix(exp, 16) else { return Ok(None) };
    if acct != &account_id.simple().to_string()[..8] || exp < Utc::now().timestamp() / 3600 {
        return Ok(None);
    }
    let Ok(pid) = Uuid::parse_str(pid) else { return Ok(None) };
    let email: Option<Option<String>> = sqlx::query_scalar("SELECT email FROM profiles WHERE id = $1 AND account_id = $2").bind(pid).bind(account_id).fetch_optional(&state.db).await?;
    Ok(match email.flatten() {
        Some(e) if same(&email_tag(state, &e), tag) => Some((pid, e)),
        _ => None,
    })
}

/// The hub's address with a signed sign-in link for this person, for an email the store sends
/// (`{{ hub_url }}`). None when they have no email.
pub fn hub_url_for(state: &AppState, account: &Account, profile: &Profile) -> Option<String> {
    let email = profile.email.as_deref().filter(|e| !e.is_empty())?;
    Some(format!("{}?t={}", hub_url(state, account), link_token(state, account.id, profile.id, email, Utc::now())))
}

/// Fill `hub_url` for a message's template: signed for the recipient on real sends; the plain
/// address on tests and previews (a test can go to a teammate, who must not get the person's hub).
pub fn fill(state: &AppState, account: &Account, profile: &Profile, is_test: bool, ctx: &mut Value) {
    let url = if is_test { Some(hub_url(state, account)) } else { hub_url_for(state, account, profile).or_else(|| Some(hub_url(state, account))) };
    if let (Some(o), Some(u)) = (ctx.as_object_mut(), url) {
        o.insert("hub_url".into(), json!(u));
    }
}

// ---------- Codes ----------

pub enum Requested {
    /// The same answer whether or not the address is known.
    Sent,
    TooMany,
}

/// Whether the store may email a code now. The send itself also refuses while sending is paused.
/// Once sender screening is on `main`, an account it has blocked sends no code here either.
pub fn may_send_code(account: &Account) -> bool {
    account.closed_at.is_none() && account.sending_paused_at.is_none()
}

/// Ask for a code for `email`. Writes the row (and its send job) whether or not the address is
/// known, under the limits.
pub async fn request_code(state: &AppState, account: &Account, email: &str, ip: Option<&str>) -> AppResult<Requested> {
    let email = crate::models::normalize_email(email).ok_or_else(|| AppError::bad("Enter a valid email address"))?;
    let eh = email_hash(state, account.id, &email);
    let ih = ip.filter(|s| !s.is_empty()).map(|ip| ip_hash(state, ip));
    let mut tx = state.db.begin().await?;
    // Always in this order (the store, then the visitor), so two requests never wait on each other.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("hub-code:{}", account.id)).execute(&mut *tx).await?;
    if let Some(h) = &ih {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("hub-code-ip:{h}")).execute(&mut *tx).await?;
    }
    let profile_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND email = $2").bind(account.id).bind(&email).fetch_optional(&mut *tx).await?;
    let id = Uuid::now_v7();
    let hash = profile_id.filter(|_| may_send_code(account)).map(|_| code_hash(state, id, &code_for(state, id)));
    // The limits are checked in the statement that writes the row, under the locks above.
    let written: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO hub_codes (id, account_id, profile_id, email_hash, ip_hash, code_hash, expires_at)
         SELECT $1, $2, $3, $4, $5, $6, now() + make_interval(mins => $7)
         WHERE (SELECT count(*) FROM hub_codes WHERE account_id = $2 AND email_hash = $4 AND created_at > now() - interval '1 hour') < $8
           AND ($5::text IS NULL OR (SELECT count(*) FROM hub_codes WHERE ip_hash = $5 AND created_at > now() - interval '1 hour') < $9)
           AND (SELECT count(*) FROM hub_codes WHERE account_id = $2 AND created_at > now() - interval '1 hour') < $10
         RETURNING id",
    )
    .bind(id)
    .bind(account.id)
    .bind(profile_id)
    .bind(&eh)
    .bind(&ih)
    .bind(&hash)
    .bind(CODE_MINUTES)
    .bind(PER_ADDRESS_HOUR)
    .bind(PER_IP_HOUR)
    .bind(PER_ACCOUNT_HOUR)
    .fetch_optional(&mut *tx)
    .await?;
    if written.is_none() {
        return Ok(Requested::TooMany);
    }
    crate::jobs::enqueue(&mut *tx, SEND_CODE_JOB, json!({"id": id}), None).await?;
    tx.commit().await?;
    Ok(Requested::Sent)
}

/// Check a code. On success the code is used up and the person it was for is returned.
pub async fn verify_code(state: &AppState, account: &Account, email: &str, code: &str) -> AppResult<Option<(Uuid, String)>> {
    let Some(email) = crate::models::normalize_email(email) else { return Ok(None) };
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Ok(None);
    }
    let eh = email_hash(state, account.id, &email);
    // One try at the newest code for this address, counted in the same statement that checks the
    // cap; the sixth try is refused even with the right code.
    let row: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "UPDATE hub_codes SET attempts = attempts + 1
         WHERE id = (SELECT id FROM hub_codes WHERE account_id = $1 AND email_hash = $2 ORDER BY created_at DESC LIMIT 1)
           AND attempts < $3 AND used_at IS NULL AND expires_at > now()
         RETURNING id, code_hash",
    )
    .bind(account.id)
    .bind(&eh)
    .bind(MAX_ATTEMPTS)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, Some(stored))) = row else { return Ok(None) };
    if !same(&stored, &code_hash(state, id, &code)) {
        return Ok(None);
    }
    let used: Option<Option<Uuid>> = sqlx::query_scalar("UPDATE hub_codes SET used_at = now() WHERE id = $1 AND used_at IS NULL RETURNING profile_id").bind(id).fetch_optional(&state.db).await?;
    let Some(Some(profile_id)) = used else { return Ok(None) };
    // Still this address's person (not changed since the code was sent).
    let current: Option<Option<String>> = sqlx::query_scalar("SELECT email FROM profiles WHERE id = $1 AND account_id = $2").bind(profile_id).bind(account.id).fetch_optional(&state.db).await?;
    Ok(match current.flatten() {
        Some(e) if e == email => Some((profile_id, e)),
        _ => None,
    })
}

/// The code email job: claim the row (so a retried job never sends twice), then send.
pub async fn run_send_code(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let claimed: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "UPDATE hub_codes SET sent_at = now()
         WHERE id = $1 AND sent_at IS NULL AND used_at IS NULL AND expires_at > now() AND code_hash IS NOT NULL AND profile_id IS NOT NULL
         RETURNING account_id, profile_id, email_hash",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, profile_id, eh)) = claimed else { return Ok(()) };
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if !may_send_code(&account) {
        return Ok(());
    }
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT email, email_suppressed_reason FROM profiles WHERE id = $1 AND account_id = $2").bind(profile_id).bind(account_id).fetch_optional(&state.db).await?;
    let Some((Some(email), None)) = row else { return Ok(()) };
    if !same(&email_hash(state, account_id, &email), &eh) {
        return Ok(());
    }
    let message = code_email(&account, &code_for(state, id));
    match crate::optin::deliver(state, &account, profile_id, &email, message).await {
        Ok(Ok(())) => {}
        Ok(Err(why)) => tracing::warn!(account = %account_id, reason = %why, "hub sign-in code not sent"),
        Err(e) => tracing::warn!(account = %account_id, error = %e.public_message(), "hub sign-in code not sent"),
    }
    Ok(())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Subject, HTML and text of the code email. The subject never holds the code (it's kept with the
/// message record).
pub fn code_email(account: &Account, code: &str) -> (String, String, String) {
    let color = crate::render::safe_color(&account.brand_color, "#1f6f5c");
    let name = esc(&account.name);
    let subject = format!("Your sign-in code for {}", account.name);
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"></head><body style="margin:0;background:#f4f4f5;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#111827">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:32px 16px">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="max-width:440px;background:#fff;border-radius:12px"><tr><td style="padding:32px">
<p style="margin:0 0 8px;font-size:15px;color:#4b5563">{name}</p>
<h1 style="margin:0 0 16px;font-size:22px">Your sign-in code</h1>
<p style="margin:0 0 20px;font-size:32px;letter-spacing:6px;font-weight:700;color:{color}">{code}</p>
<p style="margin:0 0 8px;font-size:15px;line-height:1.5;color:#4b5563">Enter it on the account page to see your orders and preferences. It works for {CODE_MINUTES} minutes.</p>
<p style="margin:0;font-size:13px;line-height:1.5;color:#6b7280">If you didn't ask for this, you can ignore this email. No one can sign in without the code.</p>
</td></tr></table></td></tr></table></body></html>"#
    );
    let text = format!("Your sign-in code for {}: {code}\n\nIt works for {CODE_MINUTES} minutes. If you didn't ask for this, you can ignore this email.\n", account.name);
    (subject, html, text)
}

// ---------- Sessions ----------

/// Start a session for a person whose email was just proven (a code or a signed link). Returns the
/// cookie's token.
pub async fn start(db: &PgPool, account_id: Uuid, profile_id: Uuid, email: &str, method: &str) -> AppResult<String> {
    let token = crate::auth::random_token(32);
    sqlx::query(
        "INSERT INTO hub_sessions (id, account_id, profile_id, token_hash, email, method, expires_at)
         SELECT $1, $2, p.id, $4, $5, $6, now() + make_interval(days => $7) FROM profiles p WHERE p.id = $3 AND p.account_id = $2",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(profile_id)
    .bind(crate::auth::sha256_hex(&token))
    .bind(email)
    .bind(method)
    .bind(SESSION_DAYS as i32)
    .execute(db)
    .await?;
    Ok(token)
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let jar = axum_extra::extract::CookieJar::from_headers(headers);
    jar.get(COOKIE).map(|c| c.value().to_string()).filter(|t| !t.is_empty() && t.len() <= 100)
}

/// Who is signed in to this store's hub on this request, if anyone. The one check every hub page,
/// form and later addition uses: the session must be this store's, live, and its person's email
/// still the one it signed in with.
pub async fn current(state: &AppState, headers: &HeaderMap, account: &Account) -> AppResult<Option<Shopper>> {
    let Some(token) = cookie_token(headers) else { return Ok(None) };
    let row: Option<(Uuid, Uuid, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT s.id, s.profile_id, s.email, s.last_seen_at FROM hub_sessions s JOIN profiles p ON p.id = s.profile_id AND p.account_id = s.account_id
         WHERE s.token_hash = $1 AND s.account_id = $2 AND s.revoked_at IS NULL AND s.expires_at > now() AND p.email = s.email",
    )
    .bind(crate::auth::sha256_hex(&token))
    .bind(account.id)
    .fetch_optional(&state.db)
    .await?;
    let Some((session_id, profile_id, email, seen)) = row else { return Ok(None) };
    if seen < Utc::now() - chrono::Duration::minutes(10) {
        sqlx::query("UPDATE hub_sessions SET last_seen_at = now() WHERE id = $1").bind(session_id).execute(&state.db).await?;
    }
    Ok(Some(Shopper { account: account.clone(), profile_id, email, session_id }))
}

/// Sign this browser out (its session can't be used again).
pub async fn sign_out(state: &AppState, headers: &HeaderMap, account: &Account) -> AppResult<()> {
    if let Some(token) = cookie_token(headers) {
        sqlx::query("UPDATE hub_sessions SET revoked_at = now() WHERE token_hash = $1 AND account_id = $2 AND revoked_at IS NULL")
            .bind(crate::auth::sha256_hex(&token))
            .bind(account.id)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

/// End every hub session of these people (a merge, or anything else that changes who they are).
pub async fn end_sessions<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid, profile_ids: &[Uuid]) -> sqlx::Result<u64> {
    Ok(sqlx::query("UPDATE hub_sessions SET revoked_at = now() WHERE account_id = $1 AND profile_id = ANY($2) AND revoked_at IS NULL")
        .bind(account_id)
        .bind(profile_ids)
        .execute(db)
        .await?
        .rows_affected())
}

/// A per-session token for the hub's forms, so another site can't post them as the shopper.
pub fn form_token(state: &AppState, session_id: Uuid) -> String {
    mac_hex(state, &format!("hub-form:{}", session_id.simple()))[..32].to_string()
}

pub fn form_token_ok(state: &AppState, session_id: Uuid, given: &str) -> bool {
    same(&form_token(state, session_id), given)
}

/// Old codes and finished sessions, a batch at a time.
pub async fn clean_up(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM hub_codes WHERE id IN (SELECT id FROM hub_codes WHERE created_at < now() - interval '2 days' LIMIT 5000)").execute(db).await?;
    sqlx::query(
        "DELETE FROM hub_sessions WHERE id IN (SELECT id FROM hub_sessions WHERE expires_at < now() - interval '1 day' OR revoked_at < now() - interval '1 day' LIMIT 5000)",
    )
    .execute(db)
    .await?;
    Ok(())
}

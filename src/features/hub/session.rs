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

/// A link token for this person, bound to the store, their current email and the moment it was
/// made (it works for [`LINK_HOURS`], once, and only if their hub generation hasn't moved since).
pub fn link_token(state: &AppState, account_id: Uuid, profile_id: Uuid, email: &str, now: DateTime<Utc>) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("h{}{}{:x}.{}", profile_id.simple(), &account_id.simple().to_string()[..8], now.timestamp(), email_tag(state, email)))
}

/// A sign-in link that checked out: who it's for and its token's hash (for using it up).
pub struct Link {
    pub profile_id: Uuid,
    pub email: String,
    token_hash: String,
}

/// The person a link token names, when it's genuine, for this store, made within the last
/// [`LINK_HOURS`], not used yet, made after the person's last hub generation change, and their
/// email is still the one it was made for. Doesn't use it up (see [`redeem_link`]).
pub async fn check_link(state: &AppState, account_id: Uuid, token: &str) -> AppResult<Option<Link>> {
    if token.len() > 300 {
        return Ok(None);
    }
    let Some(payload) = crate::auth::verify(&state.cfg.secret_key, token) else { return Ok(None) };
    let Some(rest) = payload.strip_prefix('h') else { return Ok(None) };
    let Some((head, tag)) = rest.split_once('.') else { return Ok(None) };
    if head.len() < 41 || !head.is_ascii() {
        return Ok(None);
    }
    let (pid, tail) = head.split_at(32);
    let (acct, issued) = tail.split_at(8);
    let Ok(issued) = i64::from_str_radix(issued, 16) else { return Ok(None) };
    let now = Utc::now().timestamp();
    if acct != &account_id.simple().to_string()[..8] || issued > now + 300 || issued + LINK_HOURS * 3600 < now {
        return Ok(None);
    }
    let Ok(pid) = Uuid::parse_str(pid) else { return Ok(None) };
    let token_hash = crate::auth::sha256_hex(token);
    let row: Option<(Option<String>, Option<DateTime<Utc>>, bool)> = sqlx::query_as(
        "SELECT p.email, (SELECT g.updated_at FROM hub_generations g WHERE g.profile_id = p.id),
                EXISTS (SELECT 1 FROM hub_used_links u WHERE u.token_hash = $3)
         FROM profiles p WHERE p.id = $1 AND p.account_id = $2",
    )
    .bind(pid)
    .bind(account_id)
    .bind(&token_hash)
    .fetch_optional(&state.db)
    .await?;
    let Some((Some(email), changed_at, used)) = row else { return Ok(None) };
    // Made before the person's hub generation last changed (a new address, sign-out everywhere, a merge).
    if used || changed_at.is_some_and(|c| issued <= c.timestamp()) || !same(&email_tag(state, &email), tag) {
        return Ok(None);
    }
    Ok(Some(Link { profile_id: pid, email, token_hash }))
}

/// Use a checked link up: true only for the first caller (a link works once).
pub async fn redeem_link(db: &PgPool, account_id: Uuid, link: &Link) -> AppResult<bool> {
    let fresh: Option<String> = sqlx::query_scalar("INSERT INTO hub_used_links (token_hash, account_id) VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING token_hash")
        .bind(&link.token_hash)
        .bind(account_id)
        .fetch_optional(db)
        .await?;
    Ok(fresh.is_some())
}

/// The hub's address with a signed sign-in link for this person, for an email the store sends
/// (`{{ hub_url }}`). None when they have no email.
pub fn hub_url_for(state: &AppState, account: &Account, profile: &Profile) -> Option<String> {
    let email = profile.email.as_deref().filter(|e| !e.is_empty())?;
    Some(format!("{}?t={}", hub_url(state, account), link_token(state, account.id, profile.id, email, Utc::now())))
}

/// Fill `hub_url` for an email's template: a sign-in link for the recipient on real email sends;
/// the plain address on tests (a test can go to a teammate, who must not get the person's hub).
/// Texts, WhatsApp and push never get a sign-in link (they're forwarded, previewed and kept in
/// the inbox; the link proves only the email address). Links to the hub's own host are never
/// click-tracked (`render::rewrite_links` leaves the link domain's own links alone).
pub fn fill(state: &AppState, account: &Account, profile: &Profile, channel: &str, is_test: bool, ctx: &mut Value) {
    let url = if channel == "email" && !is_test { hub_url_for(state, account, profile) } else { None }.unwrap_or_else(|| hub_url(state, account));
    if let Some(o) = ctx.as_object_mut() {
        o.insert("hub_url".into(), json!(url));
    }
}

// ---------- Generations ----------

/// End every hub session, sign-in link and waiting code of this person, for good: their hub
/// generation goes up, and everything issued before stops working. Called when their email
/// changes, on "sign out everywhere", and by merges (for both people). Takes any executor so a
/// merge can call it inside its own transaction.
pub async fn end_sessions<'e>(db: impl sqlx::PgExecutor<'e>, profile_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO hub_generations (profile_id, account_id, generation, updated_at) SELECT id, account_id, 1, now() FROM profiles WHERE id = $1
         ON CONFLICT (profile_id) DO UPDATE SET generation = hub_generations.generation + 1, updated_at = now()",
    )
    .bind(profile_id)
    .execute(db)
    .await?;
    Ok(())
}

/// The SQL for a person's current hub generation (`$pid` names the profile id).
fn generation_sql(pid: &str) -> String {
    format!("COALESCE((SELECT g.generation FROM hub_generations g WHERE g.profile_id = {pid}), 0)")
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
        &format!("INSERT INTO hub_codes (id, account_id, profile_id, email_hash, ip_hash, code_hash, expires_at, generation)
         SELECT $1, $2, $3, $4, $5, $6, now() + make_interval(mins => $7), {}
         WHERE (SELECT count(*) FROM hub_codes WHERE account_id = $2 AND email_hash = $4 AND created_at > now() - interval '1 hour') < $8
           AND ($5::text IS NULL OR (SELECT count(*) FROM hub_codes WHERE ip_hash = $5 AND created_at > now() - interval '1 hour') < $9)
           AND (SELECT count(*) FROM hub_codes WHERE account_id = $2 AND created_at > now() - interval '1 hour') < $10
         RETURNING id", generation_sql("$3")),
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
///
/// Only the newest code for the address counts. Under the lock code requests take, one statement
/// counts the try and uses the code up, re-checking that it is still the newest, unexpired and
/// under the try cap: the sixth try fails even with the right code, and two tries at once can't
/// both pass.
pub async fn verify_code(state: &AppState, account: &Account, email: &str, code: &str) -> AppResult<Option<(Uuid, String)>> {
    let Some(email) = crate::models::normalize_email(email) else { return Ok(None) };
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Ok(None);
    }
    let eh = email_hash(state, account.id, &email);
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("hub-code:{}", account.id)).execute(&mut *tx).await?;
    let newest = "(SELECT id FROM hub_codes WHERE account_id = $1 AND email_hash = $2 ORDER BY created_at DESC LIMIT 1)";
    let id: Option<Uuid> = sqlx::query_scalar(&format!("SELECT {newest}")).bind(account.id).bind(&eh).fetch_one(&mut *tx).await?;
    let Some(id) = id else { return Ok(None) };
    let row: Option<(bool, Option<Uuid>, i64)> = sqlx::query_as(&format!(
        "UPDATE hub_codes SET attempts = attempts + 1, used_at = CASE WHEN code_hash = $4 THEN now() END
         WHERE id = $3 AND id = {newest} AND attempts < $5 AND used_at IS NULL AND expires_at > now()
         RETURNING used_at IS NOT NULL, profile_id, generation"
    ))
    .bind(account.id)
    .bind(&eh)
    .bind(id)
    .bind(code_hash(state, id, &code))
    .bind(MAX_ATTEMPTS)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some((true, Some(profile_id), generation)) = row else { return Ok(None) };
    // Still this address's person, and nothing ended their hub access since the code was sent.
    let current: Option<(Option<String>, i64)> = sqlx::query_as(&format!("SELECT p.email, {} FROM profiles p WHERE p.id = $1 AND p.account_id = $2", generation_sql("p.id")))
        .bind(profile_id)
        .bind(account.id)
        .fetch_optional(&state.db)
        .await?;
    Ok(match current {
        Some((Some(e), g)) if e == email && g == generation => Some((profile_id, e)),
        _ => None,
    })
}

/// The code email job: claim the row (so a retried job never sends twice), wait for the mail
/// throttle, check everything again, then send. Like a double opt-in confirmation it isn't billed;
/// it's kept as a sent message marked transactional, so a bounce or complaint still counts.
pub async fn run_send_code(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let claimed: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE hub_codes SET sent_at = now()
         WHERE id = $1 AND sent_at IS NULL AND used_at IS NULL AND expires_at > now() AND code_hash IS NOT NULL AND profile_id IS NOT NULL
         RETURNING account_id, profile_id",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, profile_id)) = claimed else { return Ok(()) };
    crate::sending::throttle_email(state).await;
    // The last check, just before the provider: the hub is still on, this is still the newest
    // unused, unexpired code for the address and the person's generation, the store may send,
    // and the address (still the one asked for) isn't suppressed.
    let to: Option<String> = sqlx::query_scalar(&format!(
        "SELECT p.email FROM hub_codes c
           JOIN profiles p ON p.id = c.profile_id AND p.account_id = c.account_id
           JOIN accounts a ON a.id = c.account_id
           JOIN hub_settings s ON s.account_id = c.account_id
         WHERE c.id = $1 AND c.used_at IS NULL AND c.expires_at > now()
           AND c.id = (SELECT n.id FROM hub_codes n WHERE n.account_id = c.account_id AND n.email_hash = c.email_hash ORDER BY n.created_at DESC LIMIT 1)
           AND c.generation = {}
           AND s.enabled AND a.closed_at IS NULL AND a.sending_paused_at IS NULL
           AND p.email IS NOT NULL AND p.email_suppressed_reason IS NULL",
        generation_sql("p.id")
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some(to) = to else { return Ok(()) };
    let eh: String = sqlx::query_scalar("SELECT email_hash FROM hub_codes WHERE id = $1").bind(id).fetch_one(&state.db).await?;
    if !same(&email_hash(state, account_id, &to), &eh) {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if !may_send_code(&account) {
        return Ok(());
    }
    let Some(from_email) = account.from_email.clone() else { return Ok(()) };
    if state.email.name() == "ses" {
        if let Err(why) = crate::sending::check_sender_domain_name(&account, &from_email) {
            tracing::warn!(account = %account_id, reason = %why, "hub sign-in code not sent");
            return Ok(());
        }
    }
    let (subject, html, text) = code_email(&account, &code_for(state, id));
    let message_id = Uuid::now_v7();
    let outgoing = crate::providers::email::OutgoingEmail {
        message_id,
        to: to.clone(),
        from_name: account.from_name.clone().unwrap_or_else(|| account.name.clone()),
        from_email,
        reply_to: account.reply_to.clone(),
        subject: subject.clone(),
        html,
        text,
        unsubscribe_url: None,
    };
    match state.email.send(&outgoing).await {
        Ok(provider_id) => {
            sqlx::query(
                "INSERT INTO messages (id, account_id, profile_id, channel, to_address, subject, status, provider_message_id, sent_at, transactional)
                 VALUES ($1, $2, $3, 'email', $4, $5, 'sent', $6, now(), true)",
            )
            .bind(message_id)
            .bind(account_id)
            .bind(profile_id)
            .bind(&to)
            .bind(&subject)
            .bind(&provider_id)
            .execute(&state.db)
            .await?;
            crate::deliverability::record_confirmation_send(&state.db, message_id, &provider_id, account_id).await?;
        }
        Err(e) => tracing::warn!(account = %account_id, error = %format!("{e:#}"), "hub sign-in code not sent"),
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
        &format!(
            "INSERT INTO hub_sessions (id, account_id, profile_id, token_hash, email, method, expires_at, generation)
             SELECT $1, $2, p.id, $4, $5, $6, now() + make_interval(days => $7), {} FROM profiles p WHERE p.id = $3 AND p.account_id = $2",
            generation_sql("p.id")
        ),
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
    let row: Option<(Uuid, Uuid, String, DateTime<Utc>)> = sqlx::query_as(&format!(
        "SELECT s.id, s.profile_id, s.email, s.last_seen_at FROM hub_sessions s JOIN profiles p ON p.id = s.profile_id AND p.account_id = s.account_id
         WHERE s.token_hash = $1 AND s.account_id = $2 AND s.revoked_at IS NULL AND s.expires_at > now() AND p.email = s.email
           AND s.generation = {}",
        generation_sql("s.profile_id")
    ))
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

/// A per-session token for the hub's forms, so another site can't post them as the shopper.
pub fn form_token(state: &AppState, session_id: Uuid) -> String {
    mac_hex(state, &format!("hub-form:{}", session_id.simple()))[..32].to_string()
}

pub fn form_token_ok(state: &AppState, session_id: Uuid, given: &str) -> bool {
    same(&form_token(state, session_id), given)
}

/// The cookie that ties the sign-in forms (before any session) to this browser.
pub const FORM_COOKIE: &str = "dm_hub_f";

/// This browser's form nonce, if it has one.
pub fn form_nonce(headers: &HeaderMap) -> Option<String> {
    let jar = axum_extra::extract::CookieJar::from_headers(headers);
    jar.get(FORM_COOKIE).map(|c| c.value().to_string()).filter(|t| t.len() >= 20 && t.len() <= 64)
}

/// A new form nonce and the cookie that carries it (same host and path as the session's).
pub fn new_form_nonce(state: &AppState, account: &Account) -> (String, Cookie<'static>) {
    let nonce = crate::auth::random_token(24);
    let c = Cookie::build((FORM_COOKIE, nonce.clone()))
        .path(cookie_path(account))
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(SESSION_DAYS))
        .build();
    (nonce, c)
}

/// The token the sign-in forms carry: only a page this browser loaded has it.
pub fn anon_form_token(state: &AppState, nonce: &str) -> String {
    mac_hex(state, &format!("hub-anon-form:{nonce}"))[..32].to_string()
}

pub fn anon_form_token_ok(state: &AppState, headers: &HeaderMap, given: &str) -> bool {
    form_nonce(headers).is_some_and(|n| same(&anon_form_token(state, &n), given))
}

/// Old codes and finished sessions, a batch at a time.
pub async fn clean_up(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM hub_codes WHERE id IN (SELECT id FROM hub_codes WHERE created_at < now() - interval '2 days' LIMIT 5000)").execute(db).await?;
    sqlx::query("DELETE FROM hub_used_links WHERE token_hash IN (SELECT token_hash FROM hub_used_links WHERE used_at < now() - make_interval(hours => $1) LIMIT 5000)")
        .bind(LINK_HOURS as i32 + 1)
        .execute(db)
        .await?;
    sqlx::query(
        "DELETE FROM hub_sessions WHERE id IN (SELECT id FROM hub_sessions WHERE expires_at < now() - interval '1 day' OR revoked_at < now() - interval '1 day' LIMIT 5000)",
    )
    .execute(db)
    .await?;
    Ok(())
}

//! "Continue with Google" for an address that already has a password login whose email was never
//! confirmed.
//!
//! Password sign-up doesn't prove the address belongs to the person, so whoever made that login
//! may be a stranger who set an account up and is waiting for the real owner to move in. Google's
//! sign-in proves the address, not the login, so we don't link the two or sign anyone in. The
//! callback (`crate::api::google`) instead keeps what Google told us here, in a row that works once
//! for 10 minutes, and gives the browser only a random token for it in an HttpOnly cookie. The
//! dashboard's `/link-google` page then offers two choices:
//!
//! - "That was me": the person types the login's password, checked by the normal password sign-in
//!   with all its throttling. Right password: Google is linked, the email is marked confirmed and
//!   they're signed in like a password sign-in (two-factor still applies). Nothing is removed.
//! - "That wasn't me, start fresh": in one transaction the old login gives up everything (see
//!   [`give_up_login`]), accounts it alone owned are closed, and a new login and account are made
//!   for the Google user, exactly as a first-time Google sign-up.
//!
//! Nothing from the browser is trusted beyond the token: the email, the Google account and its
//! name all come from the row.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::audit::record;
use super::login::{self, ReqMeta, SessionStart};
use super::ratelimit;
use crate::api::account::{insert_owner_tx, session_cookie, with_cookie_domain};
use crate::api::google::{brand_for, GoogleClaims};
use crate::api::ApiJson;
use crate::auth;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const COOKIE: &str = "pl_google_link";
/// The cookie goes only to this page's API routes.
pub const COOKIE_PATH: &str = "/api/auth/google/link";
const MINUTES: i64 = 10;

/// Keep a Google sign-in waiting for the person's choice. Returns the token for the cookie.
pub async fn create(state: &AppState, user_id: Uuid, c: &GoogleClaims, next: &str) -> AppResult<String> {
    let token = auth::random_token(32);
    sqlx::query(
        "INSERT INTO google_link_pending (token_hash, user_id, email, google_sub, google_name, google_given_name, next, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(mins => $8))",
    )
    .bind(auth::sha256_hex(&token))
    .bind(user_id)
    .bind(&c.email)
    .bind(&c.sub)
    .bind(&c.name)
    .bind(&c.given_name)
    .bind(next)
    .bind(MINUTES as i32)
    .execute(&state.db)
    .await?;
    Ok(token)
}

pub fn cookie(state: &AppState, token: String) -> Cookie<'static> {
    with_cookie_domain(
        state,
        Cookie::build((COOKIE, token)).path(COOKIE_PATH).http_only(true).secure(state.cfg.secure_cookies).same_site(SameSite::Lax).max_age(time::Duration::minutes(MINUTES)).build(),
    )
}

fn clear(state: &AppState, jar: CookieJar) -> CookieJar {
    jar.remove(with_cookie_domain(state, Cookie::build(COOKIE).path(COOKIE_PATH).build()))
}

#[derive(sqlx::FromRow)]
struct Pending {
    token_hash: String,
    user_id: Uuid,
    email: String,
    google_sub: String,
    google_name: Option<String>,
    google_given_name: Option<String>,
    next: String,
}

fn gone() -> AppError {
    AppError::NotFound("This Google sign-in has expired or was already used. Sign in again.".into())
}

/// The waiting sign-in this browser holds, still unused and current. Doesn't use it up.
async fn held(state: &AppState, jar: &CookieJar) -> AppResult<Pending> {
    let token = jar.get(COOKIE).map(|c| c.value().to_string()).filter(|t| !t.is_empty() && t.len() <= 200).ok_or_else(gone)?;
    sqlx::query_as(
        "SELECT token_hash, user_id, email, google_sub, google_name, google_given_name, next FROM google_link_pending
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(auth::sha256_hex(&token))
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(gone)
}

/// Use the waiting sign-in up, inside the transaction that acts on it. False: it was used,
/// expired or never existed.
async fn take(tx: &mut sqlx::PgConnection, token_hash: &str) -> AppResult<bool> {
    let n = sqlx::query("UPDATE google_link_pending SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()")
        .bind(token_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    Ok(n == 1)
}

async fn limit(state: &AppState, meta: &ReqMeta) -> AppResult<()> {
    ratelimit::hit_ip(state, "google_link", meta.ip.as_deref(), 60, 900).await
}

/// `GET /api/auth/google/link`: what the page tells the person about the existing login.
pub async fn show_h(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar) -> AppResult<Json<Value>> {
    let meta = ReqMeta::from_headers(&headers);
    limit(&state, &meta).await?;
    let p = held(&state, &jar).await?;
    let row: Option<(DateTime<Utc>, bool)> = sqlx::query_as("SELECT created_at, password_hash IS NOT NULL FROM users WHERE id = $1 AND email = $2 AND disabled_at IS NULL")
        .bind(p.user_id)
        .bind(&p.email)
        .fetch_optional(&state.db)
        .await?;
    let (created_at, has_password) = row.ok_or_else(gone)?;
    let created_on = created_at.with_timezone(&chrono_tz::America::Los_Angeles).format("%B %-d, %Y").to_string();
    Ok(Json(json!({
        "email": p.email,
        "login_created_at": created_at,
        "login_created_on": created_on,
        "has_password": has_password,
        "can_start_fresh": state.cfg.allow_signup,
        "expires_in_minutes": MINUTES,
    })))
}

#[derive(Deserialize)]
pub struct PasswordBody {
    pub password: String,
}

/// `POST /api/auth/google/link/password`: "That was me". The login's password, checked by the
/// normal password sign-in; then Google is linked and they're signed in as with a password.
pub async fn password_h(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar, ApiJson(b): ApiJson<PasswordBody>) -> AppResult<(CookieJar, Json<Value>)> {
    let meta = ReqMeta::from_headers(&headers);
    limit(&state, &meta).await?;
    let p = held(&state, &jar).await?;
    // Throttled and recorded exactly like the sign-in page; a wrong password gets its answer.
    let (uid, version) = login::check_login_password(&state, &p.email, &b.password, &meta).await?;
    if uid != p.user_id {
        return Err(gone());
    }
    let mut tx = state.db.begin().await?;
    if !take(&mut tx, &p.token_hash).await? {
        return Err(gone());
    }
    // Only while the login is still the one whose password was just checked.
    let n = sqlx::query(
        "UPDATE users SET google_sub = $2, email_confirmed_at = COALESCE(email_confirmed_at, now())
         WHERE id = $1 AND email = $3 AND google_sub IS NULL AND credential_version = $4 AND disabled_at IS NULL",
    )
    .bind(uid)
    .bind(&p.google_sub)
    .bind(&p.email)
    .bind(version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(AppError::Conflict("this login changed while you were signing in; continue with Google again".into()));
    }
    tx.commit().await?;
    for account_id in login::user_accounts(&state, uid).await? {
        record(&state, account_id, &meta.actor(Some(uid)), "login.google_linked", None, json!({"proof": "password"})).await;
    }
    let account_id = login::pick_account(&state, uid).await?.ok_or_else(|| AppError::Forbidden("this login isn't part of any account anymore; ask a teammate to invite you again".into()))?;
    // A password sign-in: two-factor still applies before the session can be used.
    let how = SessionStart { credential_version: Some(version), entered_password: true, ..Default::default() };
    let token = login::start_session_with(&state, uid, account_id, "password", None, &meta, how).await?;
    record(&state, account_id, &meta.actor(Some(uid)), "login", None, json!({"method": "password"})).await;
    let mut me = login::me_for_token(&state, &token, &meta).await?;
    me["next"] = json!(p.next);
    Ok((clear(&state, jar).add(session_cookie(&state, token)), Json(me)))
}

/// `POST /api/auth/google/link/fresh`: "That wasn't me, start fresh". In one transaction: the old
/// login gives up everything and accounts it alone owned close ([`give_up_login`]), then a new
/// login and account are made for the Google user, as for a first-time Google sign-up.
pub async fn fresh_h(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar) -> AppResult<(CookieJar, Json<Value>)> {
    let meta = ReqMeta::from_headers(&headers);
    limit(&state, &meta).await?;
    if !state.cfg.allow_signup {
        return Err(AppError::Forbidden("sign-ups are turned off on this server".into()));
    }
    let p = held(&state, &jar).await?;
    let mut tx = state.db.begin().await?;
    if !take(&mut tx, &p.token_hash).await? {
        return Err(gone());
    }
    // Still the unconfirmed login the page described; held until this commits.
    let old: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 AND email = $2 AND google_sub IS NULL AND email_confirmed_at IS NULL AND disabled_at IS NULL FOR UPDATE")
        .bind(p.user_id)
        .bind(&p.email)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(old) = old else {
        return Err(AppError::Conflict("this login changed while you were deciding; continue with Google again".into()));
    };
    let closed = give_up_login(&mut tx, old).await?;
    let claims = GoogleClaims { sub: p.google_sub.clone(), email: p.email.clone(), name: p.google_name.clone(), given_name: p.google_given_name.clone() };
    let (company, website) = brand_for(&claims);
    let (user, account) = insert_owner_tx(&mut tx, &claims.email, None, Some(&claims.sub), claims.name.clone(), &company, website).await?;
    tx.commit().await?;
    for account_id in &closed {
        record(&state, *account_id, &meta.actor(Some(user.id)), "account.closed", None, json!({"reason": "email_owner_started_fresh"})).await;
    }
    let token = login::start_session(&state, user.id, account.id, "google", None, &meta).await?;
    record(&state, account.id, &meta.actor(Some(user.id)), "login", None, json!({"method": "google", "started_fresh": true})).await;
    let mut me = login::me_for_token(&state, &token, &meta).await?;
    me["next"] = json!("/onboarding");
    Ok((clear(&state, jar).add(session_cookie(&state, token)), Json(me)))
}

/// Make a login unusable and empty, and close the accounts it alone owned. Returns those accounts.
///
/// The login: its password, sessions, resets, confirmation links, recovery codes, two-factor and
/// single sign-on links go; every API key it made (anywhere) is revoked; it leaves every account;
/// its email becomes an unusable placeholder (freeing the address) and it's marked disabled.
///
/// Each account it was the only owner of is closed: every key and pending invite there stops
/// working, every member leaves it (their logins are untouched), single sign-on is turned off, its
/// site key is replaced (so its website snippet and forms stop), campaigns that were scheduled or
/// sending stop, flows go back to draft, email sending is paused, and auto-recharge and plan
/// renewal stop. Sessions and API keys never work on a closed account (see `login`), and the
/// scheduler starts no campaign, scheduled report, feed sync or product check for it
/// (`jobs::scheduler_tick`, `reports::saved::enqueue_due`, `catalog::feeds::queue_due`,
/// `catalog::verify::queue_due`).
pub async fn give_up_login(tx: &mut sqlx::PgConnection, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    let owned: Vec<Uuid> = sqlx::query_scalar("SELECT account_id FROM memberships WHERE user_id = $1 AND role = 'owner'").bind(user_id).fetch_all(&mut *tx).await?;
    // Lock those accounts (joining one takes this lock too, see `members::join`), then every
    // membership in them, so a role change or a join in flight is either seen here or waits.
    sqlx::query("SELECT id FROM accounts WHERE id = ANY($1) ORDER BY id FOR UPDATE").bind(&owned).execute(&mut *tx).await?;
    let members: Vec<(Uuid, Uuid, String)> = sqlx::query_as("SELECT account_id, user_id, role FROM memberships WHERE account_id = ANY($1) ORDER BY account_id, user_id FOR UPDATE")
        .bind(&owned)
        .fetch_all(&mut *tx)
        .await?;
    let closed: Vec<Uuid> = owned.iter().copied().filter(|a| !members.iter().any(|(acct, uid, role)| acct == a && *uid != user_id && role == "owner")).collect();
    sqlx::query(
        "UPDATE accounts SET closed_at = now(), public_key = 'pk_closed_' || replace(gen_random_uuid()::text, '-', ''),
                sending_paused_at = COALESCE(sending_paused_at, now()), sending_paused_reason = COALESCE(sending_paused_reason, 'account closed'),
                auto_recharge_enabled = false, email_plan_cancel_at_period_end = true
         WHERE id = ANY($1)",
    )
    .bind(&closed)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE revoked_at IS NULL AND (account_id = ANY($1) OR created_by = $2)").bind(&closed).bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM team_invites WHERE account_id = ANY($1) AND accepted_at IS NULL").bind(&closed).execute(&mut *tx).await?;
    sqlx::query("UPDATE campaigns SET status = 'failed', error = 'account closed', updated_at = now() WHERE account_id = ANY($1) AND status IN ('scheduled', 'sending', 'testing')")
        .bind(&closed)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE flows SET status = 'draft', updated_at = now() WHERE account_id = ANY($1) AND status <> 'draft'").bind(&closed).execute(&mut *tx).await?;
    sqlx::query("UPDATE account_sso SET enabled = false WHERE account_id = ANY($1)").bind(&closed).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM sessions WHERE account_id = ANY($1) OR user_id = $2").bind(&closed).bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM memberships WHERE account_id = ANY($1) OR user_id = $2").bind(&closed).bind(user_id).execute(&mut *tx).await?;
    for table in ["recovery_codes", "password_resets", "email_confirmations", "google_link_pending", "sso_identities"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id = $1")).bind(user_id).execute(&mut *tx).await?;
    }
    sqlx::query(
        "UPDATE users SET email = 'removed+' || id::text || '@invalid.dumbmail', password_hash = NULL, google_sub = NULL,
                totp_secret_enc = NULL, totp_pending_enc = NULL, totp_enabled_at = NULL, totp_last_step = NULL,
                credential_version = credential_version + 1, last_account_id = NULL, disabled_at = now()
         WHERE id = $1",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    Ok(closed)
}

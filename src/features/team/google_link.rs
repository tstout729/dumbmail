//! Linking a Google account to an existing login, and "Continue with Google" for an address that
//! already has a password login whose email was never confirmed.
//!
//! Password sign-up doesn't prove the address belongs to the person, so whoever made that login
//! may be a stranger who set an account up and is waiting for the real owner to move in. Google's
//! sign-in proves the address (when Google is the authority for it, see
//! `crate::api::google::GoogleClaims::authoritative`), not the login, so we don't link the two or
//! sign anyone in. The callback instead keeps what Google told us here, in a row that works once
//! for 10 minutes, and gives the browser only a random token for it in an HttpOnly cookie. The
//! dashboard's `/link-google` page then offers two choices:
//!
//! - "That was me": the person types the login's password, checked by the normal password sign-in
//!   with all its throttling. Right password: they're signed in as with a password, and Google is
//!   linked (and the email marked confirmed) once that sign-in is complete. Nothing is removed.
//! - "That wasn't me, start fresh": in one transaction the old login gives up everything (see
//!   [`give_up_login`]), accounts it alone owned are closed, and a new login and account are made
//!   for the Google user, exactly as a first-time Google sign-up.
//!
//! **A link is saved only once the sign-in that makes it is complete.** Whether it's "That was me"
//! or a Google sign-in to a login whose email is confirmed, the session starts first; the link
//! waits in this table, tied to that session, until its two-factor step is done (see
//! [`complete_if_signed_in`], called by `login` when a code is entered or two-factor is set up).
//! Until then the login doesn't change. The link is made only while the login is still as it was
//! checked: no Google account, the same confirmation state and the same credential version.
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
/// How long the choice stays open.
const MINUTES: i64 = 10;
/// How long a link waits for its sign-in's two-factor step.
const STEP_MINUTES: i64 = 15;

/// The domain of a given-up login's placeholder address. Nobody can sign up or be invited with it.
const REMOVED_DOMAIN: &str = "removed.dumbmail.invalid";

/// Whether an address is a given-up login's placeholder (refused for new logins and invites).
pub fn reserved_login_email(email: &str) -> bool {
    email.trim().to_ascii_lowercase().rsplit_once('@').is_some_and(|(_, d)| d == REMOVED_DOMAIN)
}

/// A link to make once a sign-in is complete, and the login's state it was checked against.
#[derive(Debug, Clone)]
pub struct LinkPlan {
    pub user_id: Uuid,
    pub email: String,
    pub google_sub: String,
    pub google_name: Option<String>,
    pub google_given_name: Option<String>,
    /// Google is the authority for the address, so linking also confirms it.
    pub authoritative: bool,
    pub credential_version: i32,
    pub was_confirmed: bool,
    pub next: String,
}

impl LinkPlan {
    pub fn new(user_id: Uuid, c: &GoogleClaims, credential_version: i32, was_confirmed: bool, next: &str) -> Self {
        LinkPlan {
            user_id,
            email: c.email.clone(),
            google_sub: c.sub.clone(),
            google_name: c.name.clone(),
            google_given_name: c.given_name.clone(),
            authoritative: c.authoritative(),
            credential_version,
            was_confirmed,
            next: next.to_string(),
        }
    }
}

/// Keep a Google sign-in waiting for the person's choice. Returns the token for the cookie.
pub async fn create(state: &AppState, user_id: Uuid, c: &GoogleClaims, next: &str) -> AppResult<String> {
    let token = auth::random_token(32);
    sqlx::query(
        "INSERT INTO google_link_pending (token_hash, user_id, email, google_sub, google_name, google_given_name, google_authoritative, next, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(mins => $9))",
    )
    .bind(auth::sha256_hex(&token))
    .bind(user_id)
    .bind(&c.email)
    .bind(&c.sub)
    .bind(&c.name)
    .bind(&c.given_name)
    .bind(c.authoritative())
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
    google_authoritative: bool,
    next: String,
}

fn gone() -> AppError {
    AppError::NotFound("This Google sign-in has expired or was already used. Sign in again.".into())
}

/// The waiting choice this browser holds, still unused, current and not yet taken by a sign-in.
/// Doesn't use it up.
async fn held(state: &AppState, jar: &CookieJar) -> AppResult<Pending> {
    let token = jar.get(COOKIE).map(|c| c.value().to_string()).filter(|t| !t.is_empty() && t.len() <= 200).ok_or_else(gone)?;
    sqlx::query_as(
        "SELECT token_hash, user_id, email, google_sub, google_name, google_given_name, google_authoritative, next FROM google_link_pending
         WHERE token_hash = $1 AND used_at IS NULL AND session_id IS NULL AND expires_at > now()",
    )
    .bind(auth::sha256_hex(&token))
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(gone)
}

/// Use the waiting choice up, inside the transaction that acts on it. False: it was used, taken
/// by a sign-in, expired or never existed.
async fn take(tx: &mut sqlx::PgConnection, token_hash: &str) -> AppResult<bool> {
    let n = sqlx::query("UPDATE google_link_pending SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND session_id IS NULL AND expires_at > now()")
        .bind(token_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    Ok(n == 1)
}

async fn limit(state: &AppState, meta: &ReqMeta) -> AppResult<()> {
    ratelimit::hit_ip(state, "google_link", meta.ip.as_deref(), 60, 900).await
}

/// Start a session for a sign-in that links Google once it's complete. The link waits, tied to
/// the new session: `choice` is the waiting choice's token hash ("That was me", which it takes),
/// or None to record a new one (a Google sign-in to a confirmed login). If the session needs no
/// more steps the link is made right away; otherwise when its two-factor step is done.
pub async fn linking_session(state: &AppState, plan: &LinkPlan, choice: Option<&str>, account_id: Uuid, method: &str, meta: &ReqMeta) -> AppResult<String> {
    let sid = Uuid::now_v7();
    let n = match choice {
        Some(hash) => sqlx::query(
            "UPDATE google_link_pending SET session_id = $2, credential_version = $3, was_confirmed = $4, expires_at = now() + make_interval(mins => $5)
             WHERE token_hash = $1 AND used_at IS NULL AND session_id IS NULL AND expires_at > now()",
        )
        .bind(hash)
        .bind(sid)
        .bind(plan.credential_version)
        .bind(plan.was_confirmed)
        .bind(STEP_MINUTES as i32)
        .execute(&state.db)
        .await?
        .rows_affected(),
        None => sqlx::query(
            "INSERT INTO google_link_pending (token_hash, user_id, email, google_sub, google_name, google_given_name, google_authoritative, next,
                                              session_id, credential_version, was_confirmed, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now() + make_interval(mins => $12))",
        )
        .bind(auth::sha256_hex(&auth::random_token(32)))
        .bind(plan.user_id)
        .bind(&plan.email)
        .bind(&plan.google_sub)
        .bind(&plan.google_name)
        .bind(&plan.google_given_name)
        .bind(plan.authoritative)
        .bind(&plan.next)
        .bind(sid)
        .bind(plan.credential_version)
        .bind(plan.was_confirmed)
        .bind(STEP_MINUTES as i32)
        .execute(&state.db)
        .await?
        .rows_affected(),
    };
    if n != 1 {
        return Err(gone());
    }
    let how = SessionStart { id: Some(sid), credential_version: Some(plan.credential_version), entered_password: method == "password", ..Default::default() };
    let token = login::start_session_with(state, plan.user_id, account_id, method, None, meta, how).await?;
    complete_if_signed_in(state, sid, meta).await?;
    Ok(token)
}

/// Make the link a session is waiting on, if the session has finished signing in (two-factor
/// included). Returns whether a link was made. Called when a linking session starts and whenever a
/// session finishes its two-factor step.
pub async fn complete_if_signed_in(state: &AppState, sid: Uuid, meta: &ReqMeta) -> AppResult<bool> {
    let ctx = login::ctx_for_session_id(state, sid, meta).await?;
    if ctx.restriction.is_some() {
        return Ok(false);
    }
    let mut tx = state.db.begin().await?;
    type Row = (Uuid, String, String, bool, Option<i32>, Option<bool>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT user_id, email, google_sub, google_authoritative, credential_version, was_confirmed FROM google_link_pending
         WHERE session_id = $1 AND used_at IS NULL AND expires_at > now() FOR UPDATE",
    )
    .bind(sid)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((uid, email, sub, authoritative, Some(version), Some(was_confirmed))) = row else { return Ok(false) };
    // The login's row first: turning two-factor on writes it, so from here it can't change. Then
    // the session's proof, read again under that lock (and the account's rule, held too): the
    // check above may be from just before two-factor was turned on.
    let totp_on: Option<bool> = sqlx::query_scalar("SELECT totp_enabled_at IS NOT NULL FROM users WHERE id = $1 FOR UPDATE").bind(uid).fetch_optional(&mut *tx).await?;
    let proof: Option<(bool, String, bool)> = sqlx::query_as(
        "SELECT s.mfa_ok, s.auth_method, a.require_two_factor FROM sessions s JOIN accounts a ON a.id = s.account_id WHERE s.id = $1 AND s.user_id = $2 FOR SHARE OF a",
    )
    .bind(sid)
    .bind(uid)
    .fetch_optional(&mut *tx)
    .await?;
    let (Some(totp_on), Some((mfa_ok, method, required))) = (totp_on, proof) else { return Ok(false) };
    let needs_code = ((method == "password" || method == "google") && totp_on) || required;
    if needs_code && !mfa_ok {
        // Two-factor arrived meanwhile: the link waits for this session's code.
        return Ok(false);
    }
    sqlx::query("UPDATE google_link_pending SET used_at = now() WHERE session_id = $1").bind(sid).execute(&mut *tx).await?;
    // Only while the login is exactly as it was checked, and this session is still its own.
    let linked = sqlx::query(
        "UPDATE users SET google_sub = $3, email_confirmed_at = CASE WHEN $4 THEN COALESCE(email_confirmed_at, now()) ELSE email_confirmed_at END
         WHERE id = $1 AND email = $2 AND google_sub IS NULL AND disabled_at IS NULL AND credential_version = $5 AND (email_confirmed_at IS NOT NULL) = $6
           AND EXISTS (SELECT 1 FROM sessions s WHERE s.id = $7 AND s.user_id = $1)",
    )
    .bind(uid)
    .bind(&email)
    .bind(&sub)
    .bind(authoritative)
    .bind(version)
    .bind(was_confirmed)
    .bind(sid)
    .execute(&mut *tx)
    .await;
    let linked = match linked {
        Ok(r) => r.rows_affected() == 1,
        // Another login took this Google account meanwhile: nothing linked.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => false,
        Err(e) => return Err(e.into()),
    };
    if !linked {
        // Used up either way; the login changed since it was checked.
        tx.commit().await?;
        tracing::info!(user = %uid, "Google link not made: the login changed before the sign-in finished");
        return Ok(false);
    }
    tx.commit().await?;
    for account_id in login::user_accounts(state, uid).await? {
        record(state, account_id, &meta.actor(Some(uid)), "login.google_linked", None, json!({})).await;
    }
    Ok(true)
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
/// normal password sign-in; they're signed in as with a password, and Google is linked once that
/// sign-in is complete (see [`linking_session`]).
pub async fn password_h(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar, ApiJson(b): ApiJson<PasswordBody>) -> AppResult<(CookieJar, Json<Value>)> {
    let meta = ReqMeta::from_headers(&headers);
    limit(&state, &meta).await?;
    let p = held(&state, &jar).await?;
    // Throttled and recorded exactly like the sign-in page; a wrong password gets its answer.
    let (uid, version) = login::check_login_password(&state, &p.email, &b.password, &meta).await?;
    if uid != p.user_id {
        return Err(gone());
    }
    let was_confirmed: bool = sqlx::query_scalar("SELECT email_confirmed_at IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    let account_id = login::pick_account(&state, uid).await?.ok_or_else(|| AppError::Forbidden("this login isn't part of any account anymore; ask a teammate to invite you again".into()))?;
    let plan = LinkPlan {
        user_id: uid,
        email: p.email.clone(),
        google_sub: p.google_sub.clone(),
        google_name: p.google_name.clone(),
        google_given_name: p.google_given_name.clone(),
        authoritative: p.google_authoritative,
        credential_version: version,
        was_confirmed,
        next: p.next.clone(),
    };
    let token = linking_session(&state, &plan, Some(&p.token_hash), account_id, "password", &meta).await?;
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
    let claims = GoogleClaims { sub: p.google_sub.clone(), email: p.email.clone(), name: p.google_name.clone(), given_name: p.google_given_name.clone(), hd: None };
    let (company, website) = brand_for(&claims);
    let (user, account) = insert_owner_tx(&mut tx, &claims.email, None, Some(&claims.sub), p.google_authoritative, claims.name.clone(), &company, website).await?;
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

/// The job that releases a closed account's phone numbers (background lane).
pub const RELEASE_NUMBERS_JOB: &str = "team_release_numbers";

/// Make a login unusable and empty, and close the accounts it alone owned. Returns those accounts.
///
/// Locks first, in the one order every ownership change and every join takes them too (see
/// `members` and `sso`): the rows of every account this login belongs to, in id order, then the
/// membership rows in those accounts, then (when deleting them) invites. Only then is "alone"
/// decided, so a role change, a transfer or a join racing with this one is either seen here or
/// waits for it, and two of these can't deadlock.
///
/// The login: its password, sessions, resets, confirmation links, recovery codes, two-factor and
/// single sign-on links go; every API key it made (anywhere) is revoked; it leaves every account;
/// its email becomes an unusable placeholder at a domain nobody can sign up with (random, so it
/// can't have been taken in advance), freeing the address; and it's marked disabled.
///
/// Each account it was the only owner of is closed: every key and pending invite there stops
/// working, every member leaves it (their logins are untouched), single sign-on is turned off, its
/// site key is replaced, campaigns that were scheduled or sending stop, flows go back to draft,
/// email sending is paused, auto-recharge and plan renewal stop, and a background job releases its
/// phone numbers. A closed account then does nothing: see `crate::models::account_closed`.
pub async fn give_up_login(tx: &mut sqlx::PgConnection, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    let theirs: Vec<Uuid> = sqlx::query_scalar("SELECT account_id FROM memberships WHERE user_id = $1").bind(user_id).fetch_all(&mut *tx).await?;
    sqlx::query("SELECT id FROM accounts WHERE id = ANY($1) ORDER BY id FOR UPDATE").bind(&theirs).execute(&mut *tx).await?;
    // Every membership change in these accounts holds the account row, so this is current.
    let members: Vec<(Uuid, Uuid, String)> = sqlx::query_as("SELECT account_id, user_id, role FROM memberships WHERE account_id = ANY($1) ORDER BY account_id, user_id FOR UPDATE")
        .bind(&theirs)
        .fetch_all(&mut *tx)
        .await?;
    let owned = members.iter().filter(|(_, u, role)| *u == user_id && role == "owner").map(|(a, ..)| *a);
    let closed: Vec<Uuid> = owned.filter(|a| !members.iter().any(|(acct, u, role)| acct == a && *u != user_id && role == "owner")).collect();
    sqlx::query(
        "UPDATE accounts SET closed_at = now(), public_key = 'pk_closed_' || replace(gen_random_uuid()::text, '-', ''),
                sending_paused_at = COALESCE(sending_paused_at, now()), sending_paused_reason = COALESCE(sending_paused_reason, 'account closed'),
                auto_recharge_enabled = false, email_plan_cancel_at_period_end = true
         WHERE id = ANY($1)",
    )
    .bind(&closed)
    .execute(&mut *tx)
    .await?;
    for account_id in &closed {
        crate::jobs::enqueue_background(&mut *tx, RELEASE_NUMBERS_JOB, json!({"account_id": account_id}), None).await?;
    }
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
    let placeholder = {
        let mut bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        format!("removed+{}@{REMOVED_DOMAIN}", hex::encode(bytes))
    };
    sqlx::query(
        "UPDATE users SET email = $2, password_hash = NULL, google_sub = NULL,
                totp_secret_enc = NULL, totp_pending_enc = NULL, totp_enabled_at = NULL, totp_last_step = NULL,
                credential_version = credential_version + 1, last_account_id = NULL, disabled_at = now()
         WHERE id = $1",
    )
    .bind(user_id)
    .bind(placeholder)
    .execute(&mut *tx)
    .await?;
    Ok(closed)
}

/// The job behind [`RELEASE_NUMBERS_JOB`]: give back a closed account's phone numbers. Each one is
/// released at most once (released numbers are skipped), so a retry is safe; a provider that keeps
/// refusing fails the job, which is retried and logged.
pub async fn release_numbers(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    if !crate::models::account_closed(&state.db, account_id).await? {
        return Ok(());
    }
    let numbers: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, phone_number FROM phone_numbers WHERE account_id = $1 AND status <> 'released' ORDER BY id").bind(account_id).fetch_all(&state.db).await?;
    let mut failed = 0;
    for (id, number) in numbers {
        if let Err(e) = crate::phone::release(state, account_id, id).await {
            failed += 1;
            tracing::error!(account = %account_id, number, error = %e.public_message(), "couldn't release a closed account's phone number; will retry");
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} phone number(s) of a closed account weren't released");
    }
    Ok(())
}

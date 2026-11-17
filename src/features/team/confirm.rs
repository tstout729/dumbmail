//! Confirming a login's email address.
//!
//! Signing up with a password doesn't prove the address belongs to the person, so a login starts
//! unconfirmed. It becomes confirmed when its owner shows they get mail there: opening a link from
//! this module, finishing a password reset, accepting a team invite sent to that address, or
//! signing in with Google for it. Single sign-on doesn't count: an account's owner sets up the
//! identity provider, and that owner may not be the address's owner.
//!
//! "Continue with Google" links to an existing password login only once it's confirmed (see
//! `crate::api::google` and [`super::google_link`]).

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::audit::{audit, record};
use super::login::{user_accounts, ReqMeta};
use super::{mail, ratelimit};
use crate::api::ApiJson;
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// How long a confirmation link works.
const LINK_HOURS: i64 = 24;

/// Email the login a new confirmation link (older unused links stop working). Does nothing for a
/// login that's already confirmed or given up.
pub async fn send_link(state: &AppState, user_id: Uuid) -> AppResult<()> {
    let row: Option<(String, bool)> = sqlx::query_as("SELECT email, email_confirmed_at IS NOT NULL FROM users WHERE id = $1 AND disabled_at IS NULL").bind(user_id).fetch_optional(&state.db).await?;
    let Some((email, false)) = row else { return Ok(()) };
    let token = auth::random_token(32);
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM email_confirmations WHERE user_id = $1 AND used_at IS NULL").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO email_confirmations (token_hash, user_id, email, expires_at) VALUES ($1, $2, $3, now() + make_interval(hours => $4))")
        .bind(auth::sha256_hex(&token))
        .bind(user_id)
        .bind(&email)
        .bind(LINK_HOURS as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let url = format!("{}/confirm-email?token={token}", state.cfg.public_url());
    mail::send(
        state,
        &email,
        "Confirm your email for DumbMail",
        &["Confirm this is your email address for your DumbMail login. Use the button below. The link works for 24 hours.".into()],
        ("Confirm my email", &url),
        "If you didn't make a DumbMail login, ignore this email.",
    )
    .await;
    Ok(())
}

/// Send the link in the background, so a slow or failing email provider never holds up or fails
/// the request that asked (sign-up, "Send the link again").
pub fn send_link_later(state: &AppState, user_id: Uuid) {
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = send_link(&st, user_id).await {
            tracing::error!(error = %e, "email confirmation link failed");
        }
    });
}

/// `POST /api/me/email-confirmation`: email the signed-in login a new confirmation link.
pub async fn request_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let uid = ctx.user_id.ok_or_else(|| AppError::Forbidden("sign in to the dashboard to do this".into()))?;
    let confirmed: bool = sqlx::query_scalar("SELECT email_confirmed_at IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
    if confirmed {
        return Ok(Json(json!({"ok": true, "already_confirmed": true, "message": "Your email is already confirmed."})));
    }
    // The same limits as asking for a password reset.
    ratelimit::hit_ip(&state, "confirm_send", ctx.ip.as_deref(), 10, 3600).await?;
    ratelimit::hit(&state, &format!("confirm_send:user:{uid}"), 3, 3600).await?;
    send_link_later(&state, uid);
    audit(&state, &ctx, "login.email_confirmation_sent", None, json!({})).await;
    Ok(Json(json!({"ok": true, "already_confirmed": false, "message": "We sent a new link to your email. It works for 24 hours."})))
}

#[derive(Deserialize)]
pub struct ConfirmBody {
    pub token: String,
}

/// `POST /api/auth/confirm-email`: open a confirmation link. Marks the address confirmed; it
/// signs nobody in.
pub async fn confirm_h(State(state): State<AppState>, headers: HeaderMap, ApiJson(b): ApiJson<ConfirmBody>) -> AppResult<Json<Value>> {
    let meta = ReqMeta::from_headers(&headers);
    ratelimit::hit_ip(&state, "confirm_email", meta.ip.as_deref(), 20, 3600).await?;
    let invalid = || AppError::bad("this confirmation link has expired or was already used; sign in and ask for a new one");
    let token = b.token.trim();
    if token.is_empty() || token.len() > 200 {
        return Err(invalid());
    }
    let mut tx = state.db.begin().await?;
    // The link counts only for the address it was sent to.
    let uid: Option<Uuid> = sqlx::query_scalar(
        "UPDATE email_confirmations c SET used_at = now() FROM users u
         WHERE c.token_hash = $1 AND c.used_at IS NULL AND c.expires_at > now() AND u.id = c.user_id AND u.email = c.email AND u.disabled_at IS NULL
         RETURNING c.user_id",
    )
    .bind(auth::sha256_hex(token))
    .fetch_optional(&mut *tx)
    .await?;
    let uid = uid.ok_or_else(invalid)?;
    sqlx::query("UPDATE users SET email_confirmed_at = COALESCE(email_confirmed_at, now()) WHERE id = $1 AND disabled_at IS NULL").bind(uid).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM email_confirmations WHERE user_id = $1 AND used_at IS NULL").bind(uid).execute(&mut *tx).await?;
    tx.commit().await?;
    for account_id in user_accounts(&state, uid).await? {
        record(&state, account_id, &meta.actor(Some(uid)), "login.email_confirmed", None, json!({})).await;
    }
    Ok(Json(json!({"ok": true, "message": "Your email is confirmed."})))
}

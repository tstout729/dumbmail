//! The account's WhatsApp connection: its WhatsApp Business Account id, phone number id, a
//! permanent system-user access token and its Meta app's app secret (both encrypted, never shown
//! again or logged), the webhook address and verify token to paste into the Meta app, WhatsApp's own
//! sign-up words and opt-in buttons, and what the last "Check connection" read from Meta.
//!
//! **One verified sender.** Sending is on only with all four values saved (the app secret too: without
//! it webhooks, opt-outs included, can't be checked). The connection then gets a `sender_id`, and
//! everything that belongs to a sender (consent, service windows, queued sends, claims) is bound to
//! it. Changing the business account or phone number makes a new sender and starts fresh in the same
//! transaction (see [`consent::sender_changed`]); disconnecting clears it.
//!
//! Embedded Signup (when DumbMail is a Meta Tech Provider) would fill the same columns from the
//! signup's answer, with `connected_via = 'embedded_signup'`; nothing else changes.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{consent, graph};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MAX_SIGNUP_WORDS: usize = 10;

/// A usable connection, with its secrets decrypted (never serialized or logged).
pub struct Conn {
    pub account_id: Uuid,
    pub waba_id: String,
    pub phone_number_id: String,
    pub token: String,
    pub app_secret: String,
    pub sender_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct Row {
    waba_id: Option<String>,
    phone_number_id: Option<String>,
    access_token_enc: Option<String>,
    access_token_hint: Option<String>,
    app_secret_enc: Option<String>,
    verify_token: String,
    webhook_key: String,
    connected_via: String,
    sender_id: Option<Uuid>,
    signup_keywords: Vec<String>,
    signup_reply: Option<String>,
    optin_buttons: Vec<String>,
    display_phone_number: Option<String>,
    verified_name: Option<String>,
    quality_rating: Option<String>,
    messaging_limit: Option<String>,
    checked_at: Option<DateTime<Utc>>,
    error: Option<String>,
    error_at: Option<DateTime<Utc>>,
    last_webhook_at: Option<DateTime<Utc>>,
}

const COLS: &str = "waba_id, phone_number_id, access_token_enc, access_token_hint, app_secret_enc, verify_token, webhook_key, connected_via, sender_id,
    signup_keywords, signup_reply, optin_buttons, display_phone_number, verified_name, quality_rating, messaging_limit, checked_at, error, error_at, last_webhook_at";

async fn ensure_row(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO whatsapp_settings (account_id, verify_token, webhook_key) VALUES ($1, $2, $3) ON CONFLICT (account_id) DO NOTHING")
        .bind(account_id)
        .bind(crate::auth::random_token(24))
        .bind(crate::auth::random_token(18))
        .execute(db)
        .await?;
    Ok(())
}

/// The account's settings row, made (with a fresh verify token and webhook address) the first time.
async fn row(db: &sqlx::PgPool, account_id: Uuid) -> AppResult<Row> {
    ensure_row(db, account_id).await?;
    Ok(sqlx::query_as::<_, Row>(&format!("SELECT {COLS} FROM whatsapp_settings WHERE account_id = $1")).bind(account_id).fetch_one(db).await?)
}

pub fn webhook_url(state: &AppState, key: &str) -> String {
    format!("{}/webhooks/whatsapp/{key}", state.cfg.public_url())
}

/// Decrypt a stored secret.
pub fn decrypt(state: &AppState, enc: &str) -> AppResult<String> {
    crate::crypto::decrypt(&state.cfg.secret_key, enc).map_err(AppError::Internal)
}

/// The connection, if sending is on (ids, token, app secret and a sender).
pub async fn load(state: &AppState, account_id: Uuid) -> AppResult<Option<Conn>> {
    type R = (Option<String>, Option<String>, Option<String>, Option<String>, Option<Uuid>);
    let r: Option<R> = sqlx::query_as("SELECT waba_id, phone_number_id, access_token_enc, app_secret_enc, sender_id FROM whatsapp_settings WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((Some(waba_id), Some(phone_number_id), Some(token_enc), Some(secret_enc), Some(sender_id))) = r else { return Ok(None) };
    Ok(Some(Conn { account_id, waba_id, phone_number_id, token: decrypt(state, &token_enc)?, app_secret: decrypt(state, &secret_enc)?, sender_id }))
}

/// The ids and token without the sending switch (Check connection and template reads work before
/// the app secret is saved).
pub async fn load_reader(state: &AppState, account_id: Uuid) -> AppResult<Option<(String, String, String)>> {
    let r: Option<(Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT waba_id, phone_number_id, access_token_enc FROM whatsapp_settings WHERE account_id = $1").bind(account_id).fetch_optional(&state.db).await?;
    let Some((Some(waba), Some(phone), Some(enc))) = r else { return Ok(None) };
    Ok(Some((waba, phone, decrypt(state, &enc)?)))
}

/// Whether sending is on (a verified sender with every credential saved).
pub async fn connected(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM whatsapp_settings WHERE account_id = $1 AND sender_id IS NOT NULL)").bind(account_id).fetch_one(db).await
}

/// The current sender id.
pub async fn sender_id<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> sqlx::Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("SELECT sender_id FROM whatsapp_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?.flatten())
}

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let r = row(&state.db, account_id).await?;
    let templates: (i64, i64) = sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE status = 'APPROVED') FROM whatsapp_templates WHERE account_id = $1 AND status <> 'DELETED'")
        .bind(account_id)
        .fetch_one(&state.db)
        .await?;
    let missing: Vec<&str> = [("WhatsApp Business Account id", r.waba_id.is_none()), ("phone number id", r.phone_number_id.is_none()), ("access token", r.access_token_enc.is_none()), ("app secret", r.app_secret_enc.is_none())]
        .into_iter()
        .filter(|(_, m)| *m)
        .map(|(n, _)| n)
        .collect();
    Ok(json!({
        "connected": r.sender_id.is_some(),
        "missing": missing,
        "connected_via": r.connected_via,
        "waba_id": r.waba_id,
        "phone_number_id": r.phone_number_id,
        "access_token_hint": r.access_token_hint,
        "access_token_set": r.access_token_enc.is_some(),
        "app_secret_set": r.app_secret_enc.is_some(),
        "webhook_url": webhook_url(state, &r.webhook_key),
        "verify_token": r.verify_token,
        "webhook_working": r.last_webhook_at.is_some(),
        "last_webhook_at": r.last_webhook_at,
        "phone": {
            "display_phone_number": r.display_phone_number, "verified_name": r.verified_name,
            "quality_rating": r.quality_rating, "messaging_limit": r.messaging_limit, "checked_at": r.checked_at,
        },
        "error": r.error, "error_at": r.error_at,
        "signup_keywords": r.signup_keywords,
        "signup_reply": r.signup_reply,
        "optin_buttons": r.optin_buttons,
        "test_numbers": super::testers::list(&state.db, account_id).await?,
        "templates": {"total": templates.0, "approved": templates.1},
        "embedded_signup_available": false,
    }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub waba_id: Option<String>,
    pub phone_number_id: Option<String>,
    /// Leave out to keep the saved token.
    pub access_token: Option<String>,
    /// Leave out to keep the saved app secret.
    pub app_secret: Option<String>,
    /// Words that sign someone up to WhatsApp marketing when sent on WhatsApp.
    pub signup_keywords: Option<Vec<String>>,
    /// The answer to a sign-up word (empty: a standard one).
    pub signup_reply: Option<String>,
    /// Quick-reply button payloads that mean "yes, send me WhatsApp messages".
    pub optin_buttons: Option<Vec<String>>,
}

fn id_field(v: &str, what: &str) -> AppResult<String> {
    let v = v.trim();
    if !graph::valid_id(v) {
        return Err(AppError::bad(format!("{what} is a number from Meta (digits only)")));
    }
    Ok(v.to_string())
}

fn hint(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

/// Save connection changes. Returns the names of what changed (for the audit log; never values).
pub async fn update(state: &AppState, account_id: Uuid, b: SettingsBody) -> AppResult<Vec<&'static str>> {
    ensure_row(&state.db, account_id).await?;
    let waba = b.waba_id.as_deref().map(|v| id_field(v, "The WhatsApp Business Account id")).transpose()?;
    let phone = b.phone_number_id.as_deref().map(|v| id_field(v, "The phone number id")).transpose()?;
    let token = match b.access_token.as_deref().map(str::trim) {
        Some(t) if t.len() < 20 || t.len() > 1000 || t.chars().any(char::is_whitespace) => return Err(AppError::bad("That doesn't look like a Meta access token")),
        Some(t) => Some((crate::crypto::encrypt(&state.cfg.secret_key, t).map_err(AppError::Internal)?, hint(t))),
        None => None,
    };
    let secret = match b.app_secret.as_deref().map(str::trim) {
        Some(s) if s.len() < 16 || s.len() > 200 || !s.chars().all(|c| c.is_ascii_alphanumeric()) => return Err(AppError::bad("That doesn't look like a Meta app secret (Meta app → App settings → Basic)")),
        Some(s) => Some(crate::crypto::encrypt(&state.cfg.secret_key, s).map_err(AppError::Internal)?),
        None => None,
    };
    let words = match &b.signup_keywords {
        Some(list) => {
            if list.len() > MAX_SIGNUP_WORDS {
                return Err(AppError::bad(format!("up to {MAX_SIGNUP_WORDS} sign-up words")));
            }
            let mut out: Vec<String> = Vec::new();
            for w in list {
                let w = crate::features::inbox::keywords::normalize(w)?;
                if !out.contains(&w) {
                    out.push(w);
                }
            }
            Some(out)
        }
        None => None,
    };
    let buttons = match &b.optin_buttons {
        Some(list) => {
            if list.len() > MAX_SIGNUP_WORDS {
                return Err(AppError::bad(format!("up to {MAX_SIGNUP_WORDS} opt-in buttons")));
            }
            let out: Vec<String> = list.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if out.iter().any(|s| s.chars().count() > 128) {
                return Err(AppError::bad("Opt-in button payloads are up to 128 characters"));
            }
            Some(out)
        }
        None => None,
    };
    let reply = b.signup_reply.as_deref().map(str::trim).map(|s| s.chars().take(1000).collect::<String>());
    let mut changed = Vec::new();
    for (name, set) in [
        ("waba_id", waba.is_some()),
        ("phone_number_id", phone.is_some()),
        ("access_token", token.is_some()),
        ("app_secret", secret.is_some()),
        ("signup_keywords", words.is_some()),
        ("signup_reply", reply.is_some()),
        ("optin_buttons", buttons.is_some()),
    ] {
        if set {
            changed.push(name);
        }
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    let mut tx = state.db.begin().await?;
    type Old = (Option<String>, Option<String>, Option<String>, Option<String>, Option<Uuid>);
    let (old_waba, old_phone, old_token, old_secret, old_sender): Old =
        sqlx::query_as("SELECT waba_id, phone_number_id, access_token_enc, app_secret_enc, sender_id FROM whatsapp_settings WHERE account_id = $1 FOR UPDATE")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
    let new_waba = waba.clone().or(old_waba.clone());
    let new_phone = phone.clone().or(old_phone.clone());
    let complete = new_waba.is_some() && new_phone.is_some() && (token.is_some() || old_token.is_some()) && (secret.is_some() || old_secret.is_some());
    let same_sender = new_waba == old_waba && new_phone == old_phone;
    let sender = match (complete, old_sender) {
        (false, _) => None,
        (true, Some(s)) if same_sender => Some(s),
        (true, _) => Some(Uuid::now_v7()),
    };
    let connection_changed = waba.is_some() || phone.is_some() || token.is_some();
    let res = sqlx::query(
        "UPDATE whatsapp_settings SET waba_id = COALESCE($2, waba_id), phone_number_id = COALESCE($3, phone_number_id),
                access_token_enc = COALESCE($4, access_token_enc), access_token_hint = COALESCE($5, access_token_hint),
                app_secret_enc = COALESCE($6, app_secret_enc), signup_keywords = COALESCE($7, signup_keywords),
                signup_reply = CASE WHEN $8::text IS NULL THEN signup_reply ELSE NULLIF($8, '') END, optin_buttons = COALESCE($9, optin_buttons),
                sender_id = $10,
                error = CASE WHEN $11 THEN NULL ELSE error END, error_at = CASE WHEN $11 THEN NULL ELSE error_at END,
                checked_at = CASE WHEN $3 IS NOT NULL AND $3 IS DISTINCT FROM phone_number_id THEN NULL ELSE checked_at END,
                updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .bind(&waba)
    .bind(&phone)
    .bind(token.as_ref().map(|t| &t.0))
    .bind(token.as_ref().map(|t| &t.1))
    .bind(&secret)
    .bind(&words)
    .bind(&reply)
    .bind(&buttons)
    .bind(sender)
    .bind(connection_changed)
    .execute(&mut *tx)
    .await;
    match res {
        Err(sqlx::Error::Database(e)) if e.constraint() == Some("whatsapp_settings_phone_uq") => {
            return Err(AppError::Conflict("Another DumbMail account already uses this WhatsApp phone number".into()));
        }
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    // A different business account or number is a different sender: nothing carries over.
    if old_sender.is_some() && !same_sender {
        consent::sender_changed(&mut tx, &state.db, account_id, new_waba != old_waba).await?;
    }
    tx.commit().await?;
    if sender.is_some() && new_waba != old_waba {
        crate::jobs::enqueue_background(&state.db, "whatsapp_sync_templates", json!({"account_id": account_id}), None).await?;
    }
    Ok(changed)
}

/// Forget the connection (ids, token, app secret and what was read from Meta) and the sender with
/// it, in one transaction: queued sends are skipped and claims not yet handed off can't go. Templates
/// stay, so reconnecting the same business account finds them.
pub async fn disconnect(state: &AppState, account_id: Uuid) -> AppResult<()> {
    ensure_row(&state.db, account_id).await?;
    let mut tx = state.db.begin().await?;
    let old: Option<Uuid> = sqlx::query_scalar("SELECT sender_id FROM whatsapp_settings WHERE account_id = $1 FOR UPDATE").bind(account_id).fetch_one(&mut *tx).await?;
    sqlx::query(
        "UPDATE whatsapp_settings SET waba_id = NULL, phone_number_id = NULL, access_token_enc = NULL, access_token_hint = NULL, app_secret_enc = NULL, sender_id = NULL,
                display_phone_number = NULL, verified_name = NULL, quality_rating = NULL, messaging_limit = NULL, checked_at = NULL, error = NULL, error_at = NULL,
                updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    if old.is_some() {
        consent::sender_changed(&mut tx, &state.db, account_id, false).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A new verify token (the old one stops working for Meta's webhook check).
pub async fn rotate_verify_token(state: &AppState, account_id: Uuid) -> AppResult<()> {
    ensure_row(&state.db, account_id).await?;
    sqlx::query("UPDATE whatsapp_settings SET verify_token = $2, updated_at = now() WHERE account_id = $1").bind(account_id).bind(crate::auth::random_token(24)).execute(&state.db).await?;
    Ok(())
}

/// Read the phone number's name, quality rating and messaging limit from Meta and keep them.
pub async fn check(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let Some((_, phone_id, token)) = load_reader(state, account_id).await? else {
        return Err(AppError::bad("Enter the WhatsApp Business Account id, phone number id and access token first"));
    };
    match graph::phone_number(&state.cfg, &token, &phone_id).await {
        Ok(v) => {
            let s = |k: &str| v[k].as_str().map(|s| s.chars().take(100).collect::<String>());
            sqlx::query(
                "UPDATE whatsapp_settings SET display_phone_number = $2, verified_name = $3, quality_rating = $4, messaging_limit = $5, checked_at = now(),
                        error = NULL, error_at = NULL WHERE account_id = $1",
            )
            .bind(account_id)
            .bind(s("display_phone_number"))
            .bind(s("verified_name"))
            .bind(s("quality_rating"))
            .bind(s("whatsapp_business_manager_messaging_limit"))
            .execute(&state.db)
            .await?;
        }
        Err(e) => {
            record_error(&state.db, account_id, &e.describe()).await?;
        }
    }
    settings_json(state, account_id).await
}

/// Keep an account-level problem Meta reported (shown in settings).
pub async fn record_error(db: &sqlx::PgPool, account_id: Uuid, what: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE whatsapp_settings SET error = $2, error_at = now(), checked_at = now() WHERE account_id = $1").bind(account_id).bind(what).execute(db).await?;
    Ok(())
}

/// The account's sign-up words, reply and opt-in buttons.
pub async fn signup(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<(Vec<String>, Option<String>, Vec<String>)> {
    Ok(sqlx::query_as("SELECT signup_keywords, signup_reply, optin_buttons FROM whatsapp_settings WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .unwrap_or_default())
}

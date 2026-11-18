//! The account's WhatsApp connection: its WhatsApp Business Account id, phone number id, a
//! permanent system-user access token and its Meta app's app secret (both encrypted, never shown
//! again or logged), the webhook address and verify token to paste into the Meta app, the team's
//! test numbers, and what the last "Check connection" read from Meta.
//!
//! Embedded Signup (when DumbMail is a Meta Tech Provider) would fill the same columns from the
//! signup's answer, with `connected_via = 'embedded_signup'`; nothing else changes.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::graph;
use crate::error::{AppError, AppResult};
use crate::models::normalize_phone;
use crate::state::AppState;

pub const MAX_TEST_NUMBERS: usize = 10;

/// A usable connection, with its secrets decrypted (never serialized or logged).
pub struct Conn {
    pub account_id: Uuid,
    pub waba_id: String,
    pub phone_number_id: String,
    pub token: String,
    pub app_secret: Option<String>,
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
    display_phone_number: Option<String>,
    verified_name: Option<String>,
    quality_rating: Option<String>,
    messaging_limit: Option<String>,
    checked_at: Option<DateTime<Utc>>,
    error: Option<String>,
    error_at: Option<DateTime<Utc>>,
    test_numbers: Vec<String>,
    last_webhook_at: Option<DateTime<Utc>>,
}

const COLS: &str = "waba_id, phone_number_id, access_token_enc, access_token_hint, app_secret_enc, verify_token, webhook_key, connected_via, display_phone_number,
    verified_name, quality_rating, messaging_limit, checked_at, error, error_at, test_numbers, last_webhook_at";

/// The account's settings row, made (with a fresh verify token and webhook address) the first time.
async fn row(db: &sqlx::PgPool, account_id: Uuid) -> AppResult<Row> {
    sqlx::query("INSERT INTO whatsapp_settings (account_id, verify_token, webhook_key) VALUES ($1, $2, $3) ON CONFLICT (account_id) DO NOTHING")
        .bind(account_id)
        .bind(crate::auth::random_token(24))
        .bind(crate::auth::random_token(18))
        .execute(db)
        .await?;
    Ok(sqlx::query_as::<_, Row>(&format!("SELECT {COLS} FROM whatsapp_settings WHERE account_id = $1")).bind(account_id).fetch_one(db).await?)
}

pub fn webhook_url(state: &AppState, key: &str) -> String {
    format!("{}/webhooks/whatsapp/{key}", state.cfg.public_url())
}

/// The connection, if it's complete (ids and token).
pub async fn load(state: &AppState, account_id: Uuid) -> AppResult<Option<Conn>> {
    let r: Option<(Option<String>, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT waba_id, phone_number_id, access_token_enc, app_secret_enc FROM whatsapp_settings WHERE account_id = $1")
            .bind(account_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((Some(waba_id), Some(phone_number_id), Some(token_enc), secret_enc)) = r else { return Ok(None) };
    let token = crate::crypto::decrypt(&state.cfg.secret_key, &token_enc).map_err(AppError::Internal)?;
    let app_secret = match secret_enc {
        Some(s) => Some(crate::crypto::decrypt(&state.cfg.secret_key, &s).map_err(AppError::Internal)?),
        None => None,
    };
    Ok(Some(Conn { account_id, waba_id, phone_number_id, token, app_secret }))
}

/// Whether WhatsApp is connected (ids and token saved).
pub async fn connected(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM whatsapp_settings WHERE account_id = $1 AND waba_id IS NOT NULL AND phone_number_id IS NOT NULL AND access_token_enc IS NOT NULL)")
        .bind(account_id)
        .fetch_one(db)
        .await
}

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let r = row(&state.db, account_id).await?;
    let connected = r.waba_id.is_some() && r.phone_number_id.is_some() && r.access_token_enc.is_some();
    let templates: (i64, i64) = sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE status = 'APPROVED') FROM whatsapp_templates WHERE account_id = $1 AND status <> 'DELETED'")
        .bind(account_id)
        .fetch_one(&state.db)
        .await?;
    Ok(json!({
        "connected": connected,
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
        "test_numbers": r.test_numbers,
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
    pub test_numbers: Option<Vec<String>>,
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
    row(&state.db, account_id).await?;
    let mut changed = Vec::new();
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
    let tests = match &b.test_numbers {
        Some(list) => {
            if list.len() > MAX_TEST_NUMBERS {
                return Err(AppError::bad(format!("up to {MAX_TEST_NUMBERS} test numbers")));
            }
            let mut out: Vec<String> = Vec::new();
            for raw in list {
                let n = normalize_phone(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't a phone number (use +15035550100)")))?;
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            Some(out)
        }
        None => None,
    };
    if waba.is_some() {
        changed.push("waba_id");
    }
    if phone.is_some() {
        changed.push("phone_number_id");
    }
    if token.is_some() {
        changed.push("access_token");
    }
    if secret.is_some() {
        changed.push("app_secret");
    }
    if tests.is_some() {
        changed.push("test_numbers");
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    let connection_changed = waba.is_some() || phone.is_some() || token.is_some();
    let res = sqlx::query(
        "UPDATE whatsapp_settings SET waba_id = COALESCE($2, waba_id), phone_number_id = COALESCE($3, phone_number_id),
                access_token_enc = COALESCE($4, access_token_enc), access_token_hint = COALESCE($5, access_token_hint),
                app_secret_enc = COALESCE($6, app_secret_enc), test_numbers = COALESCE($7, test_numbers),
                error = CASE WHEN $8 THEN NULL ELSE error END, error_at = CASE WHEN $8 THEN NULL ELSE error_at END,
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
    .bind(&tests)
    .bind(connection_changed)
    .execute(&state.db)
    .await;
    match res {
        Err(sqlx::Error::Database(e)) if e.constraint() == Some("whatsapp_settings_phone_uq") => {
            Err(AppError::Conflict("Another DumbMail account already uses this WhatsApp phone number".into()))
        }
        Err(e) => Err(e.into()),
        Ok(_) => Ok(changed),
    }
}

/// Forget the connection (ids, token, app secret and what was read from Meta). Templates stay, so
/// reconnecting the same account finds them; nothing can be sent until it's connected again.
pub async fn disconnect(state: &AppState, account_id: Uuid) -> AppResult<()> {
    sqlx::query(
        "UPDATE whatsapp_settings SET waba_id = NULL, phone_number_id = NULL, access_token_enc = NULL, access_token_hint = NULL, app_secret_enc = NULL,
                display_phone_number = NULL, verified_name = NULL, quality_rating = NULL, messaging_limit = NULL, checked_at = NULL, error = NULL, error_at = NULL,
                updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// A new verify token (the old one stops working for Meta's webhook check).
pub async fn rotate_verify_token(state: &AppState, account_id: Uuid) -> AppResult<()> {
    row(&state.db, account_id).await?;
    sqlx::query("UPDATE whatsapp_settings SET verify_token = $2, updated_at = now() WHERE account_id = $1").bind(account_id).bind(crate::auth::random_token(24)).execute(&state.db).await?;
    Ok(())
}

/// Read the phone number's name, quality rating and messaging limit from Meta and keep them.
pub async fn check(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let Some(c) = load(state, account_id).await? else {
        return Err(AppError::bad("Enter the WhatsApp Business Account id, phone number id and access token first"));
    };
    match graph::phone_number(&state.cfg, &c.token, &c.phone_number_id).await {
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

/// The account's test numbers.
pub async fn test_numbers(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT test_numbers FROM whatsapp_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?.unwrap_or_default())
}

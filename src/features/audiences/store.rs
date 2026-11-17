//! Connections and syncs in the database, and access tokens kept fresh.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use super::platforms::{self, PErr, Platform};
use crate::state::AppState;

#[derive(Debug, Clone, FromRow)]
pub struct ConnRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub platform: String,
    pub status: String,
    pub connected_as: Option<String>,
    pub access_token_enc: Option<String>,
    pub refresh_token_enc: Option<String>,
    pub token_hint: Option<String>,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub connected_at: Option<DateTime<Utc>>,
}

impl ConnRow {
    pub fn platform(&self) -> Platform {
        Platform::parse(&self.platform).unwrap_or(Platform::Meta)
    }
}

pub const CONN_COLS: &str = "id, account_id, platform, status, connected_as, access_token_enc, refresh_token_enc, token_hint, token_expires_at, error, connected_at";

pub async fn connection(db: &sqlx::PgPool, account_id: Uuid, p: Platform) -> sqlx::Result<Option<ConnRow>> {
    sqlx::query_as(&format!("SELECT {CONN_COLS} FROM ad_connections WHERE account_id = $1 AND platform = $2")).bind(account_id).bind(p.as_str()).fetch_optional(db).await
}

pub async fn connection_by_id(db: &sqlx::PgPool, id: Uuid) -> sqlx::Result<Option<ConnRow>> {
    sqlx::query_as(&format!("SELECT {CONN_COLS} FROM ad_connections WHERE id = $1")).bind(id).fetch_optional(db).await
}

/// Last four characters, so people can tell tokens apart without seeing them.
pub fn hint(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

/// Save a finished sign-in (or a reconnect, which keeps the row so existing syncs carry on).
pub async fn save_tokens(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, p: Platform, t: &platforms::Tokens) -> anyhow::Result<Uuid> {
    let key = &state.cfg.secret_key;
    let access = crate::crypto::encrypt(key, &t.access)?;
    let refresh = t.refresh.as_deref().map(|r| crate::crypto::encrypt(key, r)).transpose()?;
    let expires = t.expires_in.filter(|s| *s > 0).map(|s| Utc::now() + Duration::seconds(s));
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ad_connections (id, account_id, platform, status, connected_as, access_token_enc, refresh_token_enc, token_hint, token_expires_at, error, connected_by, connected_at)
         VALUES ($1, $2, $3, 'connected', $4, $5, $6, $7, $8, NULL, $9, now())
         ON CONFLICT (account_id, platform) DO UPDATE SET status = 'connected', connected_as = EXCLUDED.connected_as, access_token_enc = EXCLUDED.access_token_enc,
             refresh_token_enc = COALESCE(EXCLUDED.refresh_token_enc, ad_connections.refresh_token_enc), token_hint = EXCLUDED.token_hint,
             token_expires_at = EXCLUDED.token_expires_at, error = NULL, connected_by = EXCLUDED.connected_by, connected_at = now(), updated_at = now()
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(p.as_str())
    .bind(&t.connected_as)
    .bind(access)
    .bind(refresh)
    .bind(hint(&t.access))
    .bind(expires)
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(id)
}

pub async fn mark_needs_reconnect(db: &sqlx::PgPool, conn_id: Uuid, msg: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE ad_connections SET status = 'needs_reconnect', error = $2, updated_at = now() WHERE id = $1 AND status = 'connected'").bind(conn_id).bind(msg).execute(db).await?;
    Ok(())
}

/// A usable access token, refreshed first when it expires within five minutes (Google's last an
/// hour). An expired or revoked sign-in marks the connection for reconnecting.
pub async fn access_token(state: &AppState, c: &ConnRow) -> Result<String, PErr> {
    let p = c.platform();
    let reconnect = || PErr::Auth(format!("{} is not connected. Reconnect it in Settings → Ad audiences.", p.label()));
    if c.status != "connected" {
        return Err(reconnect());
    }
    let key = &state.cfg.secret_key;
    let access = c.access_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(key, t).ok()).ok_or_else(reconnect)?;
    let due = c.token_expires_at.is_some_and(|e| e < Utc::now() + Duration::minutes(5));
    if !due {
        return Ok(access);
    }
    if p != Platform::Google {
        // Meta's long-lived tokens are renewed by the scheduler well before they expire (see
        // `refresh_meta_tokens`); one that has run out needs a new sign-in.
        if c.token_expires_at.is_some_and(|e| e < Utc::now()) {
            let msg = format!("{} sign-in expired. Reconnect it in Settings → Ad audiences.", p.label());
            let _ = mark_needs_reconnect(&state.db, c.id, &msg).await;
            return Err(PErr::Auth(msg));
        }
        return Ok(access);
    }
    let refresh = c.refresh_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(key, t).ok());
    match platforms::refresh(state, p, &access, refresh.as_deref()).await {
        Ok(t) => {
            store_refreshed(state, c.id, &t).await.map_err(|e| PErr::Transient(format!("couldn't save the refreshed token: {e}")))?;
            Ok(t.access)
        }
        Err(PErr::Auth(m)) => {
            let _ = mark_needs_reconnect(&state.db, c.id, &m).await;
            Err(PErr::Auth(m))
        }
        Err(e) => Err(e),
    }
}

async fn store_refreshed(state: &AppState, conn_id: Uuid, t: &platforms::Tokens) -> anyhow::Result<()> {
    let key = &state.cfg.secret_key;
    let access = crate::crypto::encrypt(key, &t.access)?;
    let refresh = t.refresh.as_deref().map(|r| crate::crypto::encrypt(key, r)).transpose()?;
    let expires = t.expires_in.filter(|s| *s > 0).map(|s| Utc::now() + Duration::seconds(s));
    sqlx::query(
        "UPDATE ad_connections SET access_token_enc = $2, refresh_token_enc = COALESCE($3, refresh_token_enc), token_hint = $4, token_expires_at = $5, error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(conn_id)
    .bind(access)
    .bind(refresh)
    .bind(hint(&t.access))
    .bind(expires)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Scheduler: trade Meta long-lived tokens for new ones in their last week (they last about 60 days).
pub async fn refresh_meta_tokens(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<ConnRow> = sqlx::query_as(&format!(
        "SELECT {CONN_COLS} FROM ad_connections WHERE platform = 'meta' AND status = 'connected' AND token_expires_at < now() + interval '7 days' AND updated_at < now() - interval '1 hour' LIMIT 50"
    ))
    .fetch_all(&state.db)
    .await?;
    for c in due {
        let Some(access) = c.access_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(&state.cfg.secret_key, t).ok()) else { continue };
        match platforms::refresh(state, Platform::Meta, &access, None).await {
            Ok(t) => store_refreshed(state, c.id, &t).await?,
            Err(PErr::Auth(m)) => mark_needs_reconnect(&state.db, c.id, &m).await?,
            Err(e) => {
                tracing::warn!(connection = %c.id, error = e.message(), "couldn't renew a Meta token; will try again");
                sqlx::query("UPDATE ad_connections SET updated_at = now() WHERE id = $1").bind(c.id).execute(&state.db).await?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct SyncRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub connection_id: Uuid,
    pub platform: String,
    pub ad_account_id: String,
    pub ad_account_name: Option<String>,
    pub login_account_id: Option<String>,
    pub source_type: String,
    pub source_id: Uuid,
    pub name: String,
    pub external_audience_id: Option<String>,
    pub include_unsubscribed: bool,
    pub match_fields: String,
    pub interval_hours: i32,
    pub status: String,
    pub current_run_id: Option<Uuid>,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub members_sent: i32,
    pub last_added: i32,
    pub last_removed: i32,
    pub last_invalid: i32,
    pub platform_size: Option<i64>,
    pub platform_size_upper: Option<i64>,
    pub match_rate: Option<f32>,
    pub platform_status: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SyncRow {
    pub fn platform(&self) -> Platform {
        Platform::parse(&self.platform).unwrap_or(Platform::Meta)
    }
    pub fn with_location(&self) -> bool {
        self.match_fields == "contact_and_location"
    }
}

pub const SYNC_COLS: &str = "s.id, s.account_id, s.connection_id, s.platform, s.ad_account_id, s.ad_account_name, s.login_account_id, s.source_type, s.source_id, s.name, s.external_audience_id, \
    s.include_unsubscribed, s.match_fields, s.interval_hours, s.status, s.current_run_id, s.next_run_at, s.last_run_at, s.last_success_at, s.last_error, s.members_sent, s.last_added, \
    s.last_removed, s.last_invalid, s.platform_size, s.platform_size_upper, s.match_rate, s.platform_status, s.created_at, s.updated_at";

pub async fn sync(db: &sqlx::PgPool, id: Uuid) -> sqlx::Result<Option<SyncRow>> {
    sqlx::query_as(&format!("SELECT {SYNC_COLS} FROM ad_audience_syncs s WHERE s.id = $1")).bind(id).fetch_optional(db).await
}

pub async fn account_sync(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> sqlx::Result<Option<SyncRow>> {
    sqlx::query_as(&format!("SELECT {SYNC_COLS} FROM ad_audience_syncs s WHERE s.id = $1 AND s.account_id = $2")).bind(id).bind(account_id).fetch_optional(db).await
}

/// The sync as the API shows it.
pub fn sync_json(s: &SyncRow, source_name: Option<&str>, connection_status: Option<&str>) -> Value {
    let p = s.platform();
    json!({
        "id": s.id,
        "platform": p.as_str(),
        "platform_label": p.label(),
        "ad_account_id": s.ad_account_id,
        "ad_account_name": s.ad_account_name,
        "login_account_id": s.login_account_id,
        "source": {"type": s.source_type, "id": s.source_id, "name": source_name, "exists": source_name.is_some()},
        "name": s.name,
        "audience_id": s.external_audience_id,
        "status": s.status,
        "running": s.current_run_id.is_some(),
        "connection_status": connection_status.unwrap_or("disconnected"),
        "include_unsubscribed": s.include_unsubscribed,
        "match_fields": s.match_fields,
        "interval_hours": s.interval_hours,
        "next_run_at": (s.status == "active").then_some(s.next_run_at),
        "last_run_at": s.last_run_at,
        "last_success_at": s.last_success_at,
        "error": s.last_error,
        "members_sent": s.members_sent,
        "last_added": s.last_added,
        "last_removed": s.last_removed,
        "last_invalid": s.last_invalid,
        "platform_size": s.platform_size,
        "platform_size_upper": s.platform_size_upper,
        "match_rate": s.match_rate,
        "platform_status": s.platform_status,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

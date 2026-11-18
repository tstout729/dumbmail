//! Connections, settings and syncs in the database, and access tokens kept fresh.
//!
//! Every connection has a generation that goes up on each connect and disconnect. A token refresh,
//! a "needs reconnect" mark or a revoke result writes only if the generation it started under is
//! still current, so a slow refresh can't overwrite a reconnect or bring back tokens after a
//! disconnect.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use super::platforms::{self, Kind, PErr, Platform};
use crate::state::AppState;

#[derive(Debug, Clone, FromRow)]
pub struct ConnRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub platform: String,
    pub status: String,
    pub generation: i64,
    pub connected_as: Option<String>,
    pub access_token_enc: Option<String>,
    pub refresh_token_enc: Option<String>,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
    pub revoke_status: Option<String>,
    pub connected_at: Option<DateTime<Utc>>,
}

impl ConnRow {
    pub fn platform(&self) -> Platform {
        Platform::parse(&self.platform).unwrap_or(Platform::Meta)
    }
}

pub const CONN_COLS: &str = "id, account_id, platform, status, generation, connected_as, access_token_enc, refresh_token_enc, token_expires_at, error_code, revoke_status, connected_at";

pub async fn connection(db: &sqlx::PgPool, account_id: Uuid, p: Platform) -> sqlx::Result<Option<ConnRow>> {
    sqlx::query_as(&format!("SELECT {CONN_COLS} FROM ad_connections WHERE account_id = $1 AND platform = $2")).bind(account_id).bind(p.as_str()).fetch_optional(db).await
}

pub async fn connection_by_id(db: &sqlx::PgPool, id: Uuid) -> sqlx::Result<Option<ConnRow>> {
    sqlx::query_as(&format!("SELECT {CONN_COLS} FROM ad_connections WHERE id = $1")).bind(id).fetch_optional(db).await
}

/// Save a finished sign-in. A reconnect keeps the row (so existing syncs carry on), starts a new
/// generation and replaces both tokens: it never inherits the old refresh token.
pub async fn save_tokens(state: &AppState, account_id: Uuid, user_id: Uuid, p: Platform, t: &platforms::Tokens) -> anyhow::Result<Uuid> {
    let key = &state.cfg.secret_key;
    let access = crate::crypto::encrypt(key, &t.access)?;
    let refresh = t.refresh.as_deref().map(|r| crate::crypto::encrypt(key, r)).transpose()?;
    let expires = t.expires_in.filter(|s| *s > 0).map(|s| Utc::now() + Duration::seconds(s));
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ad_connections (id, account_id, platform, status, connected_as, access_token_enc, refresh_token_enc, token_expires_at, connected_by, connected_at)
         VALUES ($1, $2, $3, 'connected', $4, $5, $6, $7, $8, now())
         ON CONFLICT (account_id, platform) DO UPDATE SET status = 'connected', generation = ad_connections.generation + 1, connected_as = EXCLUDED.connected_as,
             access_token_enc = EXCLUDED.access_token_enc, refresh_token_enc = EXCLUDED.refresh_token_enc, token_expires_at = EXCLUDED.token_expires_at,
             error_code = NULL, revoke_status = NULL, connected_by = EXCLUDED.connected_by, connected_at = now(), updated_at = now()
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(p.as_str())
    .bind(&t.connected_as)
    .bind(access)
    .bind(refresh)
    .bind(expires)
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(id)
}

/// Mark the connection for reconnecting, if it's still the generation that failed.
pub async fn mark_needs_reconnect(db: &sqlx::PgPool, conn_id: Uuid, generation: i64, code: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE ad_connections SET status = 'needs_reconnect', error_code = $3, updated_at = now() WHERE id = $1 AND generation = $2 AND status = 'connected'")
        .bind(conn_id)
        .bind(generation)
        .bind(code)
        .execute(db)
        .await?;
    Ok(())
}

/// The connection changed (reconnected or disconnected) while a refresh was in flight.
pub const CONNECTION_CHANGED: PErr = PErr { kind: Kind::Transient, code: "network" };

/// A usable access token, refreshed first when it expires within five minutes (Google's last an
/// hour). An expired or revoked sign-in marks the connection for reconnecting.
pub async fn access_token(state: &AppState, c: &ConnRow) -> Result<String, PErr> {
    let p = c.platform();
    let reconnect = PErr { kind: Kind::Auth, code: "token_expired" };
    if c.status != "connected" {
        return Err(reconnect);
    }
    let key = &state.cfg.secret_key;
    let access = c.access_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(key, t).ok()).ok_or(reconnect)?;
    let due = c.token_expires_at.is_some_and(|e| e < Utc::now() + Duration::minutes(5));
    if !due {
        return Ok(access);
    }
    if p != Platform::Google {
        // Meta's long-lived tokens aren't renewed on the server; the settings page asks people to
        // reconnect before they run out. TikTok's don't expire.
        if c.token_expires_at.is_some_and(|e| e < Utc::now()) {
            let _ = mark_needs_reconnect(&state.db, c.id, c.generation, "token_expired").await;
            return Err(reconnect);
        }
        return Ok(access);
    }
    let Some(refresh) = c.refresh_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(key, t).ok()) else {
        let _ = mark_needs_reconnect(&state.db, c.id, c.generation, "token_expired").await;
        return Err(reconnect);
    };
    match platforms::refresh_google(state, &refresh).await {
        Ok(t) => {
            let stored = store_refreshed(state, c, &t).await.map_err(|_| CONNECTION_CHANGED)?;
            if !stored {
                return Err(CONNECTION_CHANGED);
            }
            Ok(t.access)
        }
        Err(e) if e.kind == Kind::Auth => {
            let _ = mark_needs_reconnect(&state.db, c.id, c.generation, e.code).await;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// Save refreshed tokens if the connection is still the generation the refresh started from.
async fn store_refreshed(state: &AppState, c: &ConnRow, t: &platforms::Tokens) -> anyhow::Result<bool> {
    let key = &state.cfg.secret_key;
    let access = crate::crypto::encrypt(key, &t.access)?;
    let refresh = t.refresh.as_deref().map(|r| crate::crypto::encrypt(key, r)).transpose()?;
    let expires = t.expires_in.filter(|s| *s > 0).map(|s| Utc::now() + Duration::seconds(s));
    let n = sqlx::query(
        "UPDATE ad_connections SET access_token_enc = $3, refresh_token_enc = COALESCE($4, refresh_token_enc), token_expires_at = $5, error_code = NULL, updated_at = now()
         WHERE id = $1 AND generation = $2 AND status = 'connected'",
    )
    .bind(c.id)
    .bind(c.generation)
    .bind(access)
    .bind(refresh)
    .bind(expires)
    .execute(&state.db)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// What a disconnect did: the tokens are always deleted here; `revoked` says whether the platform
/// confirmed it revoked DumbMail's access.
pub struct Disconnected {
    pub revoked: bool,
}

/// Delete the tokens (starting a new generation, so nothing in flight can write them back), then
/// ask the platform to revoke them and record whether it confirmed.
pub async fn disconnect(state: &AppState, account_id: Uuid, p: Platform) -> anyhow::Result<Option<Disconnected>> {
    let row: Option<(Uuid, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "WITH old AS (SELECT id, access_token_enc, refresh_token_enc FROM ad_connections WHERE account_id = $1 AND platform = $2 AND status <> 'disconnected' FOR UPDATE)
         UPDATE ad_connections c SET status = 'disconnected', generation = c.generation + 1, access_token_enc = NULL, refresh_token_enc = NULL, token_expires_at = NULL,
             error_code = NULL, revoke_status = NULL, updated_at = now()
         FROM old WHERE c.id = old.id
         RETURNING c.id, c.generation, old.access_token_enc, old.refresh_token_enc",
    )
    .bind(account_id)
    .bind(p.as_str())
    .fetch_optional(&state.db)
    .await?;
    let Some((id, generation, access, refresh)) = row else { return Ok(None) };
    let key = &state.cfg.secret_key;
    let access = access.and_then(|t| crate::crypto::decrypt(key, &t).ok());
    let refresh = refresh.and_then(|t| crate::crypto::decrypt(key, &t).ok());
    let revoked = match access.as_deref() {
        Some(a) => match platforms::revoke(state, p, a, refresh.as_deref()).await {
            Ok(()) => true,
            Err(e) => {
                tracing::info!(platform = p.as_str(), code = e.code, "the platform didn't confirm revoking DumbMail's access");
                false
            }
        },
        None => false,
    };
    sqlx::query("UPDATE ad_connections SET revoke_status = $3 WHERE id = $1 AND generation = $2")
        .bind(id)
        .bind(generation)
        .bind(if revoked { "confirmed" } else { "not_confirmed" })
        .execute(&state.db)
        .await?;
    Ok(Some(Disconnected { revoked }))
}

// ---------- Settings ----------

#[derive(Debug, Clone, Default, FromRow)]
pub struct Settings {
    pub google_consent_at: Option<DateTime<Utc>>,
    pub google_consent_by_email: Option<String>,
}

pub async fn settings(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<Settings> {
    Ok(sqlx::query_as("SELECT google_consent_at, google_consent_by_email FROM ad_audience_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?.unwrap_or_default())
}

/// The SQL test that an account has confirmed Google's consent requirement.
pub const GOOGLE_CONSENT_SQL: &str = "EXISTS (SELECT 1 FROM ad_audience_settings st WHERE st.account_id = s.account_id AND st.google_consent_at IS NOT NULL)";

// ---------- Syncs ----------

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
    pub audience_created_at: Option<DateTime<Utc>>,
    pub interval_hours: i32,
    pub status: String,
    pub pause_reason: Option<String>,
    /// Being deleted (status `removing`): also delete the audience on the platform once everyone
    /// DumbMail uploaded is confirmed removed.
    pub delete_audience: bool,
    pub current_run_id: Option<Uuid>,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    pub sent_count: i32,
    pub confirmed_count: i32,
    pub pending_removal_count: i32,
    pub last_added: i32,
    pub last_removed: i32,
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
    /// Being deleted: every run withdraws everyone DumbMail uploaded, and the sync goes once the
    /// platform confirms it.
    pub fn removing(&self) -> bool {
        self.status == "removing"
    }
}

pub const SYNC_COLS: &str = "s.id, s.account_id, s.connection_id, s.platform, s.ad_account_id, s.ad_account_name, s.login_account_id, s.source_type, s.source_id, s.name, \
    s.external_audience_id, s.audience_created_at, s.interval_hours, s.status, s.pause_reason, s.delete_audience, s.current_run_id, s.next_run_at, s.last_run_at, \
    s.last_success_at, s.last_error_code, s.sent_count, s.confirmed_count, s.pending_removal_count, s.last_added, s.last_removed, s.platform_size, \
    s.platform_size_upper, s.match_rate, s.platform_status, s.created_at, s.updated_at";

pub async fn sync(db: &sqlx::PgPool, id: Uuid) -> sqlx::Result<Option<SyncRow>> {
    sqlx::query_as(&format!("SELECT {SYNC_COLS} FROM ad_audience_syncs s WHERE s.id = $1")).bind(id).fetch_optional(db).await
}

pub async fn account_sync(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> sqlx::Result<Option<SyncRow>> {
    sqlx::query_as(&format!("SELECT {SYNC_COLS} FROM ad_audience_syncs s WHERE s.id = $1 AND s.account_id = $2")).bind(id).bind(account_id).fetch_optional(db).await
}

/// Plain words for a sync that's being deleted.
pub fn removing_words(s: &SyncRow) -> Option<String> {
    if !s.removing() {
        return None;
    }
    let l = s.platform().label();
    let then = if s.delete_audience || s.platform() == Platform::Tiktok {
        format!("then deletes the audience on {l} and this sync")
    } else {
        format!("then deletes this sync and leaves the empty audience on {l}")
    };
    Some(format!(
        "Removing: DumbMail is taking everyone it uploaded out of the audience on {l}, and {then}. Until {l} confirms every removal ({} still to go), the sync stays here and DumbMail keeps trying.",
        s.pending_removal_count + s.sent_count
    ))
}

/// Plain words for why a sync is paused.
pub fn pause_words(s: &SyncRow) -> Option<String> {
    let l = s.platform().label();
    if s.status != "paused" {
        return None;
    }
    Some(match s.pause_reason.as_deref()? {
        "user" => "Paused. DumbMail adds nobody, but still removes people who leave or opt out.".into(),
        "google_consent" => "Paused: Google syncs need an owner or admin to confirm the advertising consent setting in Settings → Ad audiences.".into(),
        "tiktok_minimum" => "Paused: below TikTok's minimum of 1,000 people. DumbMail deleted the TikTok audience so nobody who left or opted out stays in it, and makes a new one when 1,000 people qualify again.".into(),
        "audience_missing" => format!("Paused: {l} says this audience no longer exists. Create a new audience to carry on."),
        "create_unclear" => format!(
            "Paused: DumbMail asked {l} to create the audience \"{}\" and couldn't read the answer, so it may exist without DumbMail knowing its ID. Check {l} and delete it if it's there, then resume.",
            s.name
        ),
        other => format!("Paused ({other})."),
    })
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
        "audience_created_at": s.audience_created_at,
        "status": s.status,
        "pause_reason": s.pause_reason,
        "pause_message": pause_words(s),
        "removing": s.removing(),
        "removing_message": removing_words(s),
        "delete_audience": s.delete_audience,
        "running": s.current_run_id.is_some(),
        "connection_status": connection_status.unwrap_or("disconnected"),
        "interval_hours": s.interval_hours,
        "next_run_at": s.next_run_at,
        "last_run_at": s.last_run_at,
        "last_success_at": s.last_success_at,
        "error_code": s.last_error_code,
        "error": s.last_error_code.as_deref().map(|c| super::sync::error_words(p, c)),
        "sent": s.sent_count,
        "confirmed": s.confirmed_count,
        "pending_removal": s.pending_removal_count,
        "last_added": s.last_added,
        "last_removed": s.last_removed,
        "platform_size": s.platform_size,
        "platform_size_upper": s.platform_size_upper,
        "match_rate": s.match_rate,
        "platform_status": s.platform_status,
        "verified_live": false,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

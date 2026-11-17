//! Abuse limits for transactional mail, which skips marketing consent: per-account caps per
//! minute and per 24 hours, and an automatic pause (with an operator alert) when the volume looks
//! like a marketing blast rather than mail people triggered.

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::send::SendBody;
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

pub const DEFAULT_PER_MINUTE: i64 = 120;
pub const DEFAULT_DAILY: i64 = 5000;

/// A spike: at least this many sends in the last hour...
const SPIKE_MIN: i64 = 500;
/// ...and this many times the account's usual hourly volume over the past week.
const SPIKE_FACTOR: f64 = 10.0;
/// Never-engaged volume: at least this many sends in 24 hours...
const COLD_MIN: i64 = 300;
/// ...several times the account's usual day...
const COLD_FACTOR: f64 = 5.0;
/// ...almost all to people with no activity besides the mail itself.
const COLD_SHARE: f64 = 0.9;

#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub per_minute: i64,
    pub daily: i64,
}

pub async fn caps(db: &PgPool, account_id: Uuid) -> sqlx::Result<Caps> {
    let row: Option<(Option<i32>, Option<i32>)> = sqlx::query_as("SELECT per_minute_limit, daily_limit FROM transactional_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?;
    let (m, d) = row.unwrap_or_default();
    Ok(Caps { per_minute: m.map_or(DEFAULT_PER_MINUTE, i64::from), daily: d.map_or(DEFAULT_DAILY, i64::from) })
}

/// The transactional pause reason, when transactional sending is paused for the account.
pub async fn paused(db: &PgPool, account_id: Uuid) -> sqlx::Result<Option<String>> {
    let row: Option<(Option<chrono::DateTime<chrono::Utc>>, Option<String>)> =
        sqlx::query_as("SELECT paused_at, paused_reason FROM transactional_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?;
    Ok(row.and_then(|(at, why)| at.map(|_| why.unwrap_or_else(|| "unusual volume".into()))))
}

/// Refuse when transactional sending, or all email for the account, is paused.
pub async fn check_paused(state: &AppState, account: &Account) -> AppResult<()> {
    if let Some(reason) = crate::deliverability::paused_reason(&state.db, account.id).await? {
        return Err(AppError::Conflict(crate::deliverability::paused_message(reason.as_deref())));
    }
    if let Some(why) = paused(&state.db, account.id).await? {
        return Err(AppError::Conflict(format!("Transactional email is paused for this account ({why}). Contact support to resume.")));
    }
    Ok(())
}

/// [`check_paused`], plus the caps (without reserving anything).
pub async fn check_open(state: &AppState, account: &Account) -> AppResult<()> {
    check_paused(state, account).await?;
    let c = caps(&state.db, account.id).await?;
    let (minute, day) = usage(&state.db, account.id).await?;
    cap_error(c, minute, day)
}

fn cap_error(c: Caps, minute: i64, day: i64) -> AppResult<()> {
    if minute >= c.per_minute {
        return Err(AppError::LimitReached(format!("transactional email is limited to {} a minute for this account; slow down and retry", c.per_minute)));
    }
    if day >= c.daily {
        return Err(AppError::LimitReached(format!("transactional email is limited to {} in 24 hours for this account; contact support to raise it", c.daily)));
    }
    Ok(())
}

/// Sends in the last minute and the last 24 hours.
async fn usage<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - interval '1 minute'), count(*)
         FROM transactional_sends WHERE account_id = $1 AND created_at > now() - interval '24 hours'",
    )
    .bind(account_id)
    .fetch_one(db)
    .await
}

/// Record a send that's about to go out, inside the caps. One at a time per account, so concurrent
/// requests can't overshoot a cap or both claim one idempotency key. Returns false when another
/// request already holds the key.
pub async fn reserve(state: &AppState, account_id: Uuid, id: Uuid, to: &str, template_id: Option<Uuid>, b: &SendBody, hash: &str, via: &str, resent_from: Option<Uuid>) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('transactional:' || $1::text, 0))").bind(account_id).execute(&mut *tx).await?;
    let c = caps(&state.db, account_id).await?;
    let (minute, day) = usage(&mut *tx, account_id).await?;
    cap_error(c, minute, day)?;
    let inserted = sqlx::query(
        "INSERT INTO transactional_sends (id, account_id, to_email, template_id, idempotency_key, request_hash, request, metadata, track_clicks, via, resent_from)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (account_id, idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING",
    )
    .bind(id)
    .bind(account_id)
    .bind(to)
    .bind(template_id)
    .bind(&b.idempotency_key)
    .bind(hash)
    .bind(serde_json::to_value(b)?)
    .bind(json!(b.metadata.clone().unwrap_or_default()))
    .bind(b.track_clicks)
    .bind(via)
    .bind(resent_from)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(inserted == 1)
}

/// Why this account's recent transactional volume looks like marketing, if it does.
pub async fn anomaly(db: &PgPool, account_id: Uuid) -> sqlx::Result<Option<String>> {
    let (last_hour, week): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - interval '1 hour'),
                count(*) FILTER (WHERE created_at <= now() - interval '1 hour')
         FROM transactional_sends WHERE account_id = $1 AND created_at > now() - interval '7 days' AND status IN ('sent','queued')",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    let usual = (week as f64 / (7.0 * 24.0 - 1.0)).max(1.0);
    if last_hour >= SPIKE_MIN && last_hour as f64 > SPIKE_FACTOR * usual {
        return Ok(Some(format!("{last_hour} transactional emails in the last hour, against about {usual:.0} an hour usually")));
    }
    let passive: Vec<String> = crate::deliverability::PASSIVE_METRICS.iter().map(|s| s.to_string()).collect();
    let (day, cold, before): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE t.created_at > now() - interval '24 hours'),
                count(*) FILTER (WHERE t.created_at > now() - interval '24 hours' AND NOT EXISTS (
                SELECT 1 FROM events e JOIN metrics mt ON mt.id = e.metric_id
                WHERE e.profile_id = t.profile_id AND mt.name <> ALL($2))),
                count(*) FILTER (WHERE t.created_at <= now() - interval '24 hours')
         FROM transactional_sends t WHERE t.account_id = $1 AND t.created_at > now() - interval '8 days' AND t.status IN ('sent','queued')",
    )
    .bind(account_id)
    .bind(&passive)
    .fetch_one(db)
    .await?;
    let usual_day = (before as f64 / 7.0).max(1.0);
    if day >= COLD_MIN && day as f64 > COLD_FACTOR * usual_day && cold as f64 >= COLD_SHARE * day as f64 {
        return Ok(Some(format!("{cold} of {day} transactional emails in the last 24 hours went to people with no other activity")));
    }
    Ok(None)
}

/// Pause transactional sending for the account and alert the operator. Returns true when this call paused it.
pub async fn pause(state: &AppState, account_id: Uuid, reason: &str) -> anyhow::Result<bool> {
    let paused: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO transactional_settings (account_id, paused_at, paused_reason) VALUES ($1, now(), $2)
         ON CONFLICT (account_id) DO UPDATE SET paused_at = now(), paused_reason = $2, updated_at = now() WHERE transactional_settings.paused_at IS NULL
         RETURNING account_id",
    )
    .bind(account_id)
    .bind(reason)
    .fetch_optional(&state.db)
    .await?;
    if paused.is_none() {
        return Ok(false);
    }
    let name: String = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    tracing::error!(account = %account_id, reason, "TRANSACTIONAL EMAIL PAUSED for unusual volume; lift with POST /admin/transactional/unpause");
    use crate::deliverability::plain;
    let text = format!(
        "Transactional email for an account was paused automatically.\n\nAccount name: {}\nAccount id: {account_id}\nReason: {}\n\n\
         Transactional email skips marketing consent, so volume like this may be marketing sent through it. Their campaigns and flows are not affected.\n\
         Once you've checked it, lift the pause with:\n\n  curl -X POST {}/admin/transactional/unpause -H \"Authorization: Bearer $ADMIN_TOKEN\" -H 'Content-Type: application/json' -d '{{\"account_id\":\"{account_id}\"}}'\n",
        plain(&name, 120),
        plain(reason, 300),
        state.cfg.public_url()
    );
    crate::deliverability::email_operator(state, account_id, &format!("Transactional email paused: {}", plain(&name, 60)), text).await;
    Ok(true)
}

/// Scheduler: check accounts that sent transactional mail in the last few minutes.
pub async fn check_recent(state: &AppState) -> anyhow::Result<()> {
    let accounts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT t.account_id FROM transactional_sends t
         WHERE t.created_at > now() - interval '10 minutes'
           AND NOT EXISTS (SELECT 1 FROM transactional_settings s WHERE s.account_id = t.account_id AND s.paused_at IS NOT NULL)
         LIMIT 1000",
    )
    .fetch_all(&state.db)
    .await?;
    for a in accounts {
        if let Some(why) = anomaly(&state.db, a).await? {
            pause(state, a, &why).await?;
        }
    }
    Ok(())
}

pub async fn settings_json(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let c = caps(db, account_id).await?;
    let (minute, day) = usage(db, account_id).await?;
    Ok(json!({
        "per_minute_limit": c.per_minute, "daily_limit": c.daily,
        "sent_last_minute": minute, "sent_last_24h": day,
        "paused": paused(db, account_id).await?.is_some(), "paused_reason": paused(db, account_id).await?,
    }))
}

/// Operator: lift the transactional pause. Returns whether it was paused.
pub async fn unpause(db: &PgPool, account_id: Uuid) -> sqlx::Result<bool> {
    let n = sqlx::query("UPDATE transactional_settings SET paused_at = NULL, paused_reason = NULL, updated_at = now() WHERE account_id = $1 AND paused_at IS NOT NULL")
        .bind(account_id)
        .execute(db)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Operator: set the caps (None returns a cap to its default).
pub async fn set_caps(db: &PgPool, account_id: Uuid, per_minute: Option<i32>, daily: Option<i32>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO transactional_settings (account_id, per_minute_limit, daily_limit) VALUES ($1, $2, $3)
         ON CONFLICT (account_id) DO UPDATE SET per_minute_limit = $2, daily_limit = $3, updated_at = now()",
    )
    .bind(account_id)
    .bind(per_minute)
    .bind(daily)
    .execute(db)
    .await?;
    Ok(())
}

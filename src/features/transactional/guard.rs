//! Who may send transactional mail (which skips marketing consent) and how much: it's off until the
//! account owner turns it on, restricted to saved templates on the hosted service until the operator
//! grants full mode, capped per minute and per 24 hours, and paused automatically (with an operator
//! alert) when the volume looks like a marketing blast rather than mail people triggered.

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

pub const DEFAULT_PER_MINUTE: i64 = 120;
pub const DEFAULT_DAILY: i64 = 5000;
/// Restricted mode's daily cap when the operator hasn't set one: TRANSACTIONAL_RESTRICTED_DAILY_LIMIT, or 200.
pub static RESTRICTED_DAILY: Lazy<i64> = Lazy::new(|| {
    std::env::var("TRANSACTIONAL_RESTRICTED_DAILY_LIMIT").ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n >= 0).unwrap_or(200)
});

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

/// The account's settings row (defaults when it has none).
#[derive(Debug, Clone, Default)]
struct Row {
    per_minute_limit: Option<i32>,
    daily_limit: Option<i32>,
    enabled_at: Option<chrono::DateTime<chrono::Utc>>,
    full_mode: bool,
    paused_at: Option<chrono::DateTime<chrono::Utc>>,
    paused_reason: Option<String>,
}

async fn row<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> sqlx::Result<Row> {
    let r: Option<(Option<i32>, Option<i32>, Option<chrono::DateTime<chrono::Utc>>, bool, Option<chrono::DateTime<chrono::Utc>>, Option<String>)> = sqlx::query_as(
        "SELECT per_minute_limit, daily_limit, enabled_at, full_mode, paused_at, paused_reason FROM transactional_settings WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    Ok(r.map(|(per_minute_limit, daily_limit, enabled_at, full_mode, paused_at, paused_reason)| Row { per_minute_limit, daily_limit, enabled_at, full_mode, paused_at, paused_reason }).unwrap_or_default())
}

/// What the account may do with transactional email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// Inline content allowed (self-hosted, or full mode granted by the operator). Otherwise only
    /// saved transactional templates.
    pub full: bool,
    pub per_minute: i64,
    pub daily: i64,
}

impl Access {
    fn of(state: &AppState, r: &Row) -> Self {
        let full = !state.cfg.hosted || r.full_mode;
        let daily_default = if full { DEFAULT_DAILY } else { *RESTRICTED_DAILY };
        Access { full, per_minute: r.per_minute_limit.map_or(DEFAULT_PER_MINUTE, i64::from), daily: r.daily_limit.map_or(daily_default, i64::from) }
    }

    pub fn mode(&self) -> &'static str {
        if self.full { "full" } else { "restricted" }
    }

    /// Refuse what this mode doesn't allow.
    pub fn allows(&self, state: &AppState, inline_content: bool) -> AppResult<()> {
        if inline_content && !self.full {
            let ask = state.cfg.support_email.as_deref().map_or("support".to_string(), |e| e.to_string());
            return Err(AppError::Forbidden(format!(
                "This account's transactional email is in restricted mode: it can only send saved transactional templates (template_id), not inline content. Ask {ask} for full mode."
            )));
        }
        Ok(())
    }
}

fn off_message() -> String {
    "Transactional email is off for this account. The account owner can turn it on in Settings → Transactional.".into()
}

/// The one place that decides whether this caller may send transactional email, and how. Today that's
/// any signed-in member or API key of an account that turned it on; when API keys get scopes, the
/// send permission is checked here.
pub async fn may_send_transactional(state: &AppState, ctx: &Ctx) -> AppResult<Access> {
    access(state, ctx.account_id).await
}

/// The account's access, or an error when transactional email is off.
pub async fn access(state: &AppState, account_id: Uuid) -> AppResult<Access> {
    let r = row(&state.db, account_id).await?;
    if r.enabled_at.is_none() {
        return Err(AppError::Forbidden(off_message()));
    }
    Ok(Access::of(state, &r))
}

/// The transactional pause reason, when transactional sending is paused for the account.
pub async fn paused(db: &PgPool, account_id: Uuid) -> sqlx::Result<Option<String>> {
    let r = row(db, account_id).await?;
    Ok(r.paused_at.map(|_| r.paused_reason.unwrap_or_else(|| "unusual volume".into())))
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
pub async fn check_open(state: &AppState, account: &Account, a: &Access) -> AppResult<()> {
    check_paused(state, account).await?;
    let (minute, day) = usage(&state.db, account.id).await?;
    cap_error(a, minute, day)
}

fn cap_error(a: &Access, minute: i64, day: i64) -> AppResult<()> {
    if minute >= a.per_minute {
        return Err(AppError::LimitReached(format!("transactional email is limited to {} a minute for this account; slow down and retry", a.per_minute)));
    }
    if day >= a.daily {
        return Err(AppError::LimitReached(format!("transactional email is limited to {} in 24 hours for this account; contact support to raise it", a.daily)));
    }
    Ok(())
}

/// Sends in the last minute and the last 24 hours.
pub async fn usage<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - interval '1 minute'), count(*)
         FROM transactional_sends WHERE account_id = $1 AND created_at > now() - interval '24 hours'",
    )
    .bind(account_id)
    .fetch_one(db)
    .await
}

/// A send about to be recorded.
pub struct Reservation<'a> {
    pub id: Uuid,
    /// The message row created with it (None when the address is suppressed and nothing will go out).
    pub message_id: Option<Uuid>,
    pub to: &'a str,
    pub profile_id: Uuid,
    pub template_id: Option<Uuid>,
    pub subject: &'a str,
    pub idempotency_key: Option<&'a str>,
    pub hash: &'a str,
    pub metadata: Value,
    pub track_clicks: bool,
    pub via: &'a str,
    /// Why it won't be sent (hard bounce or complaint): recorded as skipped.
    pub skipped: Option<String>,
    /// The encrypted retry payload.
    pub payload_enc: Option<String>,
}

pub enum Reserved {
    New,
    /// An earlier request with the same idempotency key, within 24 hours.
    Replay(Uuid),
}

/// The earlier send for an idempotency key within 24 hours, if any. A key reused for a different
/// request is an error; a key older than 24 hours is released for reuse.
pub async fn find_key(conn: &mut sqlx::PgConnection, account_id: Uuid, key: &str, hash: &str) -> AppResult<Option<Uuid>> {
    let row: Option<(Uuid, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id, request_hash, created_at FROM transactional_sends WHERE account_id = $1 AND idempotency_key = $2")
            .bind(account_id)
            .bind(key)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((id, prior_hash, created)) = row else { return Ok(None) };
    if created < chrono::Utc::now() - chrono::Duration::hours(24) {
        sqlx::query("UPDATE transactional_sends SET idempotency_key = NULL WHERE id = $1").bind(id).execute(&mut *conn).await?;
        return Ok(None);
    }
    if prior_hash != hash {
        return Err(AppError::Conflict("this idempotency_key was already used for a different email in the last 24 hours".into()));
    }
    Ok(Some(id))
}

/// Record a send, inside the caps, in one transaction with its message row and the job that sends it
/// if this request never gets to (a crash right after). One reservation at a time per account, so
/// concurrent requests can't overshoot a cap; the idempotency key is looked up under that lock, so a
/// retry of an accepted request is answered with the first result, never refused by the cap.
pub async fn reserve(state: &AppState, account_id: Uuid, a: &Access, r: Reservation<'_>) -> AppResult<Reserved> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('transactional:' || $1::text, 0))").bind(account_id).execute(&mut *tx).await?;
    if let Some(key) = r.idempotency_key {
        if let Some(prior) = find_key(&mut tx, account_id, key, r.hash).await? {
            tx.commit().await?;
            return Ok(Reserved::Replay(prior));
        }
    }
    let (minute, day) = usage(&mut *tx, account_id).await?;
    cap_error(a, minute, day)?;
    let status = if r.skipped.is_some() { "skipped" } else { "queued" };
    let inserted = sqlx::query(
        "INSERT INTO transactional_sends (id, account_id, message_id, profile_id, to_email, template_id, subject, status, error, idempotency_key, request_hash, request, metadata, track_clicks, via, payload_enc)
         VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10, '{}', $11, $12, $13, $14)
         ON CONFLICT (account_id, idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING",
    )
    .bind(r.id)
    .bind(account_id)
    .bind(r.profile_id)
    .bind(r.to)
    .bind(r.template_id)
    .bind(r.subject)
    .bind(status)
    .bind(&r.skipped)
    .bind(r.idempotency_key)
    .bind(r.hash)
    .bind(&r.metadata)
    .bind(r.track_clicks)
    .bind(r.via)
    .bind(&r.payload_enc)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted != 1 {
        return Err(AppError::Conflict("another request with this idempotency_key is being handled; retry in a moment".into()));
    }
    if let (Some(mid), None) = (r.message_id, &r.skipped) {
        sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address, subject, transactional) VALUES ($1, $2, $3, 'email', $4, $5, true)")
            .bind(mid)
            .bind(account_id)
            .bind(r.profile_id)
            .bind(r.to)
            .bind(r.subject)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE transactional_sends SET message_id = $2 WHERE id = $1").bind(r.id).bind(mid).execute(&mut *tx).await?;
        let backstop = chrono::Utc::now() + chrono::Duration::seconds(super::send::BACKSTOP_SECS);
        crate::jobs::enqueue(&mut *tx, super::RETRY_JOB, json!({"send_id": r.id}), Some(backstop)).await?;
    }
    tx.commit().await?;
    Ok(Reserved::New)
}

/// Why this account's recent transactional volume looks like marketing, if it does.
pub async fn anomaly(db: &PgPool, account_id: Uuid) -> sqlx::Result<Option<String>> {
    let (last_hour, week): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - interval '1 hour'),
                count(*) FILTER (WHERE created_at <= now() - interval '1 hour')
         FROM transactional_sends WHERE account_id = $1 AND created_at > now() - interval '7 days' AND status IN ('sent','queued','sending','unknown')",
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
         FROM transactional_sends t WHERE t.account_id = $1 AND t.created_at > now() - interval '8 days' AND t.status IN ('sent','queued','sending','unknown')",
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

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let r = row(&state.db, account_id).await?;
    let a = Access::of(state, &r);
    let (minute, day) = usage(&state.db, account_id).await?;
    let mode = if r.enabled_at.is_none() { "off" } else { a.mode() };
    Ok(json!({
        "enabled": r.enabled_at.is_some(), "enabled_at": r.enabled_at, "mode": mode,
        "hosted": state.cfg.hosted,
        "full_mode_granted": a.full,
        "per_minute_limit": a.per_minute, "daily_limit": a.daily,
        "sent_last_minute": minute, "sent_last_24h": day,
        "paused": r.paused_at.is_some(), "paused_reason": r.paused_at.map(|_| r.paused_reason.clone().unwrap_or_else(|| "unusual volume".into())),
        "support_email": state.cfg.support_email,
    }))
}

/// Owner: turn transactional email on (after accepting what it's for) or off.
pub async fn set_enabled(state: &AppState, ctx: &Ctx, enabled: bool, acknowledged: bool) -> AppResult<Value> {
    if enabled {
        let Some(user_id) = ctx.user_id.filter(|_| !ctx.via_api_key) else {
            return Err(AppError::Forbidden("only the account owner can turn on transactional email, signed in to the dashboard".into()));
        };
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE user_id = $1 AND account_id = $2").bind(user_id).bind(ctx.account_id).fetch_optional(&state.db).await?;
        if role.as_deref() != Some("owner") {
            return Err(AppError::Forbidden("only the account owner can turn on transactional email".into()));
        }
        if !acknowledged {
            return Err(AppError::bad("confirm that transactional email is only for messages a person triggered (receipts, password resets, shipping notices), never marketing: send acknowledge: true"));
        }
        sqlx::query(
            "INSERT INTO transactional_settings (account_id, enabled_at, enabled_by) VALUES ($1, now(), $2)
             ON CONFLICT (account_id) DO UPDATE SET enabled_at = COALESCE(transactional_settings.enabled_at, now()), enabled_by = COALESCE(transactional_settings.enabled_by, $2), updated_at = now()",
        )
        .bind(ctx.account_id)
        .bind(user_id)
        .execute(&state.db)
        .await?;
        tracing::warn!(account = %ctx.account_id, user = %user_id, "transactional email turned on");
    } else {
        sqlx::query("UPDATE transactional_settings SET enabled_at = NULL, enabled_by = NULL, updated_at = now() WHERE account_id = $1").bind(ctx.account_id).execute(&state.db).await?;
    }
    settings_json(state, ctx.account_id).await
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

/// Operator: grant or take back full mode (inline content and the normal caps) on the hosted service.
pub async fn set_full_mode(db: &PgPool, account_id: Uuid, full: bool) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO transactional_settings (account_id, full_mode) VALUES ($1, $2)
         ON CONFLICT (account_id) DO UPDATE SET full_mode = $2, updated_at = now()",
    )
    .bind(account_id)
    .bind(full)
    .execute(db)
    .await?;
    Ok(())
}

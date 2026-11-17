//! Automatic email pause. Every customer sends through one Amazon SES account, which AWS puts under
//! review at a 5% bounce rate or 0.1% complaint rate (and can suspend at ~10% / 0.5%). An account whose
//! recent email crosses tighter limits stops sending email until an operator lifts the pause.
//!
//! Rates are checked when SES reports a bounce or complaint (at most every few seconds per account;
//! anything skipped is picked up by the scheduler), both for the account's last 7 days and for the
//! campaign the message belonged to, so one bad list stops partway through its first send.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

/// Rate limits for one scope (an account's recent email, or one campaign).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Emails accepted by the provider before rates are judged, so one bounce can't pause a small sender.
    pub min_sent: i64,
    /// Pause when hard bounces / sent is above this.
    pub max_bounce_rate: f64,
    /// Pause when complaints / sent is above this (and there are at least `MIN_COMPLAINTS`).
    pub max_complaint_rate: f64,
}

/// Account: the last `WINDOW_DAYS` of email. Below AWS's review levels (5% / 0.1%) to leave headroom.
pub const ACCOUNT: Limits = Limits { min_sent: 200, max_bounce_rate: 0.04, max_complaint_rate: 0.0008 };
/// Campaign circuit breaker: a send whose first recipients bounce badly (a bought or stale list).
pub const CAMPAIGN: Limits = Limits { min_sent: 100, max_bounce_rate: 0.10, max_complaint_rate: 0.005 };
/// Complaints needed before the complaint rate counts: one or two annoyed recipients never pause anyone.
pub const MIN_COMPLAINTS: i64 = 3;
pub const WINDOW_DAYS: i32 = 7;
/// Per account, rates are recomputed at most this often from webhooks; the scheduler catches up the rest.
const CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Email outcomes in a window, from `email_sends`: `sent` counts every real email the provider accepted
/// (the denominator AWS uses), `bounced` the permanent (hard) bounces and `complained` the spam complaints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct Outcomes {
    pub sent: i64,
    pub bounced: i64,
    pub complained: i64,
}

/// Why these outcomes should pause sending, or None when they're within limits.
pub fn verdict(o: &Outcomes, l: &Limits) -> Option<String> {
    if o.sent < l.min_sent || o.sent <= 0 {
        return None;
    }
    let sent = o.sent as f64;
    let bounce = o.bounced as f64 / sent;
    if bounce > l.max_bounce_rate {
        return Some(format!("hard bounce rate {:.1}% ({} of {} emails)", bounce * 100.0, o.bounced, o.sent));
    }
    let complaint = o.complained as f64 / sent;
    if o.complained >= MIN_COMPLAINTS && complaint > l.max_complaint_rate {
        return Some(format!("spam complaint rate {:.2}% ({} of {} emails)", complaint * 100.0, o.complained, o.sent));
    }
    None
}

/// Emails sent inside the window: the last 7 days, and since the pause was last lifted.
const OUTCOMES_SELECT: &str = "SELECT count(*)::bigint AS sent,
        count(*) FILTER (WHERE s.hard_bounce)::bigint AS bounced,
        count(*) FILTER (WHERE s.complaint)::bigint AS complained
    FROM email_sends s JOIN accounts a ON a.id = s.account_id
    WHERE s.account_id = $1
      AND s.sent_at > GREATEST(now() - make_interval(days => $2), COALESCE(a.sending_window_start, '-infinity'))";

pub async fn account_outcomes(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<Outcomes> {
    sqlx::query_as(OUTCOMES_SELECT).bind(account_id).bind(WINDOW_DAYS).fetch_one(db).await
}

pub async fn campaign_outcomes(db: &sqlx::PgPool, account_id: Uuid, campaign_id: Uuid) -> sqlx::Result<Outcomes> {
    sqlx::query_as(&format!("{OUTCOMES_SELECT} AND s.campaign_id = $3")).bind(account_id).bind(WINDOW_DAYS).bind(campaign_id).fetch_one(db).await
}

/// The provider accepted a real email: count it toward the account's rates and its warm-up limit.
pub async fn record_send(db: &sqlx::PgPool, message_id: Uuid, provider_message_id: &str, account_id: Uuid, campaign_id: Option<Uuid>) -> sqlx::Result<()> {
    insert_send(db, message_id, provider_message_id, account_id, campaign_id, true).await
}

/// A double opt-in confirmation went out: it counts toward bounce and complaint rates, not the warm-up
/// limit (so sign-up forms, or a bot filling them in, can't raise the limit).
pub async fn record_confirmation_send(db: &sqlx::PgPool, message_id: Uuid, provider_message_id: &str, account_id: Uuid) -> sqlx::Result<()> {
    insert_send(db, message_id, provider_message_id, account_id, None, false).await
}

async fn insert_send(db: &sqlx::PgPool, message_id: Uuid, provider_message_id: &str, account_id: Uuid, campaign_id: Option<Uuid>, counts: bool) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO email_sends (message_id, provider_message_id, account_id, campaign_id, counts_toward_limit) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(message_id)
    .bind(provider_message_id)
    .bind(account_id)
    .bind(campaign_id)
    .bind(counts)
    .execute(db)
    .await?;
    Ok(())
}

/// SES reported a hard bounce (`complaint` false) or a complaint. Matched by the provider's message id,
/// so it counts even when the message was deleted with its profile. Returns the send's (account, campaign),
/// or None for mail that isn't a real send (tests, system alerts).
pub async fn record_strike(db: &sqlx::PgPool, provider_message_id: &str, complaint: bool) -> sqlx::Result<Option<(Uuid, Option<Uuid>)>> {
    let q = if complaint {
        "UPDATE email_sends SET complaint = true WHERE provider_message_id = $1 RETURNING account_id, campaign_id"
    } else {
        "UPDATE email_sends SET hard_bounce = true WHERE provider_message_id = $1 RETURNING account_id, campaign_id"
    };
    sqlx::query_as(q).bind(provider_message_id).fetch_optional(db).await
}

/// Scheduler: send records only matter for the 7-day window.
pub async fn prune(db: &sqlx::PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM email_sends WHERE sent_at < now() - interval '30 days'").execute(db).await?;
    Ok(())
}

/// Tenant-chosen text (account and campaign names, emails) made safe to quote in a reason or an alert:
/// one line, no control characters, at most `max` characters.
pub fn plain(s: &str, max: usize) -> String {
    let one_line: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = one_line.trim();
    if t.chars().count() > max { format!("{}…", t.chars().take(max).collect::<String>()) } else { t.to_string() }
}

/// The message customers see on a stopped campaign or a blocked test send.
pub fn paused_message(reason: Option<&str>) -> String {
    match reason {
        Some(r) => format!("Email sending is paused for this account because of a high {r}. Contact support to resume."),
        None => "Email sending is paused for this account because of high bounces or spam complaints. Contact support to resume.".into(),
    }
}

/// Is email sending paused for this account? (Read fresh; the pause can land mid-batch.)
pub async fn paused_reason<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> sqlx::Result<Option<Option<String>>> {
    let row: Option<(Option<chrono::DateTime<chrono::Utc>>, Option<String>)> =
        sqlx::query_as("SELECT sending_paused_at, sending_paused_reason FROM accounts WHERE id = $1").bind(account_id).fetch_optional(db).await?;
    Ok(row.and_then(|(at, reason)| at.map(|_| reason)))
}

/// Recompute rates for the account (and these campaigns) now; pause the account if any is over its limit.
/// Returns true when this call paused it.
pub async fn check(state: &AppState, account_id: Uuid, campaign_ids: &[Uuid]) -> anyhow::Result<bool> {
    if paused_reason(&state.db, account_id).await?.is_some() {
        return Ok(false);
    }
    for &cid in campaign_ids {
        let o = campaign_outcomes(&state.db, account_id, cid).await?;
        if let Some(why) = verdict(&o, &CAMPAIGN) {
            let name: Option<String> = sqlx::query_scalar("SELECT name FROM campaigns WHERE id = $1").bind(cid).fetch_optional(&state.db).await?;
            let reason = format!("{why} on campaign \"{}\"", plain(&name.unwrap_or_else(|| cid.to_string()), 80));
            return pause(state, account_id, &reason).await;
        }
    }
    let o = account_outcomes(&state.db, account_id).await?;
    match verdict(&o, &ACCOUNT) {
        Some(why) => pause(state, account_id, &format!("{why} in the last {WINDOW_DAYS} days")).await,
        None => Ok(false),
    }
}

/// Pause email sending: stop email campaigns in progress (their unsent messages are skipped with a
/// reason) and alert the operator. Flows and test sends are refused at the send path while paused.
/// Scheduled campaigns stay scheduled and stop when they come due, unless the pause is lifted first.
pub async fn pause(state: &AppState, account_id: Uuid, reason: &str) -> anyhow::Result<bool> {
    let name: Option<String> = sqlx::query_scalar(
        "UPDATE accounts SET sending_paused_at = now(), sending_paused_reason = $2 WHERE id = $1 AND sending_paused_at IS NULL RETURNING name",
    )
    .bind(account_id)
    .bind(reason)
    .fetch_optional(&state.db)
    .await?;
    let Some(name) = name else { return Ok(false) };
    tracing::error!(account = %account_id, account_name = %name, reason, "EMAIL SENDING PAUSED for high bounces/complaints; lift with POST /admin/unpause");
    let stopped: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM campaigns WHERE account_id = $1 AND channel = 'email' AND status = 'sending'")
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    let msg = paused_message(Some(reason));
    for cid in stopped {
        stop_campaign(&state.db, cid, &msg).await?;
    }
    alert_operator(state, account_id, &name, reason).await;
    Ok(true)
}

/// Fail a campaign because sending is paused; its queued messages are skipped (never silently dropped).
/// The campaign is stopped first, in the same transaction, so a batch claiming or re-queuing a message
/// at the same moment (see `campaigns::claim`) can't leave one queued behind it.
pub async fn stop_campaign(db: &sqlx::PgPool, campaign_id: Uuid, msg: &str) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE campaigns SET status = 'failed', error = $2, updated_at = now() WHERE id = $1 AND status IN ('sending','scheduled','testing')")
        .bind(campaign_id)
        .bind(msg)
        .execute(&mut *tx)
        .await?;
    let skipped: Vec<(Uuid, Uuid)> = sqlx::query_as("UPDATE messages SET status = 'skipped', error = 'sending paused' WHERE campaign_id = $1 AND status = 'queued' AND NOT is_test RETURNING account_id, id")
        .bind(campaign_id)
        .fetch_all(&mut *tx)
        .await?;
    if let Some((account, _)) = skipped.first() {
        crate::features::messages_unsent_in(&mut tx, *account, &skipped.iter().map(|r| r.1).collect::<Vec<_>>()).await;
    }
    tx.commit().await?;
    Ok(())
}

/// Operator: lift the pause and restart the measuring window, so old bounces don't pause it again.
/// Returns whether the account was paused.
pub async fn unpause(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<bool> {
    let was: Option<bool> = sqlx::query_scalar(
        "UPDATE accounts a SET sending_paused_at = NULL, sending_paused_reason = NULL, sending_window_start = now()
         FROM (SELECT sending_paused_at IS NOT NULL AS was FROM accounts WHERE id = $1) old
         WHERE a.id = $1 RETURNING old.was",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    Ok(was.unwrap_or(false))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Email OPERATOR_ALERT_EMAIL (or SUPPORT_EMAIL) through the platform's email provider. The address
/// sends to itself, so its domain must be verified with the provider. Failures are only logged.
/// Tenant-chosen text is flattened to one line and kept out of the ready-to-run command, which only
/// carries the account id.
async fn alert_operator(state: &AppState, account_id: Uuid, name: &str, reason: &str) {
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let (name, reason) = (plain(name, 120), plain(reason, 300));
    let owner = plain(owner.as_deref().unwrap_or("(unknown)"), 254);
    let text = format!(
        "Email sending for an account was paused automatically.\n\nAccount name: {name}\nAccount id: {account_id}\nOwner email: {owner}\nReason: {reason}\n\n\
         Their email campaigns in progress were stopped and flow emails are skipped until the pause is lifted.\n\
         Once the list is cleaned up, lift it with:\n\n  curl -X POST {}/admin/unpause -H \"Authorization: Bearer $ADMIN_TOKEN\" -H 'Content-Type: application/json' -d '{{\"account_id\":\"{account_id}\"}}'\n",
        state.cfg.public_url()
    );
    email_operator(state, account_id, &format!("Sending paused: {}", plain(&name, 60)), text).await;
}

/// Email the operator (OPERATOR_ALERT_EMAIL, or SUPPORT_EMAIL) a plain-text alert about an account.
/// Callers flatten tenant-chosen text with [`plain`]. Failures are only logged.
pub async fn email_operator(state: &AppState, account_id: Uuid, subject: &str, text: String) {
    let Some(to) = state.cfg.operator_alert_email.as_deref().or(state.cfg.support_email.as_deref()).filter(|s| s.contains('@')) else {
        tracing::error!(account = %account_id, subject, "no OPERATOR_ALERT_EMAIL or SUPPORT_EMAIL set; alert only logged");
        return;
    };
    let html = format!("<pre style=\"font-family:ui-monospace,monospace;white-space:pre-wrap\">{}</pre>", esc(&text));
    let email = OutgoingEmail {
        message_id: Uuid::now_v7(),
        to: to.to_string(),
        from_name: "DumbMail alerts".into(),
        from_email: to.to_string(),
        reply_to: None,
        subject: subject.to_string(),
        html,
        text,
        unsubscribe_url: None,
    };
    if let Err(e) = state.email.send(&email).await {
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email the operator alert");
    }
}

#[derive(Default)]
pub struct Pending {
    last_check: Option<Instant>,
    due: bool,
    campaigns: HashSet<Uuid>,
}

/// Per account: when rates were last checked and what's waiting for the scheduler (lives in `AppState`).
pub type PendingChecks = Mutex<HashMap<Uuid, Pending>>;

/// A bounce or complaint arrived: check now unless this account was checked in the last few seconds,
/// in which case the scheduler's `check_pending` does it shortly.
pub async fn note_outcome(state: &AppState, account_id: Uuid, campaign_id: Option<Uuid>) {
    let campaigns = {
        let Ok(mut map) = state.pending_checks.lock() else { return };
        let p = map.entry(account_id).or_default();
        p.campaigns.extend(campaign_id);
        if p.last_check.is_some_and(|t| t.elapsed() < CHECK_INTERVAL) {
            p.due = true;
            return;
        }
        p.last_check = Some(Instant::now());
        p.due = false;
        std::mem::take(&mut p.campaigns).into_iter().collect::<Vec<_>>()
    };
    if let Err(e) = check(state, account_id, &campaigns).await {
        tracing::warn!(account = %account_id, error = %format!("{e:#}"), "sending rate check failed");
    }
}

/// Scheduler: run the checks that `note_outcome` put off.
pub async fn check_pending(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<(Uuid, Vec<Uuid>)> = {
        let Ok(mut map) = state.pending_checks.lock() else { return Ok(()) };
        map.retain(|_, p| p.due || p.last_check.is_some_and(|t| t.elapsed() < CHECK_INTERVAL * 6));
        map.iter_mut()
            .filter(|(_, p)| p.due)
            .map(|(id, p)| {
                p.due = false;
                p.last_check = Some(Instant::now());
                (*id, std::mem::take(&mut p.campaigns).into_iter().collect())
            })
            .collect()
    };
    for (account_id, campaigns) in due {
        if let Err(e) = check(state, account_id, &campaigns).await {
            tracing::warn!(account = %account_id, error = %format!("{e:#}"), "sending rate check failed");
        }
    }
    Ok(())
}

// ---------- Warm-up: a 24-hour email limit that grows with clean sending ----------
//
// A new account can't send its whole list on day one. It starts at `WARMUP_START` emails per 24 hours,
// and each day the limit becomes twice its busiest finished day (in the account's time zone) of the last 30, as long as its recent bounces and
// complaints stay well under the pause limits. A bought or stale list then shows up (and pauses the
// account) within a few hundred emails instead of tens of thousands. An account that sent nothing for
// 30 days starts over, because a list left alone that long has gone stale. Operators can set a fixed
// limit instead (`POST /admin/sending-limit`).

/// Emails a new account may send in its first 24 hours, unless `WARMUP_START_DAILY` says otherwise.
pub const WARMUP_START: i64 = 500;
/// Days of history the warm-up looks back over for the busiest day.
pub const WARMUP_LOOKBACK_DAYS: i32 = 30;
/// Grows only while recent email is this clean (judged once there are 100 emails to judge).
pub const HEALTHY: Limits = Limits { min_sent: 100, max_bounce_rate: 0.02, max_complaint_rate: 0.0003 };
/// How long a computed limit is reused before it's read again from the database.
const LIMIT_CACHE: Duration = Duration::from_secs(60);
/// The earliest a deferred send tries again, so a full limit doesn't re-run batches every few seconds.
const MIN_RETRY: chrono::Duration = chrono::Duration::minutes(15);

/// The 24-hour limit for an account with this history. `peak` is its busiest finished day in the lookback;
/// today never counts, or the limit could double again and again within one day.
pub fn warmup_limit(start: i64, peak: i64, recent: &Outcomes) -> i64 {
    let healthy = recent.sent < HEALTHY.min_sent
        || (recent.bounced as f64 / recent.sent as f64 <= HEALTHY.max_bounce_rate
            && (recent.complained < 2 || recent.complained as f64 / recent.sent as f64 <= HEALTHY.max_complaint_rate));
    let earned = if healthy { peak.saturating_mul(2) } else { peak };
    earned.max(start)
}

/// An account's 24-hour email limit right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DailyLimit {
    pub limit: i64,
    /// Real emails accepted by the provider in the last 24 hours.
    pub sent_24h: i64,
    /// Set by an operator rather than earned.
    pub fixed: bool,
}

impl DailyLimit {
    pub fn remaining(&self) -> i64 {
        (self.limit - self.sent_24h).max(0)
    }
}

pub async fn daily_limit(state: &AppState, account_id: Uuid) -> sqlx::Result<DailyLimit> {
    let db = &state.db;
    let (fixed, sent_24h, peak): (Option<i32>, i64, i64) = sqlx::query_as(
        "SELECT a.email_daily_limit_override,
            (SELECT count(*) FROM email_sends s WHERE s.account_id = a.id AND s.counts_toward_limit AND s.sent_at > now() - interval '24 hours')::bigint,
            COALESCE((SELECT max(n) FROM (SELECT count(*) AS n FROM email_sends s WHERE s.account_id = a.id AND s.counts_toward_limit
                AND s.sent_at > now() - make_interval(days => $2)
                AND (s.sent_at AT TIME ZONE a.timezone)::date < (now() AT TIME ZONE a.timezone)::date
                GROUP BY (s.sent_at AT TIME ZONE a.timezone)::date) d), 0)::bigint
         FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .bind(WARMUP_LOOKBACK_DAYS)
    .fetch_one(db)
    .await?;
    if let Some(f) = fixed {
        return Ok(DailyLimit { limit: i64::from(f.max(0)), sent_24h, fixed: true });
    }
    let recent = account_outcomes(db, account_id).await?;
    Ok(DailyLimit { limit: warmup_limit(state.cfg.warmup_start_daily.max(1), peak, &recent), sent_24h, fixed: false })
}

#[derive(Debug)]
pub struct CachedLimit {
    read_at: Instant,
    limit: DailyLimit,
    /// Sends recorded since `read_at` (not yet in `limit.sent_24h`).
    since: i64,
    /// Slots taken for emails still being sent (not yet recorded). Kept across re-reads.
    inflight: i64,
    /// Re-read on the next send whatever its age (an operator changed the limit).
    stale: bool,
}

/// Per account: the last limit read and the sends since (lives in `AppState`). This counts within one
/// process; the hosted service runs a single instance.
pub type LimitCache = Mutex<HashMap<Uuid, CachedLimit>>;

/// Take one email from the account's 24-hour limit. Ok(()) means send it; Err(when) means the limit is
/// used up and the email should wait until then. Only real email on the hosted service is limited.
pub async fn take_daily_slot(state: &AppState, account_id: Uuid) -> anyhow::Result<Result<(), chrono::DateTime<chrono::Utc>>> {
    if !state.cfg.hosted {
        return Ok(Ok(()));
    }
    let fresh = {
        let map = state.daily_limits.lock().map_err(|_| anyhow::anyhow!("limit cache poisoned"))?;
        map.get(&account_id).is_some_and(|c| !c.stale && c.read_at.elapsed() < LIMIT_CACHE)
    };
    if !fresh {
        let limit = daily_limit(state, account_id).await?;
        let mut map = state.daily_limits.lock().map_err(|_| anyhow::anyhow!("limit cache poisoned"))?;
        let c = map.entry(account_id).or_insert(CachedLimit { read_at: Instant::now(), limit, since: 0, inflight: 0, stale: false });
        c.read_at = Instant::now();
        c.limit = limit;
        c.since = 0;
        c.stale = false;
    }
    let allowed = {
        let mut map = state.daily_limits.lock().map_err(|_| anyhow::anyhow!("limit cache poisoned"))?;
        let Some(c) = map.get_mut(&account_id) else { return Ok(Ok(())) };
        if c.limit.sent_24h + c.since + c.inflight < c.limit.limit {
            c.inflight += 1;
            true
        } else {
            false
        }
    };
    if allowed {
        return Ok(Ok(()));
    }
    // Room opens up as the oldest sends in the window turn 24 hours old.
    let oldest: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT min(sent_at) FROM email_sends WHERE account_id = $1 AND counts_toward_limit AND sent_at > now() - interval '24 hours'")
            .bind(account_id)
            .fetch_one(&state.db)
            .await?;
    let now = chrono::Utc::now();
    let when = oldest.map(|t| t + chrono::Duration::hours(24)).unwrap_or(now).max(now + MIN_RETRY);
    Ok(Err(when))
}

/// The email that took a slot was sent and recorded.
pub fn complete_daily_slot(state: &AppState, account_id: Uuid) {
    if let Ok(mut map) = state.daily_limits.lock() {
        if let Some(c) = map.get_mut(&account_id) {
            c.inflight = (c.inflight - 1).max(0);
            c.since += 1;
        }
    }
}

/// Give back a slot taken for an email that then wasn't sent (it failed, or the provider was busy).
pub fn return_daily_slot(state: &AppState, account_id: Uuid) {
    if let Ok(mut map) = state.daily_limits.lock() {
        if let Some(c) = map.get_mut(&account_id) {
            c.inflight = (c.inflight - 1).max(0);
        }
    }
}

/// Re-read the limit on the next send (an operator changed it). Emails mid-send stay counted.
pub fn forget_daily_limit(state: &AppState, account_id: Uuid) {
    if let Ok(mut map) = state.daily_limits.lock() {
        if let Some(c) = map.get_mut(&account_id) {
            c.stale = true;
        }
    }
}

// ---------- Unengaged profiles (sunset) ----------

/// Campaign email skips people who got at least `SUNSET_MIN_EMAILS` emails in the last `SUNSET_DAYS` days
/// and did nothing else in that time: no open, click, sign-up, order, site visit or any other event.
/// Mailbox providers judge a sender on whether people engage; mailing people who never do is what slowly
/// sends a domain to spam. Flows are exempt: their triggers (a checkout, a sign-up) are engagement.
pub const SUNSET_DAYS: i32 = 180;
pub const SUNSET_MIN_EMAILS: i64 = 10;
/// Events that don't show the person did anything.
pub const PASSIVE_METRICS: &[&str] = &["Received Email", "Received SMS", "Bounced Email", "Marked Email as Spam", "Unsubscribed", "Unsubscribed from SMS", "Received Transactional Email"];

/// Appends ` AND NOT (<p is unengaged>)` for a query over `profiles p`.
pub fn push_not_unengaged(qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>) {
    qb.push(" AND NOT ((SELECT count(*) FROM (SELECT 1 FROM messages sm WHERE sm.profile_id = p.id AND sm.channel = 'email' AND NOT sm.is_test AND sm.status IN ('sent','delivered','bounced','complained') AND sm.created_at > now() - make_interval(days => ")
        .push_bind(SUNSET_DAYS)
        .push(") LIMIT ")
        .push_bind(SUNSET_MIN_EMAILS)
        .push(") r) >= ")
        .push_bind(SUNSET_MIN_EMAILS)
        .push(" AND NOT EXISTS (SELECT 1 FROM events se JOIN metrics sx ON sx.id = se.metric_id WHERE se.profile_id = p.id AND se.occurred_at > now() - make_interval(days => ")
        .push_bind(SUNSET_DAYS)
        .push(") AND sx.name <> ALL(")
        .push_bind(PASSIVE_METRICS.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .push(")))");
}

// ---------- Risky addresses in imports ----------

/// Shared inboxes: nobody in particular signed up, and they report spam and bounce more often.
const ROLE_LOCAL_PARTS: &[&str] = &[
    "abuse", "admin", "administrator", "billing", "compliance", "devnull", "dns", "ftp", "hostmaster", "info", "inquiries",
    "inquiry", "mailerdaemon", "marketing", "noc", "noreply", "donotreply", "postmaster", "privacy", "root", "sales", "security",
    "spam", "support", "sysadmin", "webmaster", "www", "office", "contact", "help", "helpdesk", "enquiries", "legal", "jobs",
    "careers", "hr", "press", "media", "newsletter", "list", "listserv", "unsubscribe",
];

/// Throwaway inbox services: addresses stop existing within hours and hard bounce.
const DISPOSABLE_DOMAINS: &[&str] = &[
    "mailinator.com", "guerrillamail.com", "guerrillamail.net", "guerrillamail.org", "sharklasers.com", "grr.la", "10minutemail.com",
    "10minutemail.net", "temp-mail.org", "tempmail.com", "tempmail.net", "tempmailo.com", "yopmail.com", "yopmail.net", "trashmail.com",
    "trashmail.de", "getnada.com", "nada.email", "dispostable.com", "maildrop.cc", "throwawaymail.com", "fakeinbox.com", "mintemail.com",
    "mohmal.com", "emailondeck.com", "burnermail.io", "mailnesia.com", "mytemp.email", "tempinbox.com", "spamgourmet.com",
    "getairmail.com", "moakt.com", "tempr.email", "discard.email", "mailcatch.com", "inboxkitten.com", "33mail.com", "emailfake.com",
    "tmpmail.org", "tmpmail.net", "minuteinbox.com", "mail.tm", "linshiyouxiang.net", "spambox.us", "trbvm.com",
];

/// Why an (already normalized) address shouldn't be imported as a marketing contact, if it shouldn't.
pub fn risky_address(email: &str) -> Option<&'static str> {
    let (local, domain) = email.rsplit_once('@')?;
    let local = local.split('+').next().unwrap_or(local);
    let compact: String = local.chars().filter(|c| !matches!(c, '.' | '-' | '_')).collect::<String>().to_lowercase();
    if ROLE_LOCAL_PARTS.contains(&compact.as_str()) {
        return Some("role");
    }
    let domain = domain.to_lowercase();
    if DISPOSABLE_DOMAINS.iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}"))) {
        return Some("disposable");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_starts_small_and_doubles_while_clean() {
        let clean = o(1000, 5, 0);
        assert_eq!(warmup_limit(WARMUP_START, 0, &Outcomes::default()), WARMUP_START, "a new account");
        assert_eq!(warmup_limit(WARMUP_START, 100, &o(100, 0, 0)), WARMUP_START, "never below the start");
        assert_eq!(warmup_limit(WARMUP_START, 500, &clean), 1000);
        assert_eq!(warmup_limit(WARMUP_START, 8000, &clean), 16000);
        // Over 2% bounces or 0.03% complaints: the limit stops growing but doesn't shrink below the best day.
        assert_eq!(warmup_limit(WARMUP_START, 8000, &o(1000, 25, 0)), 8000);
        assert_eq!(warmup_limit(WARMUP_START, 8000, &o(10000, 0, 4)), 8000);
        // One complaint is never enough to stop growth; too few emails are never judged.
        assert_eq!(warmup_limit(WARMUP_START, 8000, &o(1000, 0, 1)), 16000);
        assert_eq!(warmup_limit(WARMUP_START, 500, &o(50, 10, 0)), 1000);
    }

    #[test]
    fn risky_addresses() {
        assert_eq!(risky_address("info@shop.com"), Some("role"));
        assert_eq!(risky_address("no-reply@shop.com"), Some("role"));
        assert_eq!(risky_address("sales+eu@shop.com"), Some("role"));
        assert_eq!(risky_address("jane@mailinator.com"), Some("disposable"));
        assert_eq!(risky_address("jane@eu.mailinator.com"), Some("disposable"));
        assert_eq!(risky_address("jane@shop.com"), None);
        assert_eq!(risky_address("information.jane@shop.com"), None);
        assert_eq!(risky_address("jane@notmailinator.com"), None);
    }

    fn o(sent: i64, bounced: i64, complained: i64) -> Outcomes {
        Outcomes { sent, bounced, complained }
    }

    #[test]
    fn small_senders_are_never_judged() {
        assert_eq!(verdict(&o(199, 199, 0), &ACCOUNT), None, "below the account minimum");
        assert_eq!(verdict(&o(99, 50, 10), &CAMPAIGN), None, "below the campaign minimum");
        assert_eq!(verdict(&o(0, 0, 0), &ACCOUNT), None);
    }

    #[test]
    fn account_bounce_threshold_is_strictly_above_4_percent() {
        assert_eq!(verdict(&o(200, 8, 0), &ACCOUNT), None, "exactly 4% is allowed");
        let why = verdict(&o(200, 9, 0), &ACCOUNT).expect("4.5% pauses");
        assert_eq!(why, "hard bounce rate 4.5% (9 of 200 emails)");
    }

    #[test]
    fn complaints_need_both_the_rate_and_a_minimum_count() {
        // 2 of 200 = 1%, far over 0.08%, but two complaints are not enough to judge.
        assert_eq!(verdict(&o(200, 0, 2), &ACCOUNT), None);
        assert!(verdict(&o(200, 0, 3), &ACCOUNT).unwrap().starts_with("spam complaint rate 1.50%"));
        // 3 of 5,000 = 0.06%: under the limit.
        assert_eq!(verdict(&o(5000, 0, 3), &ACCOUNT), None);
        // 5 of 5,000 = 0.10%: over.
        assert!(verdict(&o(5000, 0, 5), &ACCOUNT).is_some());
    }

    #[test]
    fn campaign_breaker_is_looser_but_trips_on_a_bad_list() {
        assert_eq!(verdict(&o(100, 10, 0), &CAMPAIGN), None, "10% is allowed");
        assert!(verdict(&o(100, 11, 0), &CAMPAIGN).is_some());
        assert_eq!(verdict(&o(1000, 0, 5), &CAMPAIGN), None, "0.5% complaints is allowed");
        assert!(verdict(&o(1000, 0, 6), &CAMPAIGN).is_some());
        // An account-level 5% would pause the account but not trip the campaign breaker.
        assert!(verdict(&o(1000, 50, 0), &ACCOUNT).is_some());
        assert_eq!(verdict(&o(1000, 50, 0), &CAMPAIGN), None);
    }

    #[test]
    fn tenant_text_is_flattened_and_capped() {
        assert_eq!(plain("Acme\r\nBcc: x@evil.test", 100), "Acme  Bcc: x@evil.test");
        assert_eq!(plain("  abcdef ", 3), "abc…");
    }

    #[test]
    fn paused_message_names_the_reason() {
        assert_eq!(
            paused_message(Some("hard bounce rate 12.0% (12 of 100 emails)")),
            "Email sending is paused for this account because of a high hard bounce rate 12.0% (12 of 100 emails). Contact support to resume."
        );
    }
}

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

/// The provider accepted a real email: count it toward the account's rates.
pub async fn record_send(db: &sqlx::PgPool, message_id: Uuid, provider_message_id: &str, account_id: Uuid, campaign_id: Option<Uuid>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO email_sends (message_id, provider_message_id, account_id, campaign_id) VALUES ($1, $2, $3, $4) ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(message_id)
    .bind(provider_message_id)
    .bind(account_id)
    .bind(campaign_id)
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
pub async fn stop_campaign(db: &sqlx::PgPool, campaign_id: Uuid, msg: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE messages SET status = 'skipped', error = 'sending paused' WHERE campaign_id = $1 AND status = 'queued' AND NOT is_test")
        .bind(campaign_id)
        .execute(db)
        .await?;
    sqlx::query("UPDATE campaigns SET status = 'failed', error = $2, updated_at = now() WHERE id = $1 AND status IN ('sending','scheduled','testing')")
        .bind(campaign_id)
        .bind(msg)
        .execute(db)
        .await?;
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
    let Some(to) = state.cfg.operator_alert_email.as_deref().or(state.cfg.support_email.as_deref()).filter(|s| s.contains('@')) else {
        tracing::error!(account = %account_id, "no OPERATOR_ALERT_EMAIL or SUPPORT_EMAIL set; sending pause alert only logged");
        return;
    };
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
    let html = format!("<pre style=\"font-family:ui-monospace,monospace;white-space:pre-wrap\">{}</pre>", esc(&text));
    let email = OutgoingEmail {
        message_id: Uuid::now_v7(),
        to: to.to_string(),
        from_name: "DumbMail alerts".into(),
        from_email: to.to_string(),
        reply_to: None,
        subject: format!("Sending paused: {}", plain(&name, 60)),
        html,
        text,
        unsubscribe_url: None,
    };
    if let Err(e) = state.email.send(&email).await {
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email the sending pause alert");
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

#[cfg(test)]
mod tests {
    use super::*;

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

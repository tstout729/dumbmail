//! How people react, from message data DumbMail already has: about hourly per account, on the
//! background lane. A campaign or a week of campaign email that few people open, or that many
//! unsubscribe from or mark as spam, pauses the account's email (`deliverability::pause`) and puts
//! it in review. Opens include machine opens. DumbMail always tracks opens on campaign email, so
//! no campaign is left out of the open-rate checks.

use serde_json::json;
use uuid::Uuid;

use super::standing::{self, New, Standing};
use super::Verdict;
use crate::deliverability::plain;
use crate::state::AppState;

pub const JOB: &str = "screening_engagement";
/// A campaign is judged once it has this many delivered emails, from 24 hours after its first send.
pub const CAMPAIGN_MIN_DELIVERED: i64 = 300;
pub const CAMPAIGN_MIN_OPEN_RATE: f64 = 0.05;
pub const CAMPAIGN_MAX_UNSUBSCRIBE_RATE: f64 = 0.02;
/// The account's last 7 days of campaign email (delivered over 24 hours ago).
pub const ACCOUNT_MIN_DELIVERED: i64 = 1000;
pub const ACCOUNT_MIN_OPEN_RATE: f64 = 0.08;
/// Campaigns first sent within this many days are looked at.
const LOOKBACK_DAYS: i32 = 14;
/// Accounts per run (the rest wait for the next hour).
const PER_RUN: i64 = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reactions {
    pub delivered: i64,
    pub opened: i64,
    pub unsubscribed: i64,
    pub complained: i64,
}

fn pct(n: i64, of: i64) -> f64 {
    if of <= 0 { 0.0 } else { n as f64 / of as f64 }
}

/// Why a campaign's reactions should pause the account, if they should.
pub fn campaign_verdict(r: &Reactions) -> Option<String> {
    if r.delivered < CAMPAIGN_MIN_DELIVERED {
        return None;
    }
    let open = pct(r.opened, r.delivered);
    if open < CAMPAIGN_MIN_OPEN_RATE {
        return Some(format!("unopened rate {:.1}% ({} of {} delivered emails not opened)", (1.0 - open) * 100.0, r.delivered - r.opened, r.delivered));
    }
    let unsub = pct(r.unsubscribed, r.delivered);
    if unsub > CAMPAIGN_MAX_UNSUBSCRIBE_RATE {
        return Some(format!("unsubscribe rate {:.1}% ({} of {} delivered emails)", unsub * 100.0, r.unsubscribed, r.delivered));
    }
    let l = crate::deliverability::CAMPAIGN;
    let complaint = pct(r.complained, r.delivered);
    if r.complained >= crate::deliverability::MIN_COMPLAINTS && complaint > l.max_complaint_rate {
        return Some(format!("spam complaint rate {:.2}% ({} of {} delivered emails)", complaint * 100.0, r.complained, r.delivered));
    }
    None
}

pub fn account_verdict(r: &Reactions) -> Option<String> {
    if r.delivered < ACCOUNT_MIN_DELIVERED {
        return None;
    }
    let open = pct(r.opened, r.delivered);
    (open < ACCOUNT_MIN_OPEN_RATE).then(|| format!("unopened rate {:.1}% ({} of {} delivered emails not opened)", (1.0 - open) * 100.0, r.delivered - r.opened, r.delivered))
}

const REACTIONS: &str = "count(*) FILTER (WHERE m.status IN ('sent','delivered','complained') OR m.opened_at IS NOT NULL)::bigint,
    count(m.opened_at)::bigint, count(m.unsubscribed_at)::bigint, count(*) FILTER (WHERE m.status = 'complained')::bigint";

pub async fn run_job(state: &AppState) -> anyhow::Result<()> {
    if !super::on(state) {
        return Ok(());
    }
    let accounts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT a.id FROM accounts a
         WHERE a.sending_review = 'approved' AND a.sending_paused_at IS NULL AND a.closed_at IS NULL
           AND EXISTS (SELECT 1 FROM campaigns c WHERE c.account_id = a.id AND c.channel = 'email' AND c.send_at > now() - make_interval(days => $1) AND c.send_at < now() - interval '24 hours')
           AND NOT EXISTS (SELECT 1 FROM screening_engagement_checks e WHERE e.account_id = a.id AND e.checked_at > now() - interval '55 minutes')
         LIMIT $2",
    )
    .bind(LOOKBACK_DAYS)
    .bind(PER_RUN)
    .fetch_all(&state.db)
    .await?;
    for id in accounts {
        if let Err(e) = check_account(state, id).await {
            tracing::warn!(account = %id, error = %format!("{e:#}"), "engagement check failed");
        }
    }
    Ok(())
}

/// Check one account (claimed for the hour in the write, so two runs can't both check it).
pub async fn check_account(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO screening_engagement_checks (account_id, checked_at) VALUES ($1, now())
         ON CONFLICT (account_id) DO UPDATE SET checked_at = now() WHERE screening_engagement_checks.checked_at < now() - interval '55 minutes' RETURNING account_id",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    if claimed.is_none() {
        return Ok(());
    }
    // Campaigns first sent 24 hours to 14 days ago, not already judged, counting only email sent
    // since the pause was last lifted.
    type Row = (Uuid, String, i64, i64, i64, i64);
    let campaigns: Vec<Row> = sqlx::query_as(&format!(
        "SELECT c.id, c.name, {REACTIONS} FROM campaigns c JOIN accounts a ON a.id = c.account_id
           JOIN messages m ON m.campaign_id = c.id AND NOT m.is_test AND m.channel = 'email' AND m.sent_at > COALESCE(a.sending_window_start, '-infinity')
         WHERE c.account_id = $1 AND c.channel = 'email' AND c.send_at > now() - make_interval(days => $2) AND c.send_at < now() - interval '24 hours'
           AND NOT EXISTS (SELECT 1 FROM screenings s WHERE s.account_id = $1 AND s.kind = 'engagement' AND s.subject_id = c.id)
         GROUP BY c.id, c.name ORDER BY min(c.send_at) LIMIT 50"
    ))
    .bind(account_id)
    .bind(LOOKBACK_DAYS)
    .fetch_all(&state.db)
    .await?;
    for (cid, name, delivered, opened, unsubscribed, complained) in campaigns {
        let r = Reactions { delivered, opened, unsubscribed, complained };
        if let Some(why) = campaign_verdict(&r) {
            let reason = format!("{why} on campaign \"{}\"", plain(&name, 80));
            return trip(state, account_id, Some((cid, name)), &reason, &r).await;
        }
    }
    let (delivered, opened, unsubscribed, complained): (i64, i64, i64, i64) = sqlx::query_as(&format!(
        "SELECT {REACTIONS} FROM messages m JOIN accounts a ON a.id = m.account_id
         WHERE m.account_id = $1 AND m.campaign_id IS NOT NULL AND m.channel = 'email' AND NOT m.is_test
           AND m.sent_at > GREATEST(now() - interval '7 days', COALESCE(a.sending_window_start, '-infinity'))
           AND m.sent_at < now() - interval '24 hours'"
    ))
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let r = Reactions { delivered, opened, unsubscribed, complained };
    if let Some(why) = account_verdict(&r) {
        return trip(state, account_id, None, &format!("{why} in the last 7 days of campaign email"), &r).await;
    }
    Ok(())
}

async fn trip(state: &AppState, account_id: Uuid, campaign: Option<(Uuid, String)>, reason: &str, r: &Reactions) -> anyhow::Result<()> {
    let reasons = vec![format!("Email sending was paused because of a high {reason}. People on your list aren't engaging, which hurts delivery for you and everyone else.")];
    let mut s = New::rules("engagement", Verdict::Review, &reasons, json!({"delivered": r.delivered, "opened": r.opened, "unsubscribed": r.unsubscribed, "complained": r.complained, "reason": reason}));
    if let Some((id, name)) = &campaign {
        s.subject = Some("campaign");
        s.subject_id = Some(*id);
        s.subject_name = Some(name.clone());
    }
    standing::record(&state.db, account_id, s).await?;
    crate::deliverability::pause(state, account_id, reason).await?;
    standing::move_to(state, account_id, &[Standing::Approved], Standing::Review, &reasons[0], &format!("Engagement check: {reason}. Email is paused.")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(delivered: i64, opened: i64, unsubscribed: i64, complained: i64) -> Reactions {
        Reactions { delivered, opened, unsubscribed, complained }
    }

    #[test]
    fn campaign_thresholds() {
        assert_eq!(campaign_verdict(&r(299, 0, 0, 0)), None, "too few to judge");
        assert!(campaign_verdict(&r(300, 6, 0, 0)).unwrap().starts_with("unopened rate 98.0%"));
        assert_eq!(campaign_verdict(&r(300, 15, 0, 0)), None, "exactly 5% opens is fine");
        assert!(campaign_verdict(&r(300, 14, 0, 0)).is_some());
        assert_eq!(campaign_verdict(&r(1000, 300, 20, 0)), None, "2% unsubscribes is fine");
        assert!(campaign_verdict(&r(1000, 300, 21, 0)).unwrap().starts_with("unsubscribe rate"));
        assert!(campaign_verdict(&r(1000, 300, 0, 6)).unwrap().starts_with("spam complaint rate"));
        assert_eq!(campaign_verdict(&r(1000, 300, 0, 5)), None);
    }

    #[test]
    fn account_thresholds() {
        assert_eq!(account_verdict(&r(999, 0, 0, 0)), None);
        assert!(account_verdict(&r(1000, 79, 0, 0)).is_some());
        assert_eq!(account_verdict(&r(1000, 80, 0, 0)), None);
    }
}

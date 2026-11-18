//! What standing does to sending. Read from the account row the send path already loaded, so an
//! approved account (and every self-hosted one) pays no query.
//!
//! - `pending` and `review`: campaign messages are skipped (campaigns can't launch; a running one is
//!   stopped when the standing changes); flow emails and texts wait, retried at most every 15
//!   minutes, and are skipped after 3 days.
//! - `blocked`: every marketing email and text is skipped, tests refused.
//!
//! Push, transactional email and replies to people who texted in aren't affected.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use once_cell::sync::Lazy;
use uuid::Uuid;

use super::standing::{self, Standing};
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow};
use crate::sending::{Outcome, SendSpec};
use crate::state::AppState;

/// How long a flow message waits for the account to be approved before it's skipped.
pub const FLOW_WAIT_DAYS: i64 = 3;
/// The soonest a waiting flow message tries again.
pub const FLOW_RETRY_MINUTES: i64 = 15;
pub const NOT_APPROVED: &str = "account not approved";
pub const BLOCKED: &str = "account blocked";

/// The send path's check, right after the email pause check. Some(outcome): don't send.
pub async fn send_gate(state: &AppState, spec: &SendSpec<'_>, to: &str, conversational: bool) -> AppResult<Option<Outcome>> {
    if conversational || !matches!(spec.channel, "email" | "sms") {
        return Ok(None);
    }
    let st = standing::of(state, spec.account);
    if st == Standing::Approved {
        return Ok(None);
    }
    if spec.is_test {
        // Who a test may reach is decided before it's sent (see `tests_only_to_members`).
        return match st {
            Standing::Blocked => Err(AppError::Conflict(standing::BLOCKED_MESSAGE.into())),
            _ => Ok(None),
        };
    }
    let reason = if st == Standing::Blocked { BLOCKED } else { NOT_APPROVED };
    if let (Some(flow_id), Some(node), None, false) = (spec.flow_id, spec.flow_node_id, spec.campaign_id, st == Standing::Blocked) {
        if st == Standing::Pending {
            super::account::kick(state, spec.account.id);
        }
        let since: chrono::DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO screening_waits (account_id, flow_id, node_id, profile_id) VALUES ($1, $2, $3, $4)
             ON CONFLICT (flow_id, node_id, profile_id) DO UPDATE SET since = screening_waits.since RETURNING since",
        )
        .bind(spec.account.id)
        .bind(flow_id)
        .bind(node)
        .bind(spec.profile.id)
        .fetch_one(&state.db)
        .await?;
        let give_up = since + chrono::Duration::days(FLOW_WAIT_DAYS);
        let now = Utc::now();
        if now < give_up {
            let next = (now + chrono::Duration::minutes(FLOW_RETRY_MINUTES)).min(give_up + chrono::Duration::seconds(30));
            return Ok(Some(Outcome::Deferred(next)));
        }
        sqlx::query("DELETE FROM screening_waits WHERE flow_id = $1 AND node_id = $2 AND profile_id = $3").bind(flow_id).bind(node).bind(spec.profile.id).execute(&state.db).await?;
    }
    skip(state, spec, to, reason).await?;
    Ok(Some(Outcome::Skipped(reason)))
}

/// Keep a skipped message with the reason (as a paused account's are), never drop it silently.
async fn skip(state: &AppState, spec: &SendSpec<'_>, to: &str, reason: &str) -> AppResult<()> {
    let id = match spec.message_id {
        Some(mid) => {
            let n = sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status IN ('queued', 'sending')").bind(mid).bind(reason).execute(&state.db).await?.rows_affected();
            (n > 0).then_some(mid)
        }
        None => {
            let mid = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, status, error)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'skipped', $9)",
            )
            .bind(mid)
            .bind(spec.account.id)
            .bind(spec.profile.id)
            .bind(spec.channel)
            .bind(spec.campaign_id)
            .bind(spec.flow_id)
            .bind(spec.flow_node_id)
            .bind(to)
            .bind(reason)
            .execute(&state.db)
            .await?;
            Some(mid)
        }
    };
    if let Some(id) = id {
        crate::features::messages_unsent(&state.db, spec.account.id, &[id]).await;
    }
    Ok(())
}

/// A campaign batch about to send for an account that may not: stop the campaign (its queued
/// messages skipped with the reason). Returns true when it was stopped.
pub async fn stop_if_held(state: &AppState, account: &Account, c: &CampaignRow) -> anyhow::Result<bool> {
    if c.channel == "push" {
        return Ok(false);
    }
    let st = standing::of(state, account);
    if st == Standing::Approved {
        return Ok(false);
    }
    let reason = if st == Standing::Blocked { BLOCKED } else { NOT_APPROVED };
    standing::stop_campaign(&state.db, c.id, standing::hold_message(st), reason).await?;
    Ok(true)
}

/// Whether test sends of a campaign on `channel` may reach only the account's members' addresses
/// (while the account isn't approved, whatever the sender's role). Blocked accounts send no tests,
/// and test texts wait for approval.
pub fn tests_only_to_members(state: &AppState, account: &Account, channel: &str) -> AppResult<bool> {
    match standing::of(state, account) {
        Standing::Approved => Ok(false),
        Standing::Blocked => Err(AppError::Conflict(standing::BLOCKED_MESSAGE.into())),
        _ if channel == "sms" => Err(AppError::Conflict("Test texts can be sent once your account is approved to send. Test emails to your team's addresses work now.".into())),
        _ => Ok(channel == "email"),
    }
}

static KICKED: Lazy<Mutex<HashMap<Uuid, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// At most once a minute per account (per server): whether to enqueue another account check.
pub fn kick_due(account_id: Uuid) -> bool {
    let Ok(mut m) = KICKED.lock() else { return false };
    m.retain(|_, t| t.elapsed() < Duration::from_secs(600));
    match m.get(&account_id) {
        Some(t) if t.elapsed() < Duration::from_secs(60) => false,
        _ => {
            m.insert(account_id, Instant::now());
            true
        }
    }
}

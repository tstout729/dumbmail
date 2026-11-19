//! What standing does to sending. The early check reads the account row the send path already
//! loaded (an approved account, and every self-hosted one, pays no query there); the final claim
//! before the provider reads the current standing again in its own statement ([`standing_sql`],
//! [`final_stop`]).
//!
//! - `pending` and `review`: campaign messages are skipped (campaigns can't launch; a running one is
//!   stopped when the standing changes); flow emails and texts wait, retried at most every 15
//!   minutes, and are skipped after 3 days. That includes an account in review because of an
//!   engagement pause: its flow email waits rather than being skipped by the pause.
//! - `blocked`: every marketing email, text and push is skipped, tests refused.
//! - Texts the account writes itself in the inbox (replies, keyword and automatic replies) don't go
//!   out until it's approved; DumbMail's fixed compliance texts (HELP, the sign-up question and
//!   confirmation) do. Flow notifications go only to the account's members until it's approved,
//!   and not at all while it's blocked.
//! - From an approved account, a flow message's content must have passed (see `content`).
//!
//! Push isn't held for a pending or in-review account; transactional email has its own rules.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use uuid::Uuid;

use super::content::AtSend;
use super::standing::{self, Standing};
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow};
use crate::sending::{Outcome, SendSpec};
use crate::state::AppState;

/// How long a flow message waits (for approval, or for its content's check) before it's skipped.
pub const FLOW_WAIT_DAYS: i64 = 3;
/// The soonest a flow message waiting for approval tries again.
pub const FLOW_RETRY_MINUTES: i64 = 15;
pub const NOT_APPROVED: &str = "account not approved";
pub const BLOCKED: &str = "account blocked";
/// Inbox texts that are DumbMail's own fixed compliance wording, sent before approval (never while
/// blocked). A sign-up confirmation counts only with the default wording, not a keyword's own reply.
pub const COMPLIANCE_TEXTS: &[&str] = &["help", "confirmation", "optin_request"];
/// A sign-up confirmation in its default wording (`inbox::keywords::confirmation` with no keyword
/// reply): the program name, the fixed lead, then the required disclosure, nothing else.
static DEFAULT_CONFIRMATION: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"^([^\n]{1,80}): Thanks! You're signed up for texts\. ([^\n.]{0,40})\. Msg & data rates may apply\. Reply HELP for help, STOP to cancel\.$").unwrap()
});
/// The account's own slots (program name, frequency) may hold only plain words: ASCII letters,
/// spaces, a little punctuation (no dots or @) and at most 4 digits in all, so no link, domain,
/// address or number can ride along.
fn plain_slot(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || " &'-,!()/".contains(c)) && s.chars().filter(char::is_ascii_digit).count() <= 4
}

/// Whether this inbox text may go before approval: a compliance kind in its fixed wording (a
/// confirmation only in its default wording, its own slots carrying no link, domain, address or
/// number).
fn fixed_compliance(kind: &str, body: &str) -> bool {
    match kind {
        "confirmation" => DEFAULT_CONFIRMATION.captures(body).is_some_and(|c| plain_slot(&c[1]) && plain_slot(&c[2])),
        k => COMPLIANCE_TEXTS.contains(&k),
    }
}

tokio::task_local! {
    /// The flow run a send belongs to (set by `flows`), so a wait is tracked per run and step.
    static FLOW_RUN: Uuid;
}

/// Run `f` (a flow step's send) as part of flow run `run`.
pub async fn in_flow_run<F: std::future::Future>(run: Uuid, f: F) -> F::Output {
    FLOW_RUN.scope(run, f).await
}

fn run_key(spec: &SendSpec<'_>) -> String {
    FLOW_RUN.try_with(|r| r.to_string()).unwrap_or_else(|_| format!("p:{}", spec.profile.id))
}

/// The send path's early check, before the email pause check. Some(outcome): don't send.
pub async fn send_gate(state: &AppState, spec: &SendSpec<'_>, to: &str, conversational: Option<&'static str>) -> AppResult<Option<Outcome>> {
    if !super::on(state) || !matches!(spec.channel, "email" | "sms") {
        return Ok(None);
    }
    let st = standing::of(state, spec.account);
    if let Some(kind) = conversational {
        return conversational_gate(st, kind, spec.content["body"].as_str().unwrap_or(""));
    }
    if st == Standing::Approved {
        // A flow message's content, as it goes out, must have passed (it can change outside any
        // check: a saved block, a translation, an edit while the flow was a draft).
        if let (Some(flow_id), None, false) = (spec.flow_id, spec.campaign_id, spec.is_test) {
            return match super::content::flow_at_send(state, spec.account, flow_id, spec.channel, spec.content).await? {
                AtSend::Go => Ok(None),
                AtSend::Wait(until, why) => wait(state, spec, to, until, why).await,
                AtSend::Stop(why, _) => {
                    skip(state, spec, to, why).await?;
                    Ok(Some(Outcome::Skipped(why)))
                }
            };
        }
        return Ok(None);
    }
    if spec.is_test {
        // Who a test may reach is decided before it's sent (see `tests_only_to_members`) and again
        // at the final check (`test_email_stop`).
        return match st {
            Standing::Blocked => Err(AppError::Conflict(standing::BLOCKED_MESSAGE.into())),
            _ => Ok(None),
        };
    }
    let reason = if st == Standing::Blocked { BLOCKED } else { NOT_APPROVED };
    if spec.flow_id.is_some() && spec.campaign_id.is_none() && st != Standing::Blocked {
        if st == Standing::Pending {
            super::account::kick(state, spec.account.id);
        }
        return wait(state, spec, to, Utc::now() + chrono::Duration::minutes(FLOW_RETRY_MINUTES), reason).await;
    }
    skip(state, spec, to, reason).await?;
    Ok(Some(Outcome::Skipped(reason)))
}

/// Texts the account writes itself wait for approval; DumbMail's fixed compliance texts don't
/// (while blocked, nothing goes).
fn conversational_gate(st: Standing, kind: &str, body: &str) -> AppResult<Option<Outcome>> {
    if st == Standing::Approved || (st != Standing::Blocked && fixed_compliance(kind, body)) {
        return Ok(None);
    }
    let reason = if st == Standing::Blocked { BLOCKED } else { NOT_APPROVED };
    if kind == "reply" {
        // Typed by a person in the inbox: the reply box says why.
        return Err(AppError::Conflict(format!("Not sent. {}", standing::hold_message(st))));
    }
    Ok(Some(Outcome::Skipped(reason)))
}

/// A flow message waits (this run and step, from when it first waited) until `until`, and after
/// [`FLOW_WAIT_DAYS`] it's skipped with `why`.
async fn wait(state: &AppState, spec: &SendSpec<'_>, to: &str, until: DateTime<Utc>, why: &'static str) -> AppResult<Option<Outcome>> {
    let (Some(flow_id), Some(node)) = (spec.flow_id, spec.flow_node_id) else {
        skip(state, spec, to, why).await?;
        return Ok(Some(Outcome::Skipped(why)));
    };
    let key = run_key(spec);
    let since: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO screening_waits (account_id, flow_id, node_id, run_key) VALUES ($1, $2, $3, $4)
         ON CONFLICT (flow_id, node_id, run_key) DO UPDATE SET since = screening_waits.since RETURNING since",
    )
    .bind(spec.account.id)
    .bind(flow_id)
    .bind(node)
    .bind(&key)
    .fetch_one(&state.db)
    .await?;
    let give_up = since + chrono::Duration::days(FLOW_WAIT_DAYS);
    if Utc::now() < give_up {
        return Ok(Some(Outcome::Deferred(until.min(give_up + chrono::Duration::seconds(30)))));
    }
    sqlx::query("DELETE FROM screening_waits WHERE flow_id = $1 AND node_id = $2 AND run_key = $3").bind(flow_id).bind(node).bind(&key).execute(&state.db).await?;
    skip(state, spec, to, why).await?;
    Ok(Some(Outcome::Skipped(why)))
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

// ---------- The final claim ----------

/// A column for the final claim's own statement (with `accounts` as `alias`): the account's
/// current standing, or 'approved' where screening is off.
pub fn standing_sql(state: &AppState, alias: &str) -> String {
    if super::on(state) { format!("{alias}.sending_review") } else { "'approved'::text".into() }
}

/// What the final claim does with the standing it just read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalStop {
    Skip(&'static str),
    /// A flow message waits for approval (the next attempt goes through the early check again).
    Defer(DateTime<Utc>),
}

/// The final claim's decision on `standing` (read in the claiming statement). A message past the
/// claim has been handed over and can't be recalled.
pub fn final_stop(standing: &str, spec: &SendSpec<'_>, conversational: Option<&str>) -> Option<FinalStop> {
    let st = Standing::parse(Some(standing));
    if st == Standing::Approved {
        return None;
    }
    let reason = if st == Standing::Blocked { BLOCKED } else { NOT_APPROVED };
    if let Some(kind) = conversational {
        let fixed = st != Standing::Blocked && fixed_compliance(kind, spec.content["body"].as_str().unwrap_or(""));
        return (!fixed).then_some(FinalStop::Skip(reason));
    }
    if spec.channel == "push" {
        return (st == Standing::Blocked).then_some(FinalStop::Skip(BLOCKED));
    }
    if spec.is_test {
        // Test emails to members are checked by `test_email_stop`; test texts wait for approval.
        return (st == Standing::Blocked || spec.channel == "sms").then_some(FinalStop::Skip(reason));
    }
    if spec.flow_id.is_some() && spec.campaign_id.is_none() && st != Standing::Blocked {
        return Some(FinalStop::Defer(Utc::now() + chrono::Duration::minutes(FLOW_RETRY_MINUTES)));
    }
    Some(FinalStop::Skip(reason))
}

/// A test email's final check (tests have no other last check): while the account isn't approved
/// it may only reach a current member, and from a blocked account nothing goes. Some(reason): don't.
pub async fn test_email_stop(state: &AppState, spec: &SendSpec<'_>, to: &str) -> AppResult<Option<&'static str>> {
    if !super::on(state) {
        return Ok(None);
    }
    let (st, member): (String, bool) = sqlx::query_as(
        "SELECT a.sending_review, EXISTS (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = a.id AND lower(u.email) = lower($2))
         FROM accounts a WHERE a.id = $1",
    )
    .bind(spec.account.id)
    .bind(to)
    .fetch_one(&state.db)
    .await?;
    Ok(match Standing::parse(Some(&st)) {
        Standing::Approved => None,
        Standing::Blocked => Some(BLOCKED),
        _ if !member => Some(NOT_APPROVED),
        _ => None,
    })
}

// ---------- Campaigns ----------

/// A campaign batch about to send: stop the campaign if the account may not send (its queued
/// messages skipped with the reason), or if its content as it is now was held or refused; if that
/// content hasn't been checked yet, the check runs on the background lane and this batch waits
/// for it. Returns true when the batch was handled here (stopped, or put off).
pub async fn stop_if_held(state: &AppState, account: &Account, c: &CampaignRow, ids: &[Uuid]) -> anyhow::Result<bool> {
    if !super::on(state) || c.channel == "push" {
        return Ok(false);
    }
    let st = standing::of(state, account);
    if st == Standing::Approved {
        return match super::content::campaign_at_send(state, account, c).await? {
            AtSend::Go => Ok(false),
            AtSend::Wait(until, _) => {
                crate::jobs::enqueue(&state.db, "campaign_batch", serde_json::json!({"campaign_id": c.id, "message_ids": ids}), Some(until)).await?;
                Ok(true)
            }
            AtSend::Stop(reason, msg) => {
                standing::stop_campaign(&state.db, c.id, &msg, reason).await?;
                Ok(true)
            }
        };
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

// ---------- Flow notifications ----------

/// A column for the notification's final check (binds: `$1` account, `$2` the address): why it
/// mustn't go because of the account's standing, or NULL. Until approved only members get them;
/// while blocked nobody does.
pub fn notification_stop_sql(state: &AppState) -> &'static str {
    if !super::on(state) {
        return "NULL::text";
    }
    "(SELECT CASE WHEN a.sending_review = 'blocked' THEN 'account blocked'
        WHEN a.sending_review <> 'approved' AND NOT EXISTS (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = a.id AND lower(u.email) = lower($2))
        THEN 'account not approved' END FROM accounts a WHERE a.id = $1)"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_default_confirmation_is_fixed_wording() {
        let d = "Acme VIP: Thanks! You're signed up for texts. Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel.";
        assert!(fixed_compliance("confirmation", d));
        let own = "Acme VIP: Thanks! You're signed up for texts. Buy crypto now at evil.test. Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel.";
        assert!(!fixed_compliance("confirmation", own));
        assert!(!fixed_compliance("confirmation", "Call 503 555 0199 now: Thanks! You're signed up for texts. Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel."));
        assert!(!fixed_compliance("confirmation", "Visit evil．test: Thanks! You're signed up for texts. Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel."));
        assert!(!fixed_compliance("confirmation", "Visit evil.test: Thanks! You're signed up for texts. Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel."));
        assert!(!fixed_compliance("confirmation", "Acme VIP: Here's 20% off! Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel."));
        assert!(fixed_compliance("help", "anything"));
        assert!(!fixed_compliance("keyword_reply", d));
        // Nothing at all while blocked.
        assert!(conversational_gate(Standing::Blocked, "help", "x").unwrap().is_some());
        assert!(conversational_gate(Standing::Pending, "help", "x").unwrap().is_none());
    }
}

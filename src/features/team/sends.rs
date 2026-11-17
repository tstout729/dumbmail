//! Test sends, with the limits that depend on who's sending.
//!
//! Owners, admins and managers (and API keys with the messaging scope) send tests to any address
//! or number, as before: they can send the real campaign anyway. Members who can't send campaigns
//! (the Content role) may send test **emails** only, only to the email addresses of the account's
//! current members, and only to addresses that already have a profile: their tests never create
//! one.

use serde_json::json;
use uuid::Uuid;

use super::access::Perm;
use super::audit::audit;
use crate::auth::Ctx;
use crate::campaigns;
use crate::error::{AppError, AppResult};
use crate::models::normalize_email;
use crate::state::AppState;

/// Send a test of campaign `campaign_id` to `to` (1 to 10 addresses). Returns how many went out.
/// Each test that goes out is recorded in the audit log, even when a later one fails.
pub async fn test_send(state: &AppState, ctx: &Ctx, campaign_id: Uuid, to: &[String], version: Option<&str>) -> AppResult<usize> {
    ctx.require(Perm::TestSend)?;
    if to.is_empty() || to.len() > 10 {
        return Err(AppError::bad("send tests to 1-10 addresses"));
    }
    let c = campaigns::load(&state.db, ctx.account_id, campaign_id).await?;
    let account = campaigns::load_account(&state.db, ctx.account_id).await?;
    let restricted = !ctx.can(Perm::CampaignSend);
    let mut to: Vec<String> = to.to_vec();
    if restricted {
        if c.channel != "email" {
            return Err(AppError::Forbidden("your role can send test emails, not test texts; ask a manager or admin".into()));
        }
        to = to.iter().map(|a| normalize_email(a).ok_or_else(|| AppError::bad(format!("{a:?} isn't an email address")))).collect::<AppResult<_>>()?;
        let members: Vec<String> = sqlx::query_scalar("SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND u.email = ANY($2)")
            .bind(ctx.account_id)
            .bind(&to)
            .fetch_all(&state.db)
            .await?;
        if let Some(outsider) = to.iter().find(|a| !members.contains(a)) {
            return Err(AppError::Forbidden(format!("your role can send tests only to team members' email addresses, and {outsider} isn't one")));
        }
    }
    let mut sent = 0;
    for addr in &to {
        match campaigns::send_test(state, &account, &c, std::slice::from_ref(addr), version, !restricted).await {
            Ok(n) => sent += n,
            Err(e) => {
                if sent > 0 {
                    record(state, ctx, &c, sent, false).await;
                }
                return Err(e);
            }
        }
    }
    record(state, ctx, &c, sent, true).await;
    Ok(sent)
}

async fn record(state: &AppState, ctx: &Ctx, c: &crate::models::CampaignRow, sent: usize, all: bool) {
    audit(state, ctx, "campaign.test_send", Some(&c.id.to_string()), json!({"campaign": c.name, "channel": c.channel, "tests_sent": sent, "all_sent": all})).await;
}

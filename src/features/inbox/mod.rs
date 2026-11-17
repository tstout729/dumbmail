//! Two-way texting: an SMS inbox for conversations, replies from the dashboard, API and MCP,
//! keywords ("text JOIN to subscribe"), automatic replies, email digests of new texts, and the
//! "Received SMS Reply" event flows use (including the "wait for a reply" step).
//!
//! The provider webhooks in `public.rs` hand every inbound text to [`receive`] (stores it, dedupes
//! retries, creates a profile for new numbers) and, after STOP/START, to [`received`] (keywords,
//! double opt-in, automatic replies). Replies go out through `sending::send_conversational`.

mod api;
pub mod conversations;
pub mod keywords;
mod notify;
pub mod replies;
pub mod settings;
pub mod store;
mod tools;

use async_trait::async_trait;
use axum::Router;
use serde_json::Value;

pub use store::{receive, record_sent, replied_since, Inbound, Received};

use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::AppResult;
use crate::state::AppState;

pub struct Module;

/// After the webhook's STOP/START handling: the reply event, then a pending double opt-in, a
/// keyword, or the default/away reply, and finally any flow waiting for this person's reply.
/// STOP, START and HELP never get keyword or automatic replies.
pub async fn received(state: &AppState, rx: &Received) -> AppResult<()> {
    match rx.kind {
        "stop" => return Ok(()),
        "start" => {
            // START or YES answers a double opt-in request too (consent was set by the webhook).
            if rx.prior_consent == "pending" && !rx.new_profile {
                keywords::confirm_pending(state, rx, true).await?;
            }
            return Ok(());
        }
        "help" => return store::record_reply_event(&state.db, rx, None, 0).await,
        _ => {}
    }
    let media: i64 = sqlx::query_scalar("SELECT jsonb_array_length(media)::bigint FROM sms_inbound WHERE id = $1").bind(rx.id).fetch_one(&state.db).await?;
    if rx.prior_consent == "pending" && rx.word == "y" && keywords::confirm_pending(state, rx, false).await? {
        store::record_reply_event(&state.db, rx, Some("Y"), media as usize).await?;
        return Ok(());
    }
    let keyword = keywords::matching(&state.db, rx.account_id, &rx.word).await?;
    store::record_reply_event(&state.db, rx, keyword.as_ref().map(|k| k.keyword.to_uppercase()).as_deref(), media as usize).await?;
    match &keyword {
        Some(k) => keywords::run(state, rx, k).await?,
        None => replies::default_reply(state, rx).await?,
    }
    store::wake_waiting_flows(&state.db, rx.profile_id).await
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "inbox"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools::definitions()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        tools::call(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "sms-inbox",
            name: "SMS inbox, replies and keywords",
            description: "Two-way texting: conversations, reply rules, keywords and sign-up disclosures, automatic replies, and the reply flow trigger and step",
            text: include_str!("../../../docs/SMS_INBOX.md"),
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            "inbox_media" => Some(notify::copy_media(state, payload).await),
            "inbox_auto_reply" => Some(replies::run_auto_reply(state, payload).await),
            "inbox_digest" => Some(notify::send_digest(state, payload).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            notify::queue_digests(state).await?;
        }
        Ok(())
    }
}

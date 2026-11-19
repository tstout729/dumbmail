//! AI shopping agent: answers shoppers' questions from the store's own information, in the
//! customer hub (signed-in shoppers), in a chat bubble on the store's website (anyone, never
//! anything personal) and, when the store turns it on, as the first answer to inbound texts and
//! WhatsApp messages. It hands off to a person when it should. See docs/AGENT.md.
//!
//! - `settings`: what the store turns on and tells it; the settings API and MCP tools.
//! - `knowledge`: the store's answers, its own website pages (read weekly) and the catalog.
//! - `claude`: one streamed model turn over raw HTTPS.
//! - `chat`: conversations, caps, the tools the server runs, hand-offs.
//! - `guard`: what crosses to Claude and back (input redaction, links, prices and codes).
//! - `proof`: what a conversation's order access rests on, and whether it still holds.
//! - `fee`: the conversation fee, held at admission and charged when an answer goes out.
//! - `delivery`: the last check before a text or WhatsApp answer goes out.
//! - `web`: the public chat pages on the hub's host and the bubble script.
//! - `sms`, `whatsapp`: first answers to inbound texts and WhatsApp messages.
//! - `api`: the team's view of conversations and the test chat.

pub mod api;
pub mod chat;
pub mod claude;
pub mod delivery;
pub mod fee;
pub mod guard;
pub mod knowledge;
pub mod proof;
pub mod settings;
pub mod sms;
pub mod web;
pub mod whatsapp;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{obj, tool};
use crate::state::AppState;

pub use chat::{CHATTED_METRIC, HANDED_OFF_METRIC};

pub struct Module;

/// When the account's standing is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// Starting a conversation or a turn (every place).
    Admission,
    /// Handing an automatic text or WhatsApp answer to the provider.
    Delivery,
}

/// Whether the account's standing lets the agent answer at `place` now. The one place sender
/// screening (PR #40) plugs in: blocked accounts get no agent anywhere; pending or in-review
/// accounts keep the hub and bubble chats but get no automatic texts or WhatsApp answers. Sender
/// screening isn't on `main` yet, so every account is allowed for now.
pub async fn standing_allows(_state: &AppState, _account_id: Uuid, _place: &str, _moment: Moment) -> bool {
    true
}

/// Background job: delete conversations 90 days after their last message (their messages first),
/// and old visitor counters, in bounded batches; queues itself again while more is left.
pub const CLEANUP_JOB: &str = "agent_cleanup";
const CLEANUP_BATCH: i64 = 500;

pub async fn cleanup(state: &AppState) -> anyhow::Result<bool> {
    let db = &state.db;
    let old = "SELECT id FROM agent_conversations WHERE last_message_at < now() - interval '90 days' ORDER BY last_message_at LIMIT $1";
    let msgs = sqlx::query(&format!("DELETE FROM agent_messages WHERE id IN (SELECT m.id FROM agent_messages m WHERE m.conversation_id IN ({old}) LIMIT $1 * 20)"))
        .bind(CLEANUP_BATCH)
        .execute(db)
        .await?
        .rows_affected();
    let convs = if msgs < (CLEANUP_BATCH * 20) as u64 {
        sqlx::query(&format!("DELETE FROM agent_conversations WHERE id IN ({old})")).bind(CLEANUP_BATCH).execute(db).await?.rows_affected()
    } else {
        0
    };
    let visitors = sqlx::query("DELETE FROM agent_visitor_usage WHERE (ip_hash, hour) IN (SELECT ip_hash, hour FROM agent_visitor_usage WHERE hour < now() - interval '2 days' LIMIT $1)")
        .bind(CLEANUP_BATCH * 4)
        .execute(db)
        .await?
        .rows_affected();
    fee::release_stale(db).await?;
    let more = msgs >= (CLEANUP_BATCH * 20) as u64 || convs >= CLEANUP_BATCH as u64 || visitors >= (CLEANUP_BATCH * 4) as u64;
    if more {
        crate::jobs::enqueue_background(db, CLEANUP_JOB, json!({}), None).await?;
    }
    Ok(more)
}

/// A redaction that erased a person's orders but kept the person (Shopify's customer redact): the
/// agent's copies of those orders go too, with the transcripts that held them.
pub async fn forget_orders(db: &sqlx::PgPool, account_id: Uuid, profile_ids: &[Uuid]) -> sqlx::Result<()> {
    if profile_ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM agent_conversations WHERE account_id = $1 AND profile_id = ANY($2) AND verified").bind(account_id).bind(profile_ids).execute(db).await?;
    Ok(())
}

const DOC: &str = include_str!("../../../docs/AGENT.md");

static ROUTE_RULES: &[RouteRule] = &[
    rule("GET", "/api/agent/settings", Perm::AccountRead),
    rule("PATCH", "/api/agent/settings", Perm::SettingsWrite),
    // Transcripts are people's words (and, for verified shoppers, their orders): the inbox's read.
    rule("GET", "/api/agent/conversations", Perm::ProfilesRead),
    rule("GET", "/api/agent/conversations/{id}", Perm::ProfilesRead),
    // A made-up shopper, but it spends model tokens: settings write.
    rule("POST", "/api/agent/test", Perm::SettingsWrite),
    // Shoppers, not team members. Each handler checks the shopper (hub session) itself.
    public("GET", "/hub/{key}/chat"),
    public("POST", "/hub/{key}/chat/send"),
    public("GET", "/hub/{key}/ask"),
    public("POST", "/hub/{key}/ask/send"),
    public("GET", "/client/agent.js"),
];

static TOOL_RULES: &[ToolRule] = &[
    ToolRule { name: "get_agent_settings", perm: Perm::AccountRead },
    ToolRule { name: "update_agent_settings", perm: Perm::SettingsWrite },
    ToolRule { name: "list_agent_conversations", perm: Perm::ProfilesRead },
    ToolRule { name: "get_agent_conversation", perm: Perm::ProfilesRead },
    ToolRule { name: "test_agent_chat", perm: Perm::SettingsWrite },
];

fn tools() -> Vec<Value> {
    vec![
        tool(
            "get_agent_settings",
            "Get shopping agent settings",
            "The AI shopping agent's settings: where it answers (customer hub, website bubble, texts, WhatsApp), its name and tone note, your questions and answers, the website pages it reads and how they were read, the monthly conversation cap and this month's use, and whether this server can run it.",
            obj(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "update_agent_settings",
            "Update shopping agent settings",
            "Change the shopping agent's settings. Send only what changes; `answers` and `pages` replace their lists. Pages must be on your own website. Turning on `sms` or `whatsapp` lets it send automatic first answers to people who write to you (billed per message like any reply). See dumbmail://docs/agent.",
            obj(settings::patch_schema(), &[]),
            false,
            false,
        ),
        tool(
            "list_agent_conversations",
            "List shopping agent conversations",
            "Recent shopping agent conversations, newest first, with hand-offs marked (reason and the agent's summary for the team), who it was (verified shoppers only) and the last message.",
            obj(
                json!({
                    "handed_off": {"type": "boolean", "description": "true: only hand-offs; false: only the rest"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100},
                    "cursor": {"type": "string", "description": "next_cursor from the previous page"},
                }),
                &[],
            ),
            true,
            false,
        ),
        tool(
            "get_agent_conversation",
            "Get a shopping agent conversation",
            "One shopping agent conversation and what was said in it, newest messages first in pages of 50 (`older_cursor` for the ones before).",
            obj(json!({"id": {"type": "string", "format": "uuid"}, "before": {"type": "string", "description": "older_cursor from the previous page"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}}), &["id"]),
            true,
            false,
        ),
        tool(
            "test_agent_chat",
            "Test the shopping agent",
            "Ask the shopping agent a question as a made-up shopper with a made-up order, to check its answers. Pass `conversation` from the previous answer to continue. It uses the model (and counts toward the monthly cap), so it runs only with confirm: true; without it you get a dry run. Nothing is sent to anyone.",
            obj(json!({"message": {"type": "string", "maxLength": chat::MAX_MESSAGE_CHARS}, "conversation": {"type": "string"}, "confirm": {"type": "boolean", "description": "true to run it; otherwise a dry run"}}), &["message"]),
            false,
            false,
        ),
    ]
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    match name {
        "get_agent_settings" => settings::settings_json(state, ctx.account_id).await,
        "update_agent_settings" => settings::update(state, ctx, a.clone()).await,
        "list_agent_conversations" => api::list(state, ctx.account_id, &crate::mcp::from(a.clone())?).await,
        "get_agent_conversation" => {
            let id = a["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("id must be a conversation id"))?;
            let q = api::MessagesQuery { before: a["before"].as_str().map(String::from), limit: a["limit"].as_i64() };
            api::one(state, ctx.account_id, id, &q).await
        }
        "test_agent_chat" => {
            if a["confirm"].as_bool() != Some(true) {
                let s = settings::settings_json(state, ctx.account_id).await?;
                return Ok(json!({
                    "dry_run": true,
                    "would": "ask the shopping agent this question as a made-up shopper with a made-up order, using the model once or a few times",
                    "available": s["available"], "conversations_this_month": s["conversations_this_month"], "monthly_cap": s["monthly_cap"],
                    "next": "call again with confirm: true to run it",
                }));
            }
            let mut b = a.clone();
            if let Some(o) = b.as_object_mut() {
                o.remove("confirm");
            }
            api::test(state, ctx, crate::mcp::from(b)?).await
        }
        _ => Err(AppError::not_found("tool")),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn routes(&self) -> Router<AppState> {
        settings::routes().merge(api::routes()).merge(web::routes())
    }

    fn access(&self) -> Access {
        Access { routes: ROUTE_RULES, tools: TOOL_RULES }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !tools().iter().any(|t| t["name"] == name) {
            return None;
        }
        Some(call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "agent",
            name: "Shopping agent",
            description: "An AI assistant that answers shoppers in the customer hub, a website chat bubble and (optionally) texts, from the store's own answers, pages, catalog and the shopper's own orders, and hands off to a person.",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            knowledge::FETCH_JOB => Some(knowledge::run_fetch(state, payload).await),
            sms::JOB => Some(sms::run(state, payload).await),
            whatsapp::JOB => Some(whatsapp::run(state, payload).await),
            CLEANUP_JOB => Some(cleanup(state).await.map(|_| ())),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        // About hourly: pages due for their weekly read, and the cleanup job (on the background
        // lane, in batches) for conversations past 90 days.
        if tick.is_multiple_of(180) {
            knowledge::queue_due(&state.db).await?;
            crate::jobs::enqueue_background(&state.db, CLEANUP_JOB, json!({}), None).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_object_schemas_and_docs_name_them() {
        for t in tools() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            let name = t["name"].as_str().unwrap();
            assert!(DOC.contains(name), "docs/AGENT.md should mention {name}");
        }
    }

    #[test]
    fn agent_metrics_are_server_only() {
        assert!(crate::audience::is_server_only_metric(CHATTED_METRIC));
        assert!(crate::audience::is_server_only_metric(HANDED_OFF_METRIC));
    }
}

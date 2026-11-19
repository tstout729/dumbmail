//! Sender screening: only clean senders reach DumbMail's shared sending reputation.
//!
//! Every hosted customer sends email through one Amazon SES account and texts through one Telnyx
//! account, so a few senders of cold email, bought lists or phishing hurt everyone. On the hosted
//! service each account has a standing ([`Standing`]): new accounts start `pending` and are
//! checked before their first real send; accounts that existed before this shipped are `approved`.
//! What's checked, the thresholds and what goes to Anthropic are in docs/SCREENING.md.
//!
//! - [`account`]: the first check (rules on the owner, website and domains, then one Claude call).
//! - [`domains`], [`imports`], [`content`]: sending and link domains, CSV imports, and what a
//!   campaign or flow sends, checked for every account.
//! - [`engagement`]: how people react to campaigns, hourly.
//! - [`gate`]: what standing does to sending (one check where sending checks the pause).
//! - [`api`] and [`admin`]: the customer's and the operator's side.
//!
//! A self-hosted install (`cfg.hosted` false) screens nothing.

pub mod account;
pub mod admin;
pub mod ai;
pub mod api;
pub mod content;
pub mod domains;
pub mod engagement;
pub mod fetch;
pub mod gate;
pub mod imports;
pub mod redact;
pub mod rules;
pub mod standing;

use async_trait::async_trait;
use axum::Router;
use serde::Serialize;
use serde_json::Value;

use super::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::AppResult;
use crate::state::AppState;

pub use content::{before_flow_edit, before_flow_live, before_launch, before_saved_block_edit, content_changed};
pub use domains::check_domain;
pub use gate::{blocks_transactional, final_stop, inbox_reply_allowed, whatsapp_stop, in_flow_run, notification_stop_sql, send_gate, standing_sql, stop_if_held, test_email_stop, tests_only_to_members, FinalStop, BLOCKED, NOT_APPROVED};
pub use imports::check_import;
pub use standing::Standing;

/// Whether this server screens senders (the hosted service).
pub fn on(state: &AppState) -> bool {
    state.cfg.hosted && state.cfg.screen_senders
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Review,
    Block,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Review => "review",
            Verdict::Block => "block",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pass" => Some(Verdict::Pass),
            "review" => Some(Verdict::Review),
            "block" => Some(Verdict::Block),
            _ => None,
        }
    }
}

pub struct Module;

static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/screening", Perm::AccountRead),
    rule("POST", "/api/screening/request", Perm::SettingsWrite),
    // Operator only (Bearer ADMIN_TOKEN, like /admin/unpause): no sign-in.
    public("GET", "/admin/screening"),
    public("POST", "/admin/screening/decide"),
    public("POST", "/admin/screening/pass"),
];
static TOOLS: &[ToolRule] = &[ToolRule { name: "get_sending_review", perm: Perm::AccountRead }, ToolRule { name: "request_sending_review", perm: Perm::SettingsWrite }];

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "screening"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes().merge(admin::routes())
    }

    fn mcp_tools(&self) -> Vec<Value> {
        api::mcp_tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        api::mcp_call(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![
            McpDoc {
                slug: "screening",
                name: "Sender screening",
                description: "How DumbMail checks new senders, imports, domains and content before they reach the shared sending reputation, and what an account's sending review means.",
                text: include_str!("../../../docs/SCREENING.md"),
            },
            McpDoc {
                slug: "acceptable-use",
                name: "Acceptable use",
                description: "What DumbMail senders may and may not send.",
                text: include_str!("../../../docs/ACCEPTABLE_USE.md"),
            },
        ]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            account::JOB => Some(account::run_job(state, payload).await),
            content::JOB => Some(content::run_job(state, payload).await),
            engagement::JOB => Some(engagement::run_job(state).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if !on(state) {
            return Ok(());
        }
        // About every 3 minutes: accounts waiting on a sending domain that now have one.
        if tick.is_multiple_of(9) {
            account::sweep_waiting(state).await?;
        }
        // About hourly: how people reacted to recent campaigns.
        if tick.is_multiple_of(180) {
            crate::jobs::enqueue_background(&state.db, engagement::JOB, serde_json::json!({}), None).await?;
        }
        Ok(())
    }

    /// Content already live when screening started is recorded as passed (see
    /// `content::record_existing`).
    async fn after_start(&self, state: &AppState) -> anyhow::Result<()> {
        content::record_existing(state).await?;
        Ok(())
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }
}

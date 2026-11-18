//! Customer hub: an account area for the store's shoppers, on the store's branded link domain (or
//! DumbMail's shared link host). Shoppers sign in with a code emailed to them, or arrive signed in
//! from a signed link in an email the store sent, and see their orders and parcels, bookings and
//! subscriptions (custom objects the store chooses), picks for them, their back-in-stock requests,
//! their email and text preferences, and the store's help. See docs/HUB.md.
//!
//! - `settings`: what the store turns on, checked; the settings API and MCP tools.
//! - `session`: sign-in codes, signed links and sessions. [`session::current`] is the one way to
//!   learn who is signed in, for the hub's pages and anything later built into the hub.
//! - `data`: what a signed-in shopper sees, always read for the session's own person and account.
//! - `page`: the hub's HTML.
//! - `web`: the public routes (the hub pages, its form posts, the launcher script).

pub mod data;
pub mod page;
pub mod session;
pub mod settings;
pub mod web;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};

use super::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{obj, tool};
use crate::state::AppState;

pub use session::{current, hub_url_for, Shopper};

/// Recorded when a signed-in shopper opens the hub (once per session a day).
pub const OPENED_METRIC: &str = "Opened Hub";
/// Recorded when a signed-in shopper opens one of their orders (once per session, order and day).
pub const VIEWED_ORDER_METRIC: &str = "Viewed Order in Hub";

pub struct Module;

const DOC: &str = include_str!("../../../docs/HUB.md");

static ROUTE_RULES: &[RouteRule] = &[
    rule("GET", "/api/hub/settings", Perm::AccountRead),
    rule("PATCH", "/api/hub/settings", Perm::SettingsWrite),
    // A made-up sample person, so no one's data: settings read is enough.
    rule("GET", "/api/hub/preview", Perm::AccountRead),
    // The hub itself: shoppers, not team members. Each handler checks the shopper's own session.
    public("GET", "/hub/{key}"),
    public("GET", "/hub/{key}/orders/{order}"),
    public("POST", "/hub/{key}/code"),
    public("POST", "/hub/{key}/verify"),
    public("POST", "/hub/{key}/prefs"),
    public("POST", "/hub/{key}/stock/{id}/cancel"),
    public("POST", "/hub/{key}/signout"),
    public("GET", "/client/hub.js"),
];

static TOOL_RULES: &[ToolRule] = &[ToolRule { name: "get_hub_settings", perm: Perm::AccountRead }, ToolRule { name: "update_hub_settings", perm: Perm::SettingsWrite }];

fn tools() -> Vec<Value> {
    vec![
        tool(
            "get_hub_settings",
            "Get customer hub settings",
            "The customer hub's settings: whether it's on, the launcher, which sections show, which custom object types and fields show, help entries, the hub's address, and the object types and fields that could be shown (sensitive fields and, for bookings, access or address fields never can).",
            obj(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "update_hub_settings",
            "Update customer hub settings",
            "Change the customer hub's settings. Send only what changes. Sensitive object fields, and for booking types any access or address field, are refused. See dumbmail://docs/hub for every field.",
            obj(settings::patch_schema(), &[]),
            false,
            false,
        ),
    ]
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    match name {
        "get_hub_settings" => settings::settings_json(state, ctx.account_id).await,
        "update_hub_settings" => settings::update(state, ctx, a.clone()).await,
        _ => Err(AppError::not_found("tool")),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "hub"
    }

    fn routes(&self) -> Router<AppState> {
        settings::routes().merge(web::routes())
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
            slug: "hub",
            name: "Customer hub",
            description: "A shopper account area on the store's site: orders and tracking, bookings and subscriptions, picks, preferences and help, with sign-in by emailed code.",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            session::SEND_CODE_JOB => Some(session::run_send_code(state, payload).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(180) {
            session::clean_up(&state.db).await?;
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
            assert!(DOC.contains(name), "docs/HUB.md should mention {name}");
        }
    }

    #[test]
    fn hub_metrics_are_server_only() {
        assert!(crate::audience::is_server_only_metric(OPENED_METRIC));
        assert!(crate::audience::is_server_only_metric(VIEWED_ORDER_METRIC));
    }
}

//! Feature modules.
//!
//! A feature owns its routes, MCP tools and docs, background jobs and scheduler work in its own
//! file (or folder) and plugs them in through the [`Feature`] trait, so building one doesn't touch
//! the shared wiring in `lib.rs`, `mcp.rs` or `jobs.rs`. To add a feature, add its module and one
//! line to [`ALL`].

pub mod agent;
pub mod audiences;
pub mod catalog;
pub mod content;
pub mod coupons;
pub mod hub;
pub mod inbox;
pub mod integrations;
pub mod klaviyo_import;
pub mod objects;
pub mod push;
pub mod reports;
pub mod reviews;
pub mod sendtime;
pub mod shopify;
pub mod team;
pub mod transactional;
pub mod warehouse;
pub mod webhooks;
pub mod whatsapp;

use async_trait::async_trait;
use axum::Router;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::AppResult;
use crate::state::AppState;

/// A reference doc served to MCP clients as `dumbmail://docs/<slug>`.
pub struct McpDoc {
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub text: &'static str,
}

#[async_trait]
pub trait Feature: Send + Sync {
    fn name(&self) -> &'static str;

    /// HTTP routes, merged into the app. Dashboard and REST routes live under `/api/`.
    fn routes(&self) -> Router<AppState> {
        Router::new()
    }

    /// MCP tool definitions (build them with `crate::mcp::tool`).
    fn mcp_tools(&self) -> Vec<Value> {
        Vec::new()
    }

    /// Run one of this feature's MCP tools. Return `None` when `name` isn't one of them.
    async fn mcp_call(&self, _state: &AppState, _ctx: &Ctx, _name: &str, _args: &Value) -> Option<AppResult<Value>> {
        None
    }

    /// Reference docs for MCP clients.
    fn mcp_docs(&self) -> Vec<McpDoc> {
        Vec::new()
    }

    /// Run a background job of one of this feature's kinds. Return `None` when `kind` isn't one of
    /// them. Name job kinds `<feature>_<what>` so two features never share one.
    async fn run_job(&self, _state: &AppState, _kind: &str, _payload: &Value) -> Option<anyhow::Result<()>> {
        None
    }

    /// Called by the scheduler about every 20 seconds; `tick` counts up from 0. Use
    /// `tick.is_multiple_of(n)` for slower work. Keep it short and enqueue jobs for anything heavy.
    async fn tick(&self, _state: &AppState, _tick: u64) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether this feature's `on_event` needs to see the account's events of `metric`. Events
    /// reach `on_event` only when a live flow or some feature wants them, so keep this cheap (an
    /// `EXISTS`, cached briefly).
    async fn wants_event(&self, _db: &sqlx::PgPool, _account_id: Uuid, _metric: &str) -> bool {
        false
    }

    /// Called once for each recorded event that a live flow or [`Feature::wants_event`] asked
    /// for, after flows have seen it.
    async fn on_event(&self, _state: &AppState, _event_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called once when the server starts, on a background task that startup never waits for. For
    /// slow upkeep that's safe to retry on the next start, like building an index concurrently.
    async fn after_start(&self, _state: &AppState) -> anyhow::Result<()> {
        Ok(())
    }
}

pub static ALL: &[&dyn Feature] = &[
    &agent::Module,
    &audiences::Module,
    &catalog::Module,
    &content::Module,
    &coupons::Module,
    &hub::Module,
    &inbox::Module,
    &integrations::Module,
    &klaviyo_import::Module,
    &objects::Module,
    &push::Module,
    &reports::Module,
    &reviews::Module,
    &sendtime::Module,
    &shopify::Module,
    &team::Module,
    &transactional::Module,
    &warehouse::Module,
    &webhooks::Module,
    &whatsapp::Module,
];

pub fn routes() -> Router<AppState> {
    ALL.iter().fold(Router::new(), |r, f| r.merge(f.routes()))
}

pub fn mcp_tools() -> Vec<Value> {
    ALL.iter().flat_map(|f| f.mcp_tools()).collect()
}

pub async fn mcp_call(state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
    for f in ALL {
        if let Some(r) = f.mcp_call(state, ctx, name, args).await {
            return Some(r);
        }
    }
    None
}

pub fn mcp_docs() -> Vec<McpDoc> {
    ALL.iter().flat_map(|f| f.mcp_docs()).collect()
}

pub async fn run_job(state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
    for f in ALL {
        if let Some(r) = f.run_job(state, kind, payload).await {
            return Some(r);
        }
    }
    None
}

/// One feature failing must not stop the scheduler or the other features.
pub async fn tick(state: &AppState, tick: u64) {
    for f in ALL {
        if let Err(e) = f.tick(state, tick).await {
            tracing::error!(feature = f.name(), error = %format!("{e:#}"), "feature scheduler work failed");
        }
    }
}

/// Whether any feature wants to see this event (see [`Feature::wants_event`]).
pub async fn wants_event(db: &sqlx::PgPool, account_id: Uuid, metric: &str) -> bool {
    for f in ALL {
        if f.wants_event(db, account_id, metric).await {
            return true;
        }
    }
    false
}

/// Messages that ended without being sent: `skipped` (with the reason in `error`: smart sending,
/// time zones, a cancelled campaign ...) or `failed`. Call it right after the write, with the ids
/// it changed. Lets webhooks and data exports report the outcome. Never fails or blocks the caller.
pub async fn messages_unsent(db: &sqlx::PgPool, account_id: Uuid, ids: &[Uuid]) {
    if ids.is_empty() {
        return;
    }
    webhooks::messages_unsent(db, account_id, ids).await;
    let rows: Vec<(&str, Uuid)> = ids.iter().map(|id| ("messages", *id)).collect();
    warehouse::changed_many(db, account_id, &rows).await;
}

/// [`messages_unsent`] for (account id, message id) pairs from more than one account.
pub async fn messages_unsent_rows(db: &sqlx::PgPool, rows: &[(Uuid, Uuid)]) {
    let mut by: std::collections::HashMap<Uuid, Vec<Uuid>> = std::collections::HashMap::new();
    for (account, id) in rows {
        by.entry(*account).or_default().push(*id);
    }
    for (account, ids) in by {
        messages_unsent(db, account, &ids).await;
    }
}

/// [`messages_unsent`] inside the caller's transaction (it goes out only if that commits; a failure
/// here can't spoil it).
pub async fn messages_unsent_in(conn: &mut sqlx::PgConnection, account_id: Uuid, ids: &[Uuid]) {
    if ids.is_empty() {
        return;
    }
    webhooks::messages_unsent_in(conn, account_id, ids).await;
    warehouse::changed_in(conn, account_id, "messages", ids).await;
}

/// Run every feature's [`Feature::after_start`] on its own task and return at once. Errors are logged.
pub fn spawn_after_start(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for f in ALL {
            if let Err(e) = f.after_start(&state).await {
                tracing::error!(feature = f.name(), error = %format!("{e:#}"), "feature startup work failed");
            }
        }
    })
}

/// Errors are logged, not returned: the event's flows already ran and must not run again on a retry.
pub async fn on_event(state: &AppState, event_id: Uuid) {
    for f in ALL {
        if let Err(e) = f.on_event(state, event_id).await {
            tracing::error!(feature = f.name(), event = %event_id, error = %format!("{e:#}"), "feature event hook failed");
        }
    }
}

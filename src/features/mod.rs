//! Feature modules.
//!
//! A feature owns its routes, MCP tools and docs, background jobs and scheduler work in its own
//! file (or folder) and plugs them in through the [`Feature`] trait, so building one doesn't touch
//! the shared wiring in `lib.rs`, `mcp.rs` or `jobs.rs`. To add a feature, add its module and one
//! line to [`ALL`].

pub mod agent;
pub mod audiences;
pub mod campaigns_plus;
pub mod catalog;
pub mod content;
pub mod coupons;
pub mod flows_plus;
pub mod hub;
pub mod inbox;
pub mod integrations;
pub mod klaviyo_import;
pub mod objects;
pub mod observe;
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

    /// The permission each of this feature's routes and MCP tools needs (see `team::access`).
    /// Routes and tools left out are refused, and a test fails for any route without a rule.
    fn access(&self) -> team::access::Access {
        team::access::Access::default()
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
    &campaigns_plus::Module,
    &catalog::Module,
    &content::Module,
    &coupons::Module,
    &flows_plus::Module,
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

// ---- Observers (webhooks and data exports) ----
//
// The rest of the app reports changes here. Each check below reads a per-server snapshot of who
// has anything set up (`observe`), with no SQL, so an account with no webhook endpoint or export
// pays nothing; observer writes never fail, block or slow the caller (see `observe`).

/// Whether anything wants to hear about this account's messages that ended without being sent.
/// When it says no, don't collect the ids (a plain `UPDATE`, as if nothing were watching).
pub fn observes_unsent(db: &sqlx::PgPool, account_id: Uuid) -> bool {
    webhooks::wants_unsent(db, account_id) || warehouse::exports(db, account_id, "messages")
}

/// Whether any account on this server's snapshot has anything set up (for sweeps that span
/// accounts). When it says no, sweeps don't collect rows just to report them.
pub fn observes_any(db: &sqlx::PgPool) -> bool {
    observe::peek(db).is_none_or(|i| i.any())
}

/// Messages that ended without being sent: `skipped` (with the reason in `error`: smart sending,
/// time zones, a cancelled campaign ...) or `failed`. Call it right after the write, with the ids
/// it changed. Lets webhooks and data exports report the outcome. Never fails or blocks the caller.
pub async fn messages_unsent(db: &sqlx::PgPool, account_id: Uuid, ids: &[Uuid]) {
    if ids.is_empty() || !observes_unsent(db, account_id) {
        return;
    }
    let Some(mut tx) = observe::begin(db, "messages unsent").await else { return };
    let res = unsent_writes(&mut tx, db, account_id, ids).await;
    observe::commit(tx, res, "messages unsent").await;
}

async fn unsent_writes(conn: &mut sqlx::PgConnection, db: &sqlx::PgPool, account_id: Uuid, ids: &[Uuid]) -> sqlx::Result<()> {
    if webhooks::wants_unsent(db, account_id) {
        webhooks::write_unsent(conn, db, account_id, ids).await?;
    }
    let rows: Vec<(&str, Uuid)> = ids.iter().map(|id| ("messages", *id)).collect();
    let rows = warehouse::change_rows(db, account_id, &rows);
    if !rows.is_empty() {
        warehouse::write_changes(conn, account_id, &rows).await?;
    }
    Ok(())
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

/// [`messages_unsent`] inside the caller's transaction (it goes out only if that commits; it can't
/// spoil or stall it).
pub async fn messages_unsent_in(conn: &mut sqlx::PgConnection, db: &sqlx::PgPool, account_id: Uuid, ids: &[Uuid]) {
    if ids.is_empty() || !observes_unsent(db, account_id) {
        return;
    }
    let Some(saved) = observe::enter(conn, "messages unsent").await else { return };
    let res = unsent_writes(conn, db, account_id, ids).await;
    observe::leave(conn, saved, res, "messages unsent").await;
}

/// An event was recorded (inside `audience::record_event`'s transaction): webhooks' `event.tracked`
/// and message topics, and the export change log. Nothing is built or written unless the account
/// has something that wants it.
#[allow(clippy::too_many_arguments)]
pub async fn event_recorded_in(
    conn: &mut sqlx::PgConnection,
    db: &sqlx::PgPool,
    account_id: Uuid,
    event_id: Uuid,
    profile_id: Uuid,
    metric: &str,
    source: crate::audience::EventSource,
    message_id: Option<Uuid>,
    properties: &Value,
) {
    let hooks = webhooks::event_rows(db, account_id, event_id, metric, source, message_id, properties);
    let changes = warehouse::event_change_rows(db, account_id, profile_id, message_id);
    if hooks.is_empty() && changes.is_empty() {
        return;
    }
    let Some(saved) = observe::enter(conn, "event recorded").await else { return };
    let res = async {
        if !hooks.is_empty() {
            webhooks::write_out(conn, account_id, &hooks).await?;
        }
        if !changes.is_empty() {
            warehouse::write_changes(conn, account_id, &changes).await?;
        }
        Ok(())
    }
    .await;
    observe::leave(conn, saved, res, "event recorded").await;
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

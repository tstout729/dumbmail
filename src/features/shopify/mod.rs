//! Shopify app: connect a store, server-side order, refund, fulfilment and checkout events from
//! webhooks, customers and their marketing consent, a history import, product sync into the
//! catalog, per-person discount codes, and the storefront snippet without pasting code.
//! See docs/SHOPIFY.md.
//!
//! - `shop`: the stored connection, domain checks, and the account's one background worker.
//! - `client`: calls to the store's GraphQL Admin API, tokens, bulk result files.
//! - `oauth`: installing (authorization-code grant) and what's set up in the store after.
//! - `webhooks`: verified deliveries, stored and processed by the worker.
//! - `orders`, `customers`, `products`: what each kind of record becomes here.
//! - `events`: recording server events (one event per order whichever source saw it first).
//! - `import`: history and product reconciles through bulk operations.
//! - `discounts`: single-use codes made in the store.
//! - `privacy`: Shopify's mandatory data request and redaction webhooks.

pub mod api;
pub mod client;
pub mod coupons;
pub mod customers;
pub mod discounts;
pub mod events;
pub mod fence;
pub mod import;
pub mod oauth;
pub mod orders;
pub mod privacy;
pub mod products;
pub mod shop;
pub mod webhooks;

use async_trait::async_trait;
use axum::Router;
use serde_json::Value;
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::AppResult;
use crate::state::AppState;

pub use discounts::{create_discount_code, create_discount_codes, DiscountTemplate, DiscountValue};

/// The Admin API version every call uses. 2026-10 is supported until October 16, 2027; move to a
/// newer stable version before then (see docs/SHOPIFY.md).
pub const API_VERSION: &str = "2026-10";

const DOC: &str = include_str!("../../../docs/SHOPIFY.md");

/// Operator configuration: the Shopify app's credentials from the Partner (Dev) Dashboard.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Settings {
    /// The app's client ID. Unset (with the secret), Settings → Shopify says it isn't available.
    #[arg(long, env = "SHOPIFY_API_KEY")]
    pub shopify_api_key: Option<String>,
    /// The app's client secret: signs OAuth callbacks and webhooks.
    #[arg(long, env = "SHOPIFY_API_SECRET", hide_env_values = true)]
    pub shopify_api_secret: Option<String>,
    /// Base URL that replaces `https://` before every store's host (tests point it at a local stand-in).
    #[arg(long, env = "SHOPIFY_API_BASE", hide = true)]
    pub shopify_api_base: Option<String>,
}

impl Settings {
    pub fn key(&self) -> Option<&str> {
        self.shopify_api_key.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    pub fn secret(&self) -> Option<&str> {
        self.shopify_api_secret.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    pub fn available(&self) -> bool {
        self.key().is_some() && self.secret().is_some()
    }
}

/// The metrics this app records from Shopify's servers. When a store connects they become
/// server-only for that account (`metrics.server_only`): the public browser endpoint refuses them,
/// and flows ignore browser copies recorded earlier. Other accounts' browser metrics are untouched.
pub const SERVER_METRICS: &[&str] = &["Placed Order", "Started Checkout", "Ordered Product", "Fulfilled Order", "Fulfilled Partial Order", "Cancelled Order", "Refunded Order"];

pub struct Module;

/// Job: the account's Shopify worker. Each run gives each kind of work its own share of the slice,
/// most urgent first, so one busy queue can't starve the others: privacy requests, then
/// unsubscribes to write back, then webhook deliveries (and the items they ask to read again), then
/// the import. Then it hands back to the queue.
async fn work(state: &AppState, account_id: Uuid, generation: Option<i32>) -> anyhow::Result<()> {
    // A job without an install (queued before jobs carried one) or for an earlier install does nothing.
    let Some(generation) = generation else { return Ok(()) };
    let Some(lease) = shop::acquire(state, account_id, generation).await? else { return Ok(()) };
    let (more, delay) = match work_inner(state, account_id, &lease).await {
        Ok(x) => x,
        Err(e) => {
            // Queued again below; failing the job too would schedule it twice.
            tracing::warn!(account = %account_id, error = %format!("{e:#}"), "Shopify work failed; trying again shortly");
            (true, Some(chrono::Duration::seconds(30)))
        }
    };
    shop::release(state, &lease, more, delay).await
}

/// Seconds of each slice for each kind of work (what one leaves unused, the later ones get).
const PRIVACY_SECS: u64 = 3;
const WRITEBACK_SECS: u64 = 3;
const WEBHOOK_SECS: u64 = 6;

async fn work_inner(state: &AppState, account_id: Uuid, lease: &shop::Lease) -> anyhow::Result<(bool, Option<chrono::Duration>)> {
    let Some(s) = shop::load(&state.db, account_id).await? else { return Ok((false, None)) };
    if s.status != "connected" || s.generation != lease.generation {
        return Ok((false, None));
    }
    // A closed account: only what's owed to the people in it (privacy requests, and unsubscribes
    // written back to the store). No webhooks, reads or imports.
    let closed = crate::models::account_closed(&state.db, account_id).await?;
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(shop::SLICE_SECS);
    let until = |secs: u64| (std::time::Instant::now() + std::time::Duration::from_secs(secs)).min(deadline);
    let mut metrics = events::Metrics::default();
    let mut wait: Option<chrono::Duration> = None;
    let mut more = false;

    // 1. Privacy requests.
    privacy::run_until(state, account_id, until(PRIVACY_SECS)).await?;

    // 2. Unsubscribes to write back.
    if shop::renew(state, lease).await? {
        match customers::write_back(state, &s, 20, until(WRITEBACK_SECS)).await {
            Ok(n) if n >= 20 => more = true,
            Ok(_) => {}
            Err(e) => {
                if let Some(secs) = e.retry_after() {
                    wait = Some(chrono::Duration::seconds(secs as i64));
                } else {
                    tracing::warn!(account = %account_id, error = %e, "couldn't write an unsubscribe back to Shopify");
                }
            }
        }
    } else {
        return Ok((false, None));
    }
    if closed {
        return Ok(match wait {
            Some(d) => (true, Some(d)),
            None => (more, None),
        });
    }

    // 3. Webhook deliveries, then the items they asked to read again.
    let hooks_until = until(WEBHOOK_SECS);
    let mut cx = orders::Ctx { state, account_id, shop: &s.shop_domain, generation: lease.generation, metrics: &mut metrics, quiet: false };
    more |= webhooks::drain(state, &mut cx, lease, hooks_until).await?;
    while wait.is_none() && std::time::Instant::now() < hooks_until.max(until(2)) {
        if !shop::renew(state, lease).await? {
            return Ok((false, None));
        }
        let batch: Vec<(String, i64)> = sqlx::query_as(
            "SELECT kind, shopify_id FROM shopify_refresh WHERE account_id = $1 ORDER BY kind DESC, queued_at LIMIT 50",
        )
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
        if batch.is_empty() {
            break;
        }
        let kind = batch[0].0.clone();
        let per = match kind.as_str() {
            "product" => products::PER_QUERY,
            orders::REFUNDS_LIVE | orders::REFUNDS_HISTORY | orders::FULL_HISTORY => orders::REFUNDS_PER_QUERY,
            _ => 50,
        };
        let ids: Vec<i64> = batch.iter().filter(|(k, _)| *k == kind).map(|(_, i)| *i).take(per).collect();
        // What the history import queued is history: recorded quietly (no flows start).
        cx.quiet = kind == orders::REFUNDS_HISTORY || kind == orders::FULL_HISTORY;
        let r = match kind.as_str() {
            "product" => products::refresh(state, &s, &ids).await,
            orders::REFUNDS_LIVE | orders::REFUNDS_HISTORY => orders::refresh_refunds(&mut cx, &s, &ids).await,
            orders::FULL_HISTORY => orders::refresh_full(&mut cx, &s, &ids).await,
            _ => customers::refresh(state, &s, &ids).await,
        };
        cx.quiet = false;
        match r {
            Ok(()) => {
                sqlx::query("DELETE FROM shopify_refresh WHERE account_id = $1 AND kind = $2 AND shopify_id = ANY($3)").bind(account_id).bind(&kind).bind(&ids).execute(&state.db).await?;
            }
            Err(e) => match e.retry_after() {
                Some(secs) => {
                    wait = Some(chrono::Duration::seconds(secs as i64));
                    more = true;
                }
                None => {
                    // Not worth retrying (permission, gone): drop them and say so.
                    sqlx::query("DELETE FROM shopify_refresh WHERE account_id = $1 AND kind = $2 AND shopify_id = ANY($3)").bind(account_id).bind(&kind).bind(&ids).execute(&state.db).await?;
                    shop::note_error(&state.db, account_id, Some(&format!("Reading {kind}s from Shopify failed: {e}."))).await?;
                }
            },
        }
    }
    let refresh_left: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_refresh WHERE account_id = $1)").bind(account_id).fetch_one(&state.db).await?;
    more |= refresh_left && wait.is_none();

    // 4. The import, with what's left of the slice.
    if wait.is_none() && std::time::Instant::now() < deadline && shop::renew(state, lease).await? {
        match import::step(state, &s, lease, &mut metrics, deadline).await? {
            import::Step::More => more = true,
            import::Step::Wait(d) => wait = Some(d),
            import::Step::Idle => {}
        }
    }
    Ok(match wait {
        Some(d) => (true, Some(d)),
        None => (more, None),
    })
}

/// Job: delete old delivery records, unprocessed bodies past 30 days, and old exports.
async fn cleanup(state: &AppState) -> anyhow::Result<()> {
    webhooks::prune(&state.db).await?;
    privacy::prune(&state.db).await?;
    Ok(())
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "shopify"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn access(&self) -> crate::features::team::access::Access {
        crate::features::team::access::Access { routes: api::ROUTES, tools: api::TOOLS }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        api::tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !api::tools().iter().any(|t| t["name"] == name) {
            return None;
        }
        Some(api::call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "shopify",
            name: "Shopify app",
            description: "Connecting a Shopify store: events from Shopify's servers, customers and consent, history import, product sync, discount codes, privacy webhooks and setting up the app.",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let account = || payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing account_id"));
        Some(match kind {
            "shopify_work" => match account() {
                Ok(a) => work(state, a, payload["generation"].as_i64().map(|g| g as i32)).await,
                Err(e) => Err(e),
            },
            "shopify_setup" => match account() {
                Ok(a) => oauth::setup(state, a, payload["generation"].as_i64().map(|g| g as i32)).await,
                Err(e) => Err(e),
            },
            "shopify_privacy" => match account() {
                Ok(a) => privacy::run(state, a).await,
                Err(e) => Err(e),
            },
            events::JOB_REBASELINE => match account() {
                Ok(a) => events::rebaseline(state, a).await,
                Err(e) => Err(e),
            },
            "shopify_cleanup" => cleanup(state).await,
            _ => return None,
        })
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            shop::requeue_stuck(&state.db).await?;
            for account_id in customers::writeback_due(&state.db).await? {
                shop::kick_now(&state.db, account_id).await?;
            }
            privacy::queue_due(&state.db).await?;
            oauth::setup_due(&state.db).await?;
            sqlx::query("DELETE FROM shopify_oauth_states WHERE created_at < now() - interval '1 hour'").execute(&state.db).await?;
        }
        if tick.is_multiple_of(30) {
            import::queue_reconciles(&state.db).await?;
        }
        if tick.is_multiple_of(180) {
            // Clean-up runs in the background lane, like the rest of this app's work.
            crate::jobs::enqueue_background(&state.db, "shopify_cleanup", serde_json::json!({}), None).await?;
        }
        Ok(())
    }
}

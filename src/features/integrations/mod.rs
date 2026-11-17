//! Custom integrations: incoming webhooks and scheduled pulls from any app, translated into
//! profiles, events, list changes and consent by a small sandboxed JavaScript function that Claude
//! (or a person) writes. Recipes ship ready-made ones (Guesty, Stripe, Calendly).
//!
//! - `api`: REST routes and the public `POST /webhooks/in/{token}` URL
//! - `signature`: checking webhooks really came from the app
//! - `sandbox`: QuickJS with memory, stack, time and size limits
//! - `actions`: validating and writing what scripts return (relay addresses are suppressed here)
//! - `pull`: request templates, SSRF-safe fetching and the OAuth token cache
//! - `engine`: deliveries, retries, pulls, backfills, test runs and replays
//! - `recipes`: the ready-made integrations in `recipes/`

pub mod actions;
pub mod api;
pub mod engine;
pub mod model;
pub mod pull;
pub mod recipes;
pub mod sandbox;
pub mod signature;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

const DOC: &str = include_str!("../../../docs/INTEGRATIONS.md");

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

/// Integration fields from tool arguments (everything except the id and confirm).
fn changes(a: &Value) -> AppResult<model::Changes> {
    let mut a = a.clone();
    if let Some(o) = a.as_object_mut() {
        o.remove("id");
        o.remove("confirm");
    }
    from(a)
}

fn confirmed(a: &Value) -> bool {
    a["confirm"].as_bool() == Some(true)
}

fn schema_fields() -> Value {
    json!({
        "name": {"type": "string", "maxLength": 100},
        "script": {"type": "string", "description": "JavaScript defining function handle(input) that returns a list of actions. See dumbmail://docs/integrations."},
        "signature": {"type": "object", "description": "How to check webhooks: {type:'none'} | {type:'hmac', header, algorithm?:'sha256'|'sha1', format?:'plain'|'stripe'|'svix', encoding?:'hex'|'base64', prefix?, timestamp_header?, tolerance_seconds?, secret_encoding?} | {type:'shared_secret', header} | {type:'shared_secret', query}."},
        "signature_secret": {"type": "string", "description": "The signing secret (write-only; never returned). Empty string removes it."},
        "settings": {"type": "object", "description": "Non-secret settings the script reads as input.settings (e.g. a list name)."},
        "pull": {"type": ["object", "null"], "description": "Scheduled fetch: {method?, url, headers?, query?, body?, body_format?, auth?, interval_minutes? (>=5), max_pages?}. Placeholders {{secret.NAME}}, {{state.KEY}}, {{settings.KEY}}, {{now}}, {{since}}, with | fallbacks. null removes it."},
        "credentials": {"type": "object", "additionalProperties": {"type": ["string", "null"]}, "description": "Secrets by name for pull placeholders (write-only). null removes one; others are kept."},
    })
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "integrations"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "integrations",
            name: "Custom integrations",
            description: "Build an integration for any app: inbound webhooks, signature checks, the translation script and its actions, scheduled pulls with OAuth, recipes (Guesty, Stripe, Calendly), limits and a worked example",
            text: DOC,
        }]
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let id = json!({"type": "string", "description": "Integration id"});
        let mut create_props = schema_fields();
        let mut update_props = schema_fields();
        update_props["id"] = id.clone();
        update_props["state"] = json!({"type": "object", "description": "Replace the script's saved state (e.g. reset a cursor)."});
        create_props.as_object_mut().map(|o| o.remove("state"));
        vec![
            tool("list_integrations", "List integrations", "Every custom integration with its status, counts and last activity.", obj(json!({}), &[]), true, false),
            tool("get_integration", "Get an integration", "One integration: inbound webhook URL, signature settings (secret shown only as a hint), script, settings, pull settings, which credentials are saved or missing, saved state, backfill progress and last error.", obj(json!({"id": id}), &["id"]), true, false),
            tool("create_integration", "Create an integration", "Create a custom integration for any app. Returns its secret inbound URL (POST webhooks there). Read dumbmail://docs/integrations first; test the script with test_integration before pointing the app at the URL.", obj(create_props, &["name"]), false, false),
            tool("update_integration", "Update an integration", "Change the name, script, signature settings or secret, settings, pull settings, credentials (write-only; null removes one) or saved state. Changing credentials or the pull's auth drops a cached access token.", obj(update_props, &["id"]), false, false),
            tool("test_integration", "Test an integration's script", "Run the script on a sample payload (or, with pull: true, on the first page fetched from the app now) and return exactly what would be written: the actions, a line per action and counts. Writes nothing. Pass `script` to try changes before saving. A pull test reuses the cached access token and asks for one only if none is cached.",
                obj(json!({"id": id, "payload": {"description": "Sample body: JSON value, or a string for raw/form bodies"}, "headers": {"type": "object"}, "query": {"type": "object"}, "source": {"enum": ["webhook", "pull", "test"]}, "pull": {"type": "boolean"}, "script": {"type": "string"}}), &["id"]), true, false),
            tool("list_integration_deliveries", "List an integration's deliveries", "The delivery log, newest first: source (webhook, pull, test, replay), status (pending, ok, error, ignored), error, counts of what was written, attempts and the next retry. Kept 30 days.",
                obj(json!({"id": id, "status": {"enum": ["pending", "ok", "error", "ignored"]}, "source": {"enum": ["webhook", "pull", "replay"]}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &["id"]), true, false),
            tool("get_integration_delivery", "Get one delivery", "A delivery's payload, headers, query, the pull request made (secrets hidden), the result (counts, a line per action, logs) and any error.", obj(json!({"id": id, "delivery_id": {"type": "string"}}), &["id", "delivery_id"]), true, false),
            tool("replay_integration_deliveries", "Replay deliveries", "Run stored deliveries again with the current script: chosen ones (delivery_ids) or every delivery that failed for good (all_failed). Writes profiles and events and can start flows, so it needs confirm=true; without it you get a dry run with the count.",
                obj(json!({"id": id, "delivery_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 500}, "all_failed": {"type": "boolean"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
            tool("run_integration_pull", "Run a pull or backfill", "Fetch from the app now, or (with backfill_since, YYYY-MM-DD) import history from that date in pages until done; progress shows in get_integration.backfill. Writes profiles and events and can start flows, so it needs confirm=true; without it you get a dry run.",
                obj(json!({"id": id, "backfill_since": {"type": "string", "description": "YYYY-MM-DD"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
            tool("cancel_integration_backfill", "Cancel a backfill", "Stop a running backfill after the page it is on.", obj(json!({"id": id}), &["id"]), false, false),
            tool("set_integration_status", "Turn an integration on or off", "disabled keeps incoming webhooks (marked ignored, replayable later) but writes nothing and stops scheduled pulls; enabled resumes.", obj(json!({"id": id, "status": {"enum": ["enabled", "disabled"]}}), &["id", "status"]), false, false),
            tool("rotate_integration_url", "Rotate the inbound URL", "Give the integration a new secret webhook URL. The old URL stops working at once, so the app must be updated. Needs confirm=true.", obj(json!({"id": id, "confirm": {"type": "boolean"}}), &["id"]), false, true),
            tool("delete_integration", "Delete an integration", "Delete the integration and its delivery log. Profiles and events it wrote stay. Needs confirm=true.", obj(json!({"id": id, "confirm": {"type": "boolean"}}), &["id"]), false, true),
            tool("list_integration_recipes", "List integration recipes", "Ready-made integrations (Guesty, Stripe, Calendly, …) with the credentials they need, setup steps and suggested flows. Pass key for one recipe in full (script, signature, pull).", obj(json!({"key": {"type": "string"}}), &[]), true, false),
            tool("create_integration_from_recipe", "Create an integration from a recipe", "Set up a ready-made integration. Returns the inbound URL, the setup steps and the credentials to add with update_integration. Suggested flows are created as drafts (create_flows: false to skip); nothing sends until a flow is turned on.",
                obj(json!({"recipe": {"type": "string"}, "name": {"type": "string"}, "create_flows": {"type": "boolean"}}), &["recipe"]), false, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        if !self.mcp_tools().iter().any(|t| t["name"] == name) {
            return None;
        }
        let acct = ctx.account_id;
        let r = async {
            match name {
                "list_integrations" => api::list(state, acct).await,
                "get_integration" => Ok(model::load(state, acct, uuid_arg(a, "id")?).await?.view(state)),
                "create_integration" => api::create(state, acct, changes(a)?).await,
                "update_integration" => Ok(model::update(state, acct, uuid_arg(a, "id")?, changes(a)?).await?.view(state)),
                "test_integration" => {
                    let mut b = a.clone();
                    b.as_object_mut().map(|o| o.remove("id"));
                    api::test(state, acct, uuid_arg(a, "id")?, from(b)?).await
                }
                "list_integration_deliveries" => {
                    let q = api::DeliveryQuery { status: a["status"].as_str().map(String::from), source: a["source"].as_str().map(String::from), limit: a["limit"].as_i64(), offset: a["offset"].as_i64() };
                    api::deliveries(state, acct, uuid_arg(a, "id")?, &q).await
                }
                "get_integration_delivery" => api::delivery(state, acct, uuid_arg(a, "id")?, uuid_arg(a, "delivery_id")?).await,
                "replay_integration_deliveries" => {
                    let b = api::ReplayBody { delivery_ids: from(a.get("delivery_ids").cloned().unwrap_or(json!([])))?, all_failed: a["all_failed"].as_bool().unwrap_or(false) };
                    let mut v = api::replay(state, acct, uuid_arg(a, "id")?, &b, !confirmed(a)).await?;
                    if !confirmed(a) {
                        v["note"] = json!("Nothing ran. Replaying writes profiles and events and can start flows. Call again with confirm=true once the user approves.");
                    }
                    Ok(v)
                }
                "run_integration_pull" => {
                    let i = model::load(state, acct, uuid_arg(a, "id")?).await?;
                    let since = a["backfill_since"].as_str().map(api::parse_since).transpose()?;
                    if !confirmed(a) {
                        i.pull_config().ok_or_else(|| AppError::bad("this integration has no pull settings; add a pull first"))?;
                        let missing = i.missing_credentials();
                        return Ok(json!({"dry_run": true, "would_start": if since.is_some() { "backfill" } else { "pull" }, "backfill_since": since, "missing_credentials": missing,
                            "note": "Nothing ran. A pull writes profiles and events and can start flows. Use test_integration with pull: true to preview a page, then call again with confirm=true once the user approves."}));
                    }
                    engine::start_pull(state, &i, since).await
                }
                "cancel_integration_backfill" => {
                    let i = model::load(state, acct, uuid_arg(a, "id")?).await?;
                    engine::cancel_backfill(state, &i).await
                }
                "set_integration_status" => Ok(model::set_status(state, acct, uuid_arg(a, "id")?, a["status"].as_str().unwrap_or("")).await?.view(state)),
                "rotate_integration_url" => {
                    let id = uuid_arg(a, "id")?;
                    if !confirmed(a) {
                        let i = model::load(state, acct, id).await?;
                        return Ok(json!({"dry_run": true, "current_url": i.inbound_url(state), "note": "Nothing changed. The current URL stops working the moment it rotates, so the app must be given the new one. Call again with confirm=true once the user approves."}));
                    }
                    Ok(model::rotate_url(state, acct, id).await?.view(state))
                }
                "delete_integration" => {
                    let id = uuid_arg(a, "id")?;
                    let i = model::load(state, acct, id).await?;
                    if !confirmed(a) {
                        return Ok(json!({"dry_run": true, "integration": i.summary(), "note": "Nothing deleted. This removes the integration and its delivery log (profiles and events it wrote stay). Call again with confirm=true once the user approves."}));
                    }
                    model::delete(state, acct, id).await?;
                    Ok(json!({"deleted": true}))
                }
                "list_integration_recipes" => match a["key"].as_str() {
                    Some(k) => recipes::get(k).map(|r| r.view(true)).ok_or_else(|| AppError::not_found("recipe")),
                    None => Ok(json!({"recipes": recipes::all().iter().map(|r| r.view(false)).collect::<Vec<_>>()})),
                },
                "create_integration_from_recipe" => {
                    let key = a["recipe"].as_str().ok_or_else(|| AppError::bad("give the recipe key"))?;
                    api::from_recipe(state, acct, key, a["name"].as_str().map(String::from), a["create_flows"].as_bool().unwrap_or(true)).await
                }
                other => Err(AppError::bad(format!("unknown tool {other}"))),
            }
        }
        .await;
        Some(r)
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            engine::JOB_DELIVERY => {
                let id = payload["delivery_id"].as_str().and_then(|s| s.parse::<Uuid>().ok());
                Some(match id {
                    Some(id) => engine::process_delivery(state, id).await,
                    None => Ok(()),
                })
            }
            engine::JOB_PULL => Some(engine::run_pull(state, payload).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            engine::schedule_pulls(state).await?;
        }
        if tick.is_multiple_of(180) {
            engine::prune(state).await?;
        }
        Ok(())
    }
}

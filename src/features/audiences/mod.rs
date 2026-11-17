//! Ad audience sync: keep lists and segments in step with Meta, Google Ads and TikTok customer-list
//! audiences, as people join and leave. See docs/AD_AUDIENCES.md.
//!
//! - `platforms`: each platform's OAuth and audience API.
//! - `normalize`: identifiers normalized and SHA-256 hashed the way each platform documents.
//! - `sync`: runs (scan, diff against the last snapshot, remove, add) as short chained jobs.
//! - `store`: connections (encrypted tokens) and syncs.
//! - `api`: REST routes, also used by the MCP tools here.

pub mod api;
pub mod normalize;
pub mod platforms;
pub mod store;
pub mod sync;

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

const DOC: &str = include_str!("../../../docs/AD_AUDIENCES.md");

fn platform_arg(a: &Value) -> AppResult<platforms::Platform> {
    a["platform"].as_str().and_then(platforms::Platform::parse).ok_or_else(|| AppError::bad("platform must be meta, google or tiktok"))
}

fn id_arg(a: &Value) -> AppResult<Uuid> {
    a["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("id must be a sync id"))
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "audiences"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let platform = json!({"enum": ["meta", "google", "tiktok"]});
        vec![
            tool(
                "list_ad_connections",
                "List ad platform connections",
                "Meta, Google Ads and TikTok: whether each is available on this server, connected, or needs reconnecting. Connecting needs a browser: send the user to connect_url (Settings → Ad audiences).",
                obj(json!({}), &[]),
                true,
                false,
            ),
            tool(
                "list_ad_accounts",
                "List ad accounts",
                "The ad accounts the connected sign-in can reach on one platform, to pick from when creating a sync. For Google without listing (manual_entry), ask the user for their Google Ads customer ID.",
                obj(json!({"platform": platform}), &["platform"]),
                true,
                false,
            ),
            tool(
                "list_audience_syncs",
                "List ad audience syncs",
                "Lists and segments synced to ad platform audiences, with status: last run, people sent, added and removed last run, the platform's audience size and match rate, and any error in plain words.",
                obj(json!({"source_type": {"enum": ["list", "segment"]}, "source_id": {"type": "string"}}), &[]),
                true,
                false,
            ),
            tool(
                "get_audience_sync",
                "Get an ad audience sync",
                "One sync's status and its recent run history.",
                obj(json!({"id": {"type": "string"}}), &["id"]),
                true,
                false,
            ),
            tool(
                "create_audience_sync",
                "Sync a list or segment to an ad audience",
                "Create a customer-list audience on Meta, Google Ads or TikTok from a list or segment and keep it in step (people who join are added, people who leave are removed). Only hashed contact details are sent; people who unsubscribed or opted out of ads are left out. This sends customer data to the ad platform: without confirm=true it returns a dry run with the number of people. Get the user's approval, and pass accept_terms=true only when they confirm these people gave them their details directly and they accept the platform's customer list terms.",
                obj(
                    json!({
                        "platform": platform,
                        "ad_account_id": {"type": "string", "description": "From list_ad_accounts (Meta: without act_; Google: customer ID)"},
                        "login_account_id": {"type": "string", "description": "Google only: the manager account ID when the ad account is reached through one"},
                        "source_type": {"enum": ["list", "segment"]},
                        "source_id": {"type": "string"},
                        "name": {"type": "string", "maxLength": 100, "description": "Audience name on the platform. Default: the list or segment name + (DumbMail)"},
                        "include_unsubscribed": {"type": "boolean", "description": "For exclusion audiences only: also send people who unsubscribed from email, so ads can leave them out. Default false."},
                        "match_fields": {"enum": ["contact", "contact_and_location"], "description": "contact: email and phone. contact_and_location adds name, city, state, ZIP and country for better matches."},
                        "interval_hours": {"type": "integer", "minimum": 1, "maximum": 168, "description": "How often to sync. Default 6."},
                        "accept_terms": {"type": "boolean"},
                        "confirm": {"type": "boolean"}
                    }),
                    &["platform", "ad_account_id", "source_type", "source_id"],
                ),
                false,
                true,
            ),
            tool(
                "update_audience_sync",
                "Change an ad audience sync",
                "Pause or resume a sync (status), or change how often it runs, whether unsubscribed people are included (exclusion audiences only) and which fields are matched.",
                obj(
                    json!({
                        "id": {"type": "string"},
                        "status": {"enum": ["active", "paused"]},
                        "interval_hours": {"type": "integer", "minimum": 1, "maximum": 168},
                        "include_unsubscribed": {"type": "boolean"},
                        "match_fields": {"enum": ["contact", "contact_and_location"]}
                    }),
                    &["id"],
                ),
                false,
                false,
            ),
            tool("run_audience_sync", "Sync an ad audience now", "Start a sync now instead of waiting for the schedule. Check get_audience_sync for the result.", obj(json!({"id": {"type": "string"}}), &["id"]), false, false),
            tool(
                "delete_audience_sync",
                "Delete an ad audience sync",
                "Stop syncing. With delete_on_platform=true the audience is also deleted on the ad platform, and ads that use it stop. Without confirm=true it only describes what would happen.",
                obj(json!({"id": {"type": "string"}, "delete_on_platform": {"type": "boolean"}, "confirm": {"type": "boolean"}}), &["id"]),
                false,
                true,
            ),
            tool(
                "disconnect_ad_platform",
                "Disconnect an ad platform",
                "Revoke DumbMail's access to Meta, Google Ads or TikTok and delete its tokens. Syncs to that platform wait until it's connected again. Without confirm=true it only describes what would happen.",
                obj(json!({"platform": platform, "confirm": {"type": "boolean"}}), &["platform"]),
                false,
                true,
            ),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let acct = ctx.account_id;
        let confirmed = a["confirm"].as_bool() == Some(true);
        Some(match name {
            "list_ad_connections" => api::connections(state, acct).await,
            "list_ad_accounts" => match platform_arg(a) {
                Ok(p) => api::ad_accounts(state, acct, p).await,
                Err(e) => Err(e),
            },
            "list_audience_syncs" => {
                let f = api::SyncFilter { source_type: a["source_type"].as_str().map(String::from), source_id: a["source_id"].as_str().and_then(|s| s.parse().ok()) };
                api::list_syncs(state, acct, &f).await
            }
            "get_audience_sync" => match id_arg(a) {
                Ok(id) => api::get_sync(state, acct, id).await,
                Err(e) => Err(e),
            },
            "create_audience_sync" => match from::<api::CreateBody>(a.clone()) {
                Ok(b) => api::create_sync(state, acct, b, !confirmed).await,
                Err(e) => Err(e),
            },
            "update_audience_sync" => {
                let mut body = a.clone();
                if let Some(o) = body.as_object_mut() {
                    o.remove("id");
                }
                match (id_arg(a), from::<api::UpdateBody>(body)) {
                    (Ok(id), Ok(b)) => api::update_sync(state, acct, id, b).await,
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            }
            "run_audience_sync" => match id_arg(a) {
                Ok(id) => api::run_now(state, acct, id).await,
                Err(e) => Err(e),
            },
            "delete_audience_sync" => match id_arg(a) {
                Ok(id) => {
                    let on_platform = a["delete_on_platform"].as_bool() == Some(true);
                    if confirmed {
                        api::delete_sync(state, acct, id, on_platform).await
                    } else {
                        match store::account_sync(&state.db, acct, id).await {
                            Ok(Some(s)) => Ok(json!({"dry_run": true, "note": if on_platform {
                                format!("Nothing deleted. This would stop syncing \"{}\" and delete the audience on {}; ads that use it stop. Call again with confirm=true once the user approves.", s.name, s.platform().label())
                            } else {
                                format!("Nothing deleted. This would stop syncing \"{}\"; the audience stays on {} as it is now. Call again with confirm=true once the user approves.", s.name, s.platform().label())
                            }})),
                            Ok(None) => Err(AppError::not_found("sync")),
                            Err(e) => Err(e.into()),
                        }
                    }
                }
                Err(e) => Err(e),
            },
            "disconnect_ad_platform" => match platform_arg(a) {
                Ok(p) if confirmed => api::disconnect(state, acct, p).await,
                Ok(p) => Ok(json!({"dry_run": true, "note": format!("Nothing changed. This would revoke DumbMail's access to {} and delete its tokens; syncs to it wait until it's connected again. Call again with confirm=true once the user approves.", p.label())})),
                Err(e) => Err(e),
            },
            _ => return None,
        })
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "ad-audiences", name: "Ad audience sync", description: "Syncing lists and segments to Meta, Google Ads and TikTok, privacy rules, and operator setup for each platform", text: DOC }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        if kind != sync::JOB_KIND {
            return None;
        }
        Some(match payload["run_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            Some(id) => sync::step(state, id).await,
            None => Ok(()),
        })
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            sync::schedule(state).await?;
        }
        if tick.is_multiple_of(180) {
            store::refresh_meta_tokens(state).await?;
        }
        Ok(())
    }
}

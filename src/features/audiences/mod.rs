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

use super::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

const DOC: &str = include_str!("../../../docs/AD_AUDIENCES.md");

// Viewing connections, settings and syncs (counts only, no personal data) needs audience read.
// Anything that changes what's sent to an ad platform, or the sign-in, needs ad_audiences.manage
// (admins and the owner); for API keys that's the account scope. The OAuth callback is a public
// route (in team::access::PUBLIC_ROUTES) bound to the member who started the sign-in.
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/ad-audiences/connections", Perm::AudienceRead),
    rule("DELETE", "/api/ad-audiences/connections/{platform}", Perm::AdAudiencesManage),
    rule("POST", "/api/ad-audiences/connections/{platform}/start", Perm::AdAudiencesManage),
    rule("GET", "/api/ad-audiences/connections/{platform}/accounts", Perm::AudienceRead),
    rule("GET", "/api/ad-audiences/settings", Perm::AudienceRead),
    rule("PATCH", "/api/ad-audiences/settings", Perm::AdAudiencesManage),
    rule("GET", "/api/ad-audiences/syncs", Perm::AudienceRead),
    rule("POST", "/api/ad-audiences/syncs", Perm::AdAudiencesManage),
    rule("GET", "/api/ad-audiences/syncs/{id}", Perm::AudienceRead),
    rule("PATCH", "/api/ad-audiences/syncs/{id}", Perm::AdAudiencesManage),
    rule("DELETE", "/api/ad-audiences/syncs/{id}", Perm::AdAudiencesManage),
    rule("POST", "/api/ad-audiences/syncs/{id}/run", Perm::AdAudiencesManage),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "list_ad_connections", perm: Perm::AudienceRead },
    ToolRule { name: "list_ad_accounts", perm: Perm::AudienceRead },
    ToolRule { name: "list_audience_syncs", perm: Perm::AudienceRead },
    ToolRule { name: "get_audience_sync", perm: Perm::AudienceRead },
    ToolRule { name: "create_audience_sync", perm: Perm::AdAudiencesManage },
    ToolRule { name: "update_audience_sync", perm: Perm::AdAudiencesManage },
    ToolRule { name: "run_audience_sync", perm: Perm::AdAudiencesManage },
    ToolRule { name: "delete_audience_sync", perm: Perm::AdAudiencesManage },
    ToolRule { name: "disconnect_ad_platform", perm: Perm::AdAudiencesManage },
];

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

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let platform = json!({"enum": ["meta", "google", "tiktok"]});
        vec![
            tool(
                "list_ad_connections",
                "List ad platform connections",
                "Meta, Google Ads and TikTok: whether each is available on this server, connected, or needs reconnecting, plus the account's Google consent setting. Connecting needs someone signed in to DumbMail in a browser (an API key can't): send the user to connect_url (Settings → Ad audiences).",
                obj(json!({}), &[]),
                true,
                false,
            ),
            tool(
                "list_ad_accounts",
                "List ad accounts",
                "The ad accounts the connected sign-in can reach on Meta or TikTok, to pick from when creating a sync. Google returns manual_entry=true: ask the user for their Google Ads customer ID (and manager account ID if they reach it through one).",
                obj(json!({"platform": platform}), &["platform"]),
                true,
                false,
            ),
            tool(
                "list_audience_syncs",
                "List ad audience syncs",
                "Lists and segments synced to ad platform audiences, with status: last run; hashed identifiers sent, confirmed by the platform, and still waiting for removal; added and removed last run; the platform's estimated audience size and match rate; and any error in plain words. None of the three platforms has been verified against a live account yet (verified_live=false).",
                obj(json!({"source_type": {"enum": ["list", "segment"]}, "source_id": {"type": "string"}}), &[]),
                true,
                false,
            ),
            tool(
                "get_audience_sync",
                "Get an ad audience sync",
                "One sync's status, its recent run history, and batches still waiting for the platform.",
                obj(json!({"id": {"type": "string"}}), &["id"]),
                true,
                false,
            ),
            tool(
                "create_audience_sync",
                "Sync a list or segment to an ad audience",
                "Create a customer-list audience on Meta, Google Ads or TikTok from a list or segment and keep it in step (people who join are added, people who leave are removed). Only people subscribed to email who aren't suppressed, aren't on a marketing hold and haven't opted out of ads are sent, as hashed email (and phone numbers that were opted in to texts). Nothing else (no names or locations) is sent. Google needs the account's Google consent setting on (an owner or admin turns it on in the dashboard). TikTok audiences are email only and need at least 1,000 people. This sends customer data to the ad platform: without confirm=true it returns a dry run with the number of people. Get the user's approval, and pass accept_terms=true only when they confirm these people gave them their details directly and they accept the platform's customer list terms.",
                obj(
                    json!({
                        "platform": platform,
                        "ad_account_id": {"type": "string", "description": "From list_ad_accounts (Meta: without act_; Google: customer ID)"},
                        "login_account_id": {"type": "string", "description": "Google only: the manager account ID when the ad account is reached through one"},
                        "source_type": {"enum": ["list", "segment"]},
                        "source_id": {"type": "string"},
                        "name": {"type": "string", "maxLength": 100, "description": "Audience name on the platform. Default: the list or segment name + (DumbMail)"},
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
                "Pause or resume a sync (status), or change how often it runs. A paused sync adds nobody but still removes people who leave or opt out. A sync being deleted can't be changed. recreate_audience=true (with confirm=true) makes a new audience when the platform says the old one is gone, or a TikTok audience couldn't be confirmed.",
                obj(
                    json!({
                        "id": {"type": "string"},
                        "status": {"enum": ["active", "paused"]},
                        "interval_hours": {"type": "integer", "minimum": 1, "maximum": 168},
                        "recreate_audience": {"type": "boolean"},
                        "confirm": {"type": "boolean"}
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
                "Stop syncing. DumbMail first removes everyone it uploaded from the platform audience; the sync shows as removing (and keeps retrying) until the platform confirms every removal, then it's deleted. With delete_on_platform=true the empty audience is then deleted on the platform too (ads that use it stop); otherwise it's kept, empty. TikTok audiences are always deleted (TikTok can't keep fewer than 1,000 people). Without confirm=true it only describes what would happen.",
                obj(json!({"id": {"type": "string"}, "delete_on_platform": {"type": "boolean"}, "confirm": {"type": "boolean"}}), &["id"]),
                false,
                true,
            ),
            tool(
                "disconnect_ad_platform",
                "Disconnect an ad platform",
                "Delete DumbMail's sign-in for Meta, Google Ads or TikTok and ask the platform to revoke it; the answer says whether the platform confirmed. Syncs to that platform wait until it's connected again. Without confirm=true it only describes what would happen.",
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
                Ok(b) => api::create_sync(state, ctx, b, !confirmed).await,
                Err(e) => Err(e),
            },
            "update_audience_sync" => {
                let mut body = a.clone();
                if let Some(o) = body.as_object_mut() {
                    o.remove("id");
                }
                match (id_arg(a), from::<api::UpdateBody>(body)) {
                    (Ok(id), Ok(b)) => {
                        if b.recreate_audience == Some(true) && !confirmed {
                            Ok(json!({"dry_run": true, "note": "Nothing changed. This makes a new audience on the platform and uploads everyone who qualifies. Call again with confirm=true once the user approves."}))
                        } else {
                            api::update_sync(state, ctx, id, b).await
                        }
                    }
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            }
            "run_audience_sync" => match id_arg(a) {
                Ok(id) => api::run_now(state, ctx, id).await,
                Err(e) => Err(e),
            },
            "delete_audience_sync" => match id_arg(a) {
                Ok(id) => {
                    let on_platform = a["delete_on_platform"].as_bool() == Some(true);
                    if confirmed {
                        api::delete_sync(state, ctx, id, on_platform).await
                    } else {
                        match store::account_sync(&state.db, acct, id).await {
                            Ok(Some(s)) => Ok(json!({"dry_run": true, "note": api::delete_note(&s, on_platform)})),
                            Ok(None) => Err(AppError::not_found("sync")),
                            Err(e) => Err(e.into()),
                        }
                    }
                }
                Err(e) => Err(e),
            },
            "disconnect_ad_platform" => match platform_arg(a) {
                Ok(p) if confirmed => api::disconnect(state, ctx, p).await,
                Ok(p) => Ok(json!({"dry_run": true, "note": format!("Nothing changed. This would delete DumbMail's sign-in for {} and ask it to revoke DumbMail's access; syncs to it wait until it's connected again. Call again with confirm=true once the user approves.", p.label())})),
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
        Ok(())
    }
}

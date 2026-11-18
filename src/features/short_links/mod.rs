//! Short links in text messages: every link the sender wrote in a text becomes
//! `https://<link domain>/s/` plus 24 letters and digits, whatever its destination, so a link never
//! pushes a text into another billable segment, and link tags (UTM) can be added to texts for free.
//!
//! - [`code`]: the short path (target code, message reference, authentication tag).
//! - [`send`]: what gets shortened, the target rows (one per destination, not per recipient) and
//!   previews. Hooks in shared code, a line each: `sending::send` calls [`send::for_send`] and
//!   renders with `render::render_sms_with`, writing the message's reference on the row it already
//!   writes; `campaigns::prepare_send` calls [`send::freeze_campaign`]; the preview in
//!   `api::messaging` and the inbox's reply dry run use [`send::preview`].
//! - The click (`/s/...`, here) is verified, recorded by `public::record_click` exactly like a
//!   `/t/c/` click, and redirected to the stored destination.
//!
//! The setting is `accounts.sms_short_links` (on by default). Targets unused for 400 days are
//! deleted by a background job.

pub mod code;
pub mod runtime;
pub mod send;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::AppResult;
use crate::features::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use crate::features::team::audit;
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub use runtime::{check_index, index_ready, issuing, mark_serving};
pub use send::{for_send, freeze_campaign};

pub struct Module;

/// The index clicks find a message by: the account and the message's reference. Built in the
/// background after startup (`messages` is big); short links wait for it (see [`runtime`]).
pub const INDEXES: &[(&str, &str)] =
    &[("messages_short_ref_idx", "CREATE INDEX CONCURRENTLY messages_short_ref_idx ON messages (account_id, short_ref) WHERE short_ref IS NOT NULL")];
const INDEX_LOCK: i64 = 0x0d4d_7368_6f72_746c;

pub const JOB_CLEANUP: &str = "short_links_cleanup";
/// Targets not put in a text for this long are deleted.
pub const KEEP_DAYS: i32 = 400;
const CLEANUP_BATCH: i64 = 1000;
const CLEANUP_BATCHES_PER_JOB: usize = 20;

const DOC: &str = include_str!("../../../docs/SHORT_LINKS.md");

static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/short-links/settings", Perm::AccountRead),
    rule("PATCH", "/api/short-links/settings", Perm::SettingsWrite),
    public("GET", "/s/{path}"),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_short_link_settings", perm: Perm::AccountRead },
    ToolRule { name: "update_short_link_settings", perm: Perm::SettingsWrite },
];

/// Build the index, then remember it's in place.
pub async fn ensure_index(state: &AppState) -> anyhow::Result<()> {
    crate::features::reports::indexes::build(state, INDEX_LOCK, "short link", INDEXES).await?;
    check_index(state).await;
    Ok(())
}

// ---------- The click ----------

const LOOKUP_SQL: &str = "SELECT t.url, m.id FROM short_link_targets t
     JOIN accounts a ON a.id = t.account_id AND a.closed_at IS NULL
     JOIN messages m ON m.account_id = t.account_id AND m.short_ref = $2 AND m.channel = 'sms'
     WHERE t.code = $1 LIMIT 16";

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "link not found").into_response()
}

/// `/s/<target><message ref><tag>`: find the target and the messages with that reference in its
/// account, keep the one the tag was made for, record the click and redirect to the stored
/// destination. Anything else (wrong shape, unknown, tampered, deleted target, closed account) gets
/// the plain not-found page.
async fn click(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let Some((target, message_ref, tag)) = code::split(&path) else { return not_found() };
    let Ok(rows) = sqlx::query_as::<_, (String, Uuid)>(LOOKUP_SQL).bind(target).bind(message_ref).fetch_all(&state.db).await else { return not_found() };
    let Some((url, message_id)) = rows.into_iter().find(|(url, id)| code::verify(&state.cfg.secret_key, target, url, *id, tag)) else { return not_found() };
    // Only ever a destination the send path stored for this account.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return not_found();
    }
    crate::public::record_click(&state, message_id, &url).await
}

// ---------- Settings ----------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub enabled: bool,
}

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let enabled: bool = sqlx::query_scalar("SELECT sms_short_links FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let sample = send::sample_link(state, &account);
    Ok(json!({
        "enabled": enabled,
        "sample": sample,
        "link_length": sample.chars().count(),
        "ready": issuing(state),
    }))
}

pub async fn update(state: &AppState, ctx: &Ctx, b: SettingsBody) -> AppResult<Value> {
    let before: bool = sqlx::query_scalar("UPDATE accounts a SET sms_short_links = $2 FROM (SELECT id, sms_short_links AS before FROM accounts WHERE id = $1 FOR UPDATE) o WHERE a.id = o.id RETURNING o.before")
        .bind(ctx.account_id)
        .bind(b.enabled)
        .fetch_one(&state.db)
        .await?;
    if before != b.enabled {
        audit::audit(state, ctx, "short_links.update", None, json!({"enabled": b.enabled})).await;
    }
    settings_json(state, ctx.account_id).await
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}

async fn update_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SettingsBody>) -> AppResult<Json<Value>> {
    Ok(Json(update(&state, &ctx, b).await?))
}

// ---------- Housekeeping ----------

/// Delete targets unused for [`KEEP_DAYS`], in batches; queue another run when there's more.
///
/// The rows are picked first and deleted by a statement that checks their age again, against the
/// same cutoff: a send can refresh a picked row meanwhile (it waits for the send's lock, then sees
/// the new `last_used_at`), and that row stays.
pub async fn cleanup(state: &AppState) -> anyhow::Result<()> {
    let cutoff: chrono::DateTime<chrono::Utc> = chrono::Utc::now() - chrono::Duration::days(i64::from(KEEP_DAYS));
    for _ in 0..CLEANUP_BATCHES_PER_JOB {
        let stale: Vec<String> = sqlx::query_scalar("SELECT code FROM short_link_targets WHERE last_used_at < $1 ORDER BY last_used_at LIMIT $2")
            .bind(cutoff)
            .bind(CLEANUP_BATCH)
            .fetch_all(&state.db)
            .await?;
        if !stale.is_empty() {
            sqlx::query("DELETE FROM short_link_targets WHERE code = ANY($1) AND last_used_at < $2").bind(&stale).bind(cutoff).execute(&state.db).await?;
        }
        if (stale.len() as i64) < CLEANUP_BATCH {
            return Ok(());
        }
    }
    crate::jobs::enqueue_background(&state.db, JOB_CLEANUP, json!({}), None).await?;
    Ok(())
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "short_links"
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn routes(&self) -> Router<AppState> {
        Router::new().route("/api/short-links/settings", get(settings_h).patch(update_h)).route("/s/{path}", get(click))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        vec![
            tool(
                "get_short_link_settings",
                "Get short link settings",
                "Whether links in text messages are shortened. On (the default), every link the sender wrote in a text becomes a short link of the same length whatever it points to (`sample` shows one, `link_length` its length), so a link never adds a billable segment, and link tags (UTM) are added to texts too. Off, links keep the long tracked form and texts aren't tagged. `ready` is false only briefly after an upgrade, while short links wait for a database index.",
                obj(json!({}), &[]),
                true,
                false,
            ),
            tool(
                "update_short_link_settings",
                "Turn short links in texts on or off",
                "Turn short links in text messages on or off for this account. Affects texts rendered from now on; links already sent keep working. A campaign keeps the setting it started with.",
                obj(json!({"enabled": {"type": "boolean", "description": "true to shorten links in texts"}}), &["enabled"]),
                false,
                false,
            ),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let r = match name {
            "get_short_link_settings" => settings_json(state, ctx.account_id).await,
            "update_short_link_settings" => async { update(state, ctx, from(a.clone())?).await }.await,
            _ => return None,
        };
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "short-links", name: "Short links in texts", description: "How links in text messages are shortened, tagged, verified and cleaned up", text: DOC }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, _payload: &Value) -> Option<anyhow::Result<()>> {
        if kind != JOB_CLEANUP {
            return None;
        }
        Some(cleanup(state).await)
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        // Another server may be the one building the index: notice when it's done, and notice if it
        // ever goes missing (clicks then answer not found instead of scanning `messages`).
        if tick.is_multiple_of(3) {
            check_index(state).await;
        }
        if tick.is_multiple_of(180) {
            crate::jobs::enqueue_background(&state.db, JOB_CLEANUP, json!({}), None).await?;
        }
        Ok(())
    }

    async fn after_start(&self, state: &AppState) -> anyhow::Result<()> {
        ensure_index(state).await
    }
}

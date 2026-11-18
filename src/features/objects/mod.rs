//! Custom objects: an account's own records (reservations, subscriptions, pets ...) with typed
//! fields and dates, linked to one or more people, with every change recorded. Flows start off
//! object changes and dates, segments test them, and integrations write them. `docs/OBJECTS.md` is
//! the reference.
//!
//! - `types`: object types and their fields (checking and normalizing values)
//! - `store`: upserts, links, history, deletes and views
//! - `flow`: object triggers, object-date fires (on the event-date machinery in `flows`), the
//!   per-step check for object runs
//! - `bulk`: bulk upserts written by a background job
//! - `api`: REST routes
//!
//! Nothing about objects goes to webhooks or data exports yet. Logs and audit entries carry only
//! ids, type keys, counts and changed field names, never field values.

pub mod api;
pub mod bulk;
pub mod flow;
pub mod hooks;
pub mod store;
pub mod types;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{Access, Perm, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "list_object_types", perm: Perm::AudienceRead },
    ToolRule { name: "upsert_object_type", perm: Perm::SettingsWrite },
    ToolRule { name: "delete_object_type", perm: Perm::SettingsWrite },
    ToolRule { name: "list_objects", perm: Perm::AudienceRead },
    ToolRule { name: "get_object", perm: Perm::AudienceRead },
    ToolRule { name: "upsert_objects", perm: Perm::AudienceWrite },
    ToolRule { name: "get_object_bulk_upsert", perm: Perm::AudienceRead },
    ToolRule { name: "delete_object", perm: Perm::AudienceWrite },
];

const DOC: &str = include_str!("../../../docs/OBJECTS.md");

/// Objects one `upsert_objects` call writes at once; more go to a bulk upsert.
const MCP_INLINE: usize = 50;
/// History rows trimmed per run of the trim job.
const TRIM_BATCH: i64 = 5000;
pub const JOB_TRIM: &str = "objects_trim";

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

fn field_schema() -> Value {
    json!({"type": "object", "properties": {
        "key": {"type": "string", "description": "Letters, digits and _ (e.g. check_in)"},
        "label": {"type": "string"},
        "type": {"enum": ["text", "number", "boolean", "date", "datetime", "enum", "list", "money"]},
        "options": {"type": "array", "items": {"type": "string"}, "description": "enum: the allowed values"},
        "currency": {"type": "string", "description": "money: e.g. USD"},
        "sensitive": {"type": "boolean", "description": "Private (door codes): hidden from lists and from people who can't read profiles"}
    }, "required": ["key", "type"]})
}

fn object_schema() -> Value {
    json!({"type": "object", "properties": {
        "type": {"type": "string", "description": "The type's key, e.g. reservation (made on first use, with field types guessed from the values)"},
        "external_id": {"type": "string", "description": "Its id in your system (1-200 characters)"},
        "fields": {"type": "object", "description": "Merged into the object's fields; null removes one. Dates as 2026-11-20, date-times with their offset."},
        "profiles": {"type": "array", "maxItems": 20, "items": {"type": "object", "properties": {
            "id": {"type": "string"}, "email": {"type": "string"}, "phone_number": {"type": "string"}, "external_id": {"type": "string"},
            "relationship": {"type": "string", "description": "e.g. guest, owner"}}},
            "description": "People it belongs to (added to those already linked). Found by id or identifiers; someone new is created, never subscribed to anything."},
        "as_of": {"description": "The change's time or version in your system: an upsert older than the newest one written is ignored"}
    }, "required": ["type", "external_id"]})
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "objects"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn access(&self) -> Access {
        Access { routes: api::ROUTES, tools: TOOLS }
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "objects",
            name: "Custom objects",
            description: "Reservations, subscriptions and other records of your own: types and fields, upserts with history, flows off object changes and dates, object segments, integration actions",
            text: DOC,
        }]
    }

    fn mcp_tools(&self) -> Vec<Value> {
        vec![
            tool("list_object_types", "List object types", "The account's object types (reservation, subscription ...) with their fields and how many objects each has.", obj(json!({}), &[]), true, false),
            tool("upsert_object_type", "Create or change an object type", "Create a type, or change its name, plural, fields (replaces the list) or title field. Types are also made on the first upsert of an object, with field types guessed from the values; use this to fix them, add enum options or mark fields sensitive.",
                obj(json!({"key": {"type": "string", "description": "Lower-case letters, digits and _"}, "name": {"type": "string"}, "plural": {"type": "string"},
                    "fields": {"type": "array", "items": field_schema(), "maxItems": 100}, "title_field": {"type": "string", "description": "The field shown as each object's title (\"\" clears)"}}), &["key"]), false, false),
            tool("delete_object_type", "Delete an object type", "Delete a type and all its objects, with their history. Refused while a flow or segment uses it. Needs confirm=true; without it you get a dry run with the count.",
                obj(json!({"key": {"type": "string"}, "confirm": {"type": "boolean"}}), &["key"]), false, true),
            tool("list_objects", "List objects", "A type's objects, newest change first, searched by title or external id (sensitive fields are never in lists). Or, with profile_id, one person's objects by type.",
                obj(json!({"type": {"type": "string"}, "q": {"type": "string"}, "profile_id": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
            tool("get_object", "Get an object", "One object (by id, or type + external_id) with its change history. Linked people and sensitive fields are shown only to callers who can read profiles.",
                obj(json!({"id": {"type": "string"}, "type": {"type": "string"}, "external_id": {"type": "string"}}), &[]), true, false),
            tool("upsert_objects", "Create or update objects", "Create or update up to 1000 objects by (type, external_id). Fields merge; every change is recorded and can start object flows. Up to 50 are written now and their results returned; more are queued as a bulk upsert (check it with get_object_bulk_upsert).",
                obj(json!({"objects": {"type": "array", "items": object_schema(), "minItems": 1, "maxItems": 1000}}), &["objects"]), false, false),
            tool("get_object_bulk_upsert", "Check a bulk upsert", "Progress of a bulk upsert: status (queued, running, done, failed), counts and the first errors.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
            tool("delete_object", "Delete objects", "Delete one object (id) or several (ids, or type + external_ids; up to 500), with their history. People in flows for them leave those flows. Needs confirm=true; without it you get a dry run.",
                obj(json!({"id": {"type": "string"}, "ids": {"type": "array", "items": {"type": "string"}, "maxItems": 500}, "type": {"type": "string"}, "external_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 500}, "confirm": {"type": "boolean"}}), &[]), false, true),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        if !TOOLS.iter().any(|t| t.name == name) {
            return None;
        }
        let acct = ctx.account_id;
        let r = async {
            match name {
                "list_object_types" => api::list_types(state, acct).await,
                "upsert_object_type" => api::upsert_type(state, ctx, from(a.clone())?).await,
                "delete_object_type" => api::delete_type(state, ctx, a["key"].as_str().unwrap_or(""), a["confirm"].as_bool() == Some(true)).await,
                "list_objects" => {
                    if let Some(pid) = a.get("profile_id").filter(|v| !v.is_null()) {
                        ctx.require(Perm::ProfilesRead)?;
                        let pid = pid.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("`profile_id` must be an id"))?;
                        return store::for_profile(&state.db, acct, pid).await;
                    }
                    let key = a["type"].as_str().ok_or_else(|| AppError::bad("give the object type (type) or a profile_id"))?;
                    let t = types::require(&state.db, acct, key).await?;
                    store::list(&state.db, &t, a["q"].as_str(), a["limit"].as_i64().unwrap_or(50).clamp(1, 200), a["offset"].as_i64().unwrap_or(0).clamp(0, 1_000_000)).await
                }
                "get_object" => {
                    let id = match (a.get("id").and_then(Value::as_str), a["type"].as_str(), a["external_id"].as_str()) {
                        (Some(_), _, _) => uuid_arg(a, "id")?,
                        (None, Some(t), Some(e)) => store::by_external(&state.db, acct, t, e).await?,
                        _ => return Err(AppError::bad("give the object's id, or its type and external_id")),
                    };
                    store::detail(&state.db, acct, id, ctx.can(Perm::ProfilesRead)).await
                }
                "upsert_objects" => {
                    let items: Vec<store::UpsertInput> = from(a.get("objects").cloned().unwrap_or(json!([])))?;
                    if items.len() > MCP_INLINE {
                        let v = bulk::start(state, acct, "mcp", &items).await?;
                        super::team::audit(state, ctx, "objects.bulk_upsert", v["id"].as_str(), json!({"count": items.len()})).await;
                        return Ok(json!({"queued": v, "note": "Queued as a bulk upsert; check it with get_object_bulk_upsert."}));
                    }
                    if items.is_empty() {
                        return Err(AppError::bad("give at least one object"));
                    }
                    for (i, b) in items.iter().enumerate() {
                        store::check_input(b).map_err(|e| AppError::bad(format!("object {i}: {}", e.public_message())))?;
                    }
                    let mut out = Vec::with_capacity(items.len());
                    for (i, b) in items.iter().enumerate() {
                        match store::upsert(state, &store::Writer::new(acct, "mcp"), b).await {
                            Ok(r) => out.push(json!(r)),
                            Err(AppError::Internal(e)) => return Err(AppError::Internal(e)),
                            Err(e) => out.push(json!({"index": i, "external_id": b.external_id, "error": e.public_message()})),
                        }
                    }
                    Ok(json!({"results": out}))
                }
                "get_object_bulk_upsert" => bulk::view(&state.db, acct, uuid_arg(a, "id")?).await,
                "delete_object" => {
                    let mut ids: Vec<Uuid> = from(a.get("ids").cloned().filter(|v| !v.is_null()).unwrap_or(json!([])))?;
                    if a.get("id").is_some_and(|v| !v.is_null()) {
                        ids.push(uuid_arg(a, "id")?);
                    }
                    let body = api::DeleteBody {
                        ids,
                        type_key: a["type"].as_str().map(String::from),
                        external_ids: from(a.get("external_ids").cloned().filter(|v| !v.is_null()).unwrap_or(json!([])))?,
                        confirm: a["confirm"].as_bool() == Some(true),
                    };
                    let ids = api::delete_ids(state, acct, &body).await?;
                    api::delete_objects(state, ctx, ids, body.confirm).await
                }
                other => Err(AppError::bad(format!("unknown tool {other}"))),
            }
        }
        .await;
        Some(r)
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let id = |k: &str| payload[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
        match kind {
            flow::JOB_CHANGE => Some(match id("change_id") {
                Some(c) => flow::run_change(state, c).await,
                None => Ok(()),
            }),
            bulk::JOB => Some(match id("bulk_id") {
                Some(b) => bulk::run(state, b).await,
                None => Ok(()),
            }),
            JOB_TRIM => Some(trim(state, id("after")).await),
            types::JOB_PURGE => Some(match (id("type_id"), payload["field"].as_str()) {
                (Some(t), Some(f)) => types::purge_field(&state.db, t, f).await.map_err(|e| anyhow::anyhow!(e.public_message())),
                _ => Ok(()),
            }),
            _ => None,
        }
    }

    /// About hourly: queue a history trim (in the background lane) unless one is waiting.
    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(180) {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND run_at <> 'infinity')").bind(JOB_TRIM).fetch_one(&state.db).await?;
            if !waiting {
                crate::jobs::enqueue_background(&state.db, JOB_TRIM, json!({}), None).await?;
            }
        }
        Ok(())
    }
}

/// Job: trim change history a batch at a time, handing over to a fresh job while there's more.
async fn trim(state: &AppState, after: Option<Uuid>) -> anyhow::Result<()> {
    let (n, next) = store::trim_history(&state.db, TRIM_BATCH, after).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    // More objects to look at, or a full batch of old changes: carry on shortly from the cursor.
    if next.is_some() || n as i64 >= TRIM_BATCH {
        crate::jobs::enqueue_background(&state.db, JOB_TRIM, json!({"after": next}), Some(chrono::Utc::now() + chrono::Duration::seconds(5))).await?;
    }
    Ok(())
}

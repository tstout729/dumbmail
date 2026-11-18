//! MCP tools for webhooks. Each one calls the same function as the REST API.

use serde_json::{json, Value};
use uuid::Uuid;

use super::api::{self, Who};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{Perm, ToolRule};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

/// Who may use each tool (the same rules as the REST routes).
pub static TOOLS: &[ToolRule] = &[
    ToolRule { name: "list_webhook_topics", perm: Perm::WebhooksRead },
    ToolRule { name: "list_webhooks", perm: Perm::WebhooksRead },
    ToolRule { name: "create_webhook", perm: Perm::WebhooksManage },
    ToolRule { name: "update_webhook", perm: Perm::WebhooksManage },
    ToolRule { name: "delete_webhook", perm: Perm::WebhooksManage },
    ToolRule { name: "rotate_webhook_secret", perm: Perm::WebhooksManage },
    ToolRule { name: "send_test_webhook", perm: Perm::WebhooksManage },
    // With delivery_id it returns a payload, which also needs webhooks.manage (checked below).
    ToolRule { name: "list_webhook_deliveries", perm: Perm::WebhooksRead },
    ToolRule { name: "resend_webhook_deliveries", perm: Perm::WebhooksManage },
    ToolRule { name: "get_flow_webhook_secret", perm: Perm::WebhooksRead },
    ToolRule { name: "rotate_flow_webhook_secret", perm: Perm::WebhooksManage },
];

fn endpoint_props() -> Value {
    json!({
        "url": {"type": "string", "description": "https:// URL on the public internet that receives the POSTs."},
        "description": {"type": "string", "maxLength": 500},
        "topics": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Topic names from list_webhook_topics, e.g. event.tracked, profile.unsubscribed, message.opened."},
        "metrics": {"type": "array", "items": {"type": "string"}, "maxItems": 50, "description": "For event.tracked: only these metrics (e.g. [\"Placed Order\"]). Empty or missing: every event."},
        "filter": {"type": ["array", "null"], "maxItems": 20, "description": "Only send events whose data matches every condition: [{field, op, value}], field is a dotted path into the event's data (e.g. event.value, list.id, profile.email). Ops as in segment property conditions. null or [] removes it.",
            "items": {"type": "object", "properties": {"field": {"type": "string"}, "op": {"type": "string"}, "value": {}}, "required": ["field", "op"]}},
        "trusted_only": {"type": "boolean", "description": "Skip events a browser sent through the public site key (source browser). Default false: every source, with the source in each envelope."},
        "enabled": {"type": "boolean"},
        "batch_size": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Events per request. 1 sends one JSON object per request; more sends a JSON array. Default 50."},
        "rate_per_second": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Most requests per second to this endpoint. Default 10."}
    })
}

pub fn defs() -> Vec<Value> {
    let mut update = endpoint_props();
    update["id"] = json!({"type": "string"});
    update["revision"] = json!({"type": "integer", "description": "The endpoint's revision as you read it (list_webhooks). If its URL changed since, the update is refused, so an old address is never put back."});
    vec![
        tool("list_webhook_topics", "List webhook topics", "Every topic an outgoing webhook can subscribe to, with what it means.", obj(json!({}), &[]), true, false),
        tool("list_webhooks", "List webhook endpoints", "The account's webhook endpoints (including Zapier/Make hooks): URL, topics, on/off, failing state and 24-hour delivery counts. Secrets are never shown, only a hint.", obj(json!({}), &[]), true, false),
        tool("create_webhook", "Create a webhook endpoint", "Start sending events to a URL. Every request is signed (see the dumbmail://docs/webhooks resource). Returns the signing secret once: give it to the user to store; it can't be shown again. Payloads carry personal data, so only use URLs the user controls.",
            obj(endpoint_props(), &["url", "topics"]), false, false),
        tool("update_webhook", "Update a webhook endpoint", "Change an endpoint's URL, topics, metrics, filter, sources, batch size or rate, or turn it on or off. Turning it back on clears its failure state. Changing the URL cancels deliveries still waiting for the old URL (they're never sent to the new one).", obj(update, &["id"]), false, false),
        tool("delete_webhook", "Delete a webhook endpoint", "Delete an endpoint and its delivery log. Without confirm=true it only describes what would happen.", obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("rotate_webhook_secret", "Rotate a webhook secret", "Make a new signing secret for an endpoint and return it once. The old secret keeps signing too for 24 hours, so the receiver can switch over.", obj(json!({"id": {"type": "string"}}), &["id"]), false, false),
        tool("send_test_webhook", "Send a test webhook", "Send one sample event (marked \"test\": true) to an endpoint now and report the response.", obj(json!({"id": {"type": "string"}, "topic": {"type": "string", "description": "Default: the endpoint's first topic."}}), &["id"]), false, false),
        tool("list_webhook_deliveries", "List webhook deliveries", "An endpoint's delivery log, newest first (kept 14 days): status, attempts, response code, error class and duration. Pass delivery_id for one delivery with its payload (personal data).",
            obj(json!({"id": {"type": "string"}, "delivery_id": {"type": "string"}, "status": {"enum": ["pending", "sending", "delivered", "failed", "dropped", "cancelled", "all"]}, "topic": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &["id"]), true, false),
        tool("resend_webhook_deliveries", "Resend webhook deliveries", "Queue earlier deliveries to the endpoint's current URL again, by delivery_ids or a since/until time range (up to 10,000; status failed by default; once a minute per endpoint). Events keep their ids so receivers can drop duplicates.",
            obj(json!({"id": {"type": "string"}, "delivery_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 1000}, "since": {"type": "string", "format": "date-time"}, "until": {"type": "string", "format": "date-time"}, "status": {"enum": ["failed", "delivered", "dropped", "all"]}}), &["id"]), false, false),
        tool("get_flow_webhook_secret", "Get the flow webhook secret", "Hint and age of the secret that signs flow webhook steps (the secret itself is never shown again).", obj(json!({}), &[]), true, false),
        tool("rotate_flow_webhook_secret", "Rotate the flow webhook secret", "Make a new secret for signing flow webhook steps and return it once. The old one stops working right away, so update the receiving systems.", obj(json!({}), &[]), false, false),
    ]
}

fn id_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("{k} must be an id")))
}

fn without(a: &Value, keys: &[&str]) -> Value {
    let mut m = a.as_object().cloned().unwrap_or_default();
    for k in keys {
        m.remove(*k);
    }
    Value::Object(m)
}

pub async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
    let acct = ctx.account_id;
    let who = Who::of(ctx);
    let r = match name {
        "list_webhook_topics" => Ok(json!({"topics": super::topics::list_json()})),
        "list_webhooks" => api::list(state, who).await.map(|v| json!({"webhooks": v})),
        "create_webhook" => api::create(state, ctx, a, "api", 50, None).await,
        "update_webhook" => match id_arg(a, "id") {
            Ok(id) => api::update(state, ctx, id, &without(a, &["id"])).await,
            Err(e) => Err(e),
        },
        "delete_webhook" => async {
            let id = id_arg(a, "id")?;
            let ep = api::get_one(state, who, id).await?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "webhook": ep, "note": "Nothing deleted. This stops all deliveries to this URL and deletes its delivery log. Call again with confirm=true once the user approves."}));
            }
            api::delete(state, ctx, id).await?;
            Ok(json!({"deleted": true}))
        }
        .await,
        "rotate_webhook_secret" => match id_arg(a, "id") {
            Ok(id) => api::rotate(state, ctx, id).await,
            Err(e) => Err(e),
        },
        "send_test_webhook" => match id_arg(a, "id") {
            Ok(id) => api::send_test(state, ctx, id, a["topic"].as_str()).await,
            Err(e) => Err(e),
        },
        "list_webhook_deliveries" => async {
            let id = id_arg(a, "id")?;
            if a.get("delivery_id").is_some_and(|v| !v.is_null()) {
                crate::features::team::access::require(ctx, Perm::WebhooksManage)?;
                return api::delivery(state, who, id, id_arg(a, "delivery_id")?).await;
            }
            let q = api::DeliveryQuery {
                status: a["status"].as_str().map(String::from),
                topic: a["topic"].as_str().map(String::from),
                limit: a["limit"].as_i64(),
                offset: a["offset"].as_i64(),
            };
            let (rows, total) = api::deliveries(state, who, id, &q).await?;
            Ok(json!({"deliveries": rows, "total": total}))
        }
        .await,
        "resend_webhook_deliveries" => async {
            let id = id_arg(a, "id")?;
            api::resend(state, ctx, id, from(without(a, &["id"]))?).await
        }
        .await,
        "get_flow_webhook_secret" => api::flow_secret_info(state, acct).await,
        "rotate_flow_webhook_secret" => api::rotate_flow_secret(state, ctx).await,
        _ => return None,
    };
    Some(r)
}

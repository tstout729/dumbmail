//! Model Context Protocol server (Streamable HTTP, JSON responses) at POST /mcp.
//! Every tool reuses the same functions as the REST API, so validation and safety rules match.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api::{analytics, audience as aud, messaging as msg};
use crate::auth::Ctx;
use crate::campaigns::{self, load_account, Audience};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub fn routes() -> Router<AppState> {
    Router::new().route("/mcp", post(handle).get(no_stream).delete(no_stream))
}

async fn no_stream() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "This MCP server uses JSON responses; POST JSON-RPC messages to /mcp.").into_response()
}

async fn handle(State(state): State<AppState>, ctx: Result<Ctx, AppError>, body: axum::body::Bytes) -> Response {
    let ctx = match ctx {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                [("WWW-Authenticate", "Bearer")],
                Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32001, "message": "Send Authorization: Bearer <DumbMail API key>. Create one in Settings → Developers."}})),
            )
                .into_response()
        }
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})).into_response(),
    };
    match parsed {
        Value::Array(items) => {
            let mut out = Vec::new();
            for it in items {
                if let Some(r) = dispatch(&state, &ctx, it).await {
                    out.push(r);
                }
            }
            if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { Json(Value::Array(out)).into_response() }
        }
        single => match dispatch(&state, &ctx, single).await {
            Some(r) => Json(r).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

async fn dispatch(state: &AppState, ctx: &Ctx, req: Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req["method"].as_str().unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));
    // Notifications get no response.
    let id = id?;
    let result: Result<Value, (i64, String)> = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}, "prompts": {"listChanged": false}, "resources": {"listChanged": false}},
                "serverInfo": {"name": "dumbmail", "title": "DumbMail email & SMS marketing", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            let mut all = tools();
            all.extend(crate::features::mcp_tools());
            Ok(json!({"tools": all}))
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let res = match crate::features::mcp_call(state, ctx, &name, &args).await {
                Some(r) => r,
                None => call(state, ctx, &name, args).await,
            };
            Ok(match res {
                Ok(v) => {
                    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
                    let structured = if v.is_object() { v } else { json!({"result": v}) };
                    json!({"content": [{"type": "text", "text": text}], "structuredContent": structured, "isError": false})
                }
                Err(e) => json!({"content": [{"type": "text", "text": format!("Error ({}): {}", e.code(), e.public_message())}], "isError": true}),
            })
        }
        "prompts/list" => Ok(json!({"prompts": prompts()})),
        "prompts/get" => prompt(params["name"].as_str().unwrap_or(""), &params["arguments"]).ok_or((-32602, "unknown prompt".into())),
        "resources/list" => {
            let mut resources = vec![
            json!({"uri": "dumbmail://docs/segments", "name": "Segment rules", "description": "How to write segment definitions, flow splits and filters", "mimeType": "text/markdown"}),
            json!({"uri": "dumbmail://docs/email", "name": "Email content blocks", "description": "Email block types and personalization variables", "mimeType": "text/markdown"}),
            json!({"uri": "dumbmail://docs/flows", "name": "Flow definitions", "description": "Triggers, steps and settings for flows", "mimeType": "text/markdown"}),
            json!({"uri": "dumbmail://docs/forms", "name": "Sign-up form reference", "description": "Every form config key: content, fields, phone step, style, teaser, targeting, A/B versions, images and previews", "mimeType": "text/markdown"})
            ];
            for d in crate::features::mcp_docs() {
                resources.push(json!({"uri": format!("dumbmail://docs/{}", d.slug), "name": d.name, "description": d.description, "mimeType": "text/markdown"}));
            }
            Ok(json!({"resources": resources}))
        }
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "resources/read" => {
            let uri = params["uri"].as_str().unwrap_or("");
            let feature_docs = crate::features::mcp_docs();
            let text = match uri {
                "dumbmail://docs/segments" | "postlark://docs/segments" => Some(SEGMENT_DOC),
                "dumbmail://docs/email" | "postlark://docs/email" => Some(EMAIL_DOC),
                "dumbmail://docs/flows" | "postlark://docs/flows" => Some(FLOW_DOC),
                "dumbmail://docs/forms" => Some(FORMS_DOC),
                other => other.strip_prefix("dumbmail://docs/").and_then(|slug| feature_docs.iter().find(|d| d.slug == slug)).map(|d| d.text),
            };
            text.map(|t| json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": t}]})).ok_or((-32002, format!("resource not found: {uri}")))
        }
        "logging/setLevel" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

const INSTRUCTIONS: &str = "DumbMail is an email and SMS marketing platform (an open-source Klaviyo alternative). \
Start with get_account to see sending readiness, plan usage and what exists. \
Audience: profiles, lists (static membership) and segments (live rules; test rules with preview_segment before saving). \
Messaging: campaigns are one-off sends to lists/segments; flows are automations started by a trigger (event/metric, list join, segment entry, date property). \
Email content is a list of blocks (see the dumbmail://docs/email resource) personalized with Jinja tags like {{ first_name|default('there') }}. \
Always render_preview new content and send_test before sending. Sending a campaign or turning a flow live reaches real people: \
the tools require confirm=true and you should get the user's explicit go-ahead first. Data: get_analytics, query_metric, campaign_performance, get_campaign_report, get_flow_report. \
Sign-up forms: create_form / update_form take a config described in the dumbmail://docs/forms resource; check one with get_form's preview_url before publishing, and upload images with upload_image. \
Times are ISO 8601; the account has a time zone (default America/Los_Angeles) used for reporting buckets.";

pub fn tool(name: &str, title: &str, description: &str, schema: Value, read_only: bool, destructive: bool) -> Value {
    json!({
        "name": name, "title": title, "description": description, "inputSchema": schema,
        "annotations": {"title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": read_only, "openWorldHint": destructive}
    })
}

pub fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

/// JSON Schema for a sign-up form's `config` (docs/FORMS.md has the full reference).
fn form_config_schema() -> Value {
    let text = |max: u64, d: &str| json!({"type": ["string", "null"], "maxLength": max, "description": d});
    let url = |d: &str| json!({"type": ["string", "null"], "format": "uri", "description": d});
    let int = |lo: i64, hi: i64, d: &str| json!({"type": ["integer", "null"], "minimum": lo, "maximum": hi, "description": d});
    let color = |d: &str| json!({"type": ["string", "null"], "pattern": "^#[0-9a-fA-F]{3,8}$", "description": d});
    let one_of = |opts: &[&str], d: &str| json!({"type": ["string", "null"], "enum": opts.iter().map(|o| json!(o)).chain(std::iter::once(Value::Null)).collect::<Vec<_>>(), "description": d});
    let boolean = |d: &str| json!({"type": ["boolean", "null"], "description": d});
    let nested = |props: Value, d: &str| json!({"type": ["object", "null"], "properties": props, "additionalProperties": false, "description": d});
    let mut variant_props = json!({"id": {"enum": ["b", "c", "d"]}});
    let content = [
        ("headline", text(200, "Per version. Default \"Get 10% off your first order\".")),
        ("body", text(1000, "Per version. Line breaks are kept.")),
        ("button_text", text(60, "Per version. Default \"Sign up\".")),
        ("image_url", url("Per version. http(s) URL; upload with upload_image. Placement follows style.layout.")),
        ("image_alt", text(200, "Per version. Alt text for the image.")),
        ("no_thanks_text", text(60, "Per version. A \"No thanks\" link under the button that closes the form (popup/flyout).")),
        ("success_message", text(200, "Per version. Thank-you headline. Default \"Thanks! Check your inbox.\"")),
        ("success_body", text(1000, "Per version. Text under the thank-you headline.")),
        ("discount_code", text(60, "Per version. Shown on the thank-you screen with a Copy button.")),
    ];
    let mut props = serde_json::Map::new();
    for (k, v) in content {
        // The base key's own schema (format, maxLength...), plus null, so nothing copied can be missing.
        let mut vp = v.clone();
        vp["type"] = json!(["string", "null"]);
        vp["description"] = json!("Empty or missing falls back to version A.");
        variant_props[k] = vp;
        props.insert(k.into(), v);
    }
    let field = json!({"type": "object", "additionalProperties": false, "required": ["key"], "properties": {
        "key": {"type": "string", "pattern": "^[a-z][a-z0-9_]{0,39}$", "description": "email, phone_number, first_name, last_name, birthday, or a custom profile property key"},
        "type": {"enum": ["text", "number", "date", "select", "radio", "checkboxes", "checkbox"], "description": "Custom fields only (default text). checkbox saves true/false; checkboxes saves a list."},
        "label": {"type": "string", "maxLength": 100},
        "placeholder": {"type": "string", "maxLength": 100},
        "required": {"type": "boolean", "description": "Default true for email, false otherwise. Checked on the server."},
        "options": {"type": "array", "minItems": 1, "maxItems": 20, "items": {"type": "string", "maxLength": 80}, "description": "Required for select, radio and checkboxes."}
    }});
    let style = nested(json!({
            "layout": one_of(&["stacked", "image_left", "image_right", "background"], "Where image_url goes. Default stacked."),
            "width": int(280, 900, "px. Default popup 420, flyout 360, embed 480, page 440."),
            "align": one_of(&["center", "left"], "Text alignment. Default center."),
            "font": one_of(&["system", "inherit", "serif", "Inter", "Poppins", "Montserrat", "Lato", "DM Sans", "Space Grotesk", "Playfair Display", "Lora", "Merriweather", "Oswald"], "Default system; inherit uses the website's font."),
            "headline_size": int(14, 56, "px. Default 24."),
            "headline_weight": {"enum": [400, 500, 600, 700, 800, null], "description": "Default 700."},
            "body_size": int(12, 22, "px. Default 15."),
            "background_color": color("Default #ffffff."),
            "headline_color": color("Default #111827."),
            "text_color": color("Body, labels and small print. Default #4b5563."),
            "button_color": color("Default brand_color."),
            "button_text_color": color("Default white or black, whichever reads better."),
            "button_full_width": boolean("Default true."),
            "input_background": color("Default #ffffff."),
            "input_border_color": color("Default #d1d5db."),
            "input_text_color": color("Default #111827."),
            "corner_radius": int(0, 40, "Default 14."),
            "button_radius": int(0, 40, "Default 8; 40 makes a pill."),
            "input_radius": int(0, 40, "Default 8."),
            "padding": int(12, 64, "Default 28."),
            "border_width": int(0, 8, "Default 0 (embed and page 1)."),
            "border_color": color("Default #e5e7eb."),
            "shadow": one_of(&["none", "soft", "strong"], "Default popup/flyout strong, embed/page none."),
            "overlay_color": color("Popup only. Default #111827."),
            "overlay_opacity": int(0, 90, "Percent. Default 55."),
            "close_on_overlay": boolean("Popup only. Default true."),
            "close_color": color("The close button. Default #9ca3af."),
            "position": one_of(&["center", "top", "bottom", "bottom_right", "bottom_left", "top_right", "top_left"], "Popup: center, top, bottom (default center). Flyout: bottom_right (default), bottom_left, top_right, top_left."),
            "animation": one_of(&["fade", "slide", "zoom", "none"], "Default fade."),
            "page_background": color("Hosted page only. Default a light tint of the button color."),
            "custom_css": {"type": ["string", "null"], "maxLength": 10000, "description": "Extra CSS inside the form's shadow root (no '<'). Hooks: .dm-card, .dm-media, .dm-content, .dm-headline, .dm-body, .dm-field, .dm-label, .dm-input, .dm-consent, .dm-button, .dm-no-thanks, .dm-legal, .dm-error, .dm-success, .dm-code, .dm-close, .dm-overlay, .dm-teaser."}
        }), "Look of the form, shared by every A/B version. Replaced whole on update.");
    let teaser = nested(json!({
            "enabled": {"type": "boolean"},
            "text": {"type": "string", "maxLength": 40, "description": "Default: headline."},
            "position": {"enum": ["bottom_left", "bottom_right", "left", "right"]},
            "show": {"enum": ["after_close", "always"], "description": "Default after_close."}
        }), "Popup/flyout: a small tab that reopens the form.");
    let targeting = nested(json!({
            "trigger": {"enum": ["delay", "scroll", "exit_intent", "manual"], "description": "Default delay (uses delay_seconds). manual opens only from the teaser, a data-dumbmail-open element or dumbmail.showForm(id)."},
            "scroll_percent": {"type": "integer", "minimum": 1, "maximum": 100, "description": "For scroll. Default 40."},
            "devices": {"enum": ["all", "desktop", "mobile"]},
            "url_contains": {"type": "array", "maxItems": 50, "items": {"type": "string", "maxLength": 500}},
            "url_excludes": {"type": "array", "maxItems": 50, "items": {"type": "string", "maxLength": 500}},
            "page_views": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Pages seen this visit before it shows. Default 1."},
            "visitors": {"enum": ["all", "new", "returning"]},
            "frequency_days": {"type": "integer", "minimum": 0, "maximum": 365, "description": "Wait after a close. Default 7."},
            "hide_for_subscribers": {"type": "boolean", "description": "Default true."},
            "start_at": {"type": "string", "format": "date-time", "description": "Only show from this time (also closes the hosted page before it)."},
            "end_at": {"type": "string", "format": "date-time", "description": "Only show until this time."}
        }), "When a popup or flyout opens; start_at/end_at schedule every kind.");
    let more = json!({
        "legal_text": text(1000, "Small print under the button; [label](https://url) becomes a link."),
        "success_button_text": text(60, "A button on the thank-you screen."),
        "success_button_url": url("Where that button goes; without it the button closes the form."),
        "redirect_url": url("Go here right after a sign-up instead of showing the thank-you screen."),
        "auto_close_seconds": int(0, 60, "Popup/flyout: close the thank-you screen after this many seconds; 0 keeps it open. Default 3 (0 with a discount code or success button)."),
        "confirm_message": text(300, "Shown instead of the thank-you screen when the list uses double opt-in."),
        "sms_consent_text": text(1000, "Shown next to the SMS consent checkbox under any phone field. Default: the standard carrier disclosure."),
        "brand_color": color("Default button color. Default: the account's brand color."),
        "delay_seconds": int(0, 600, "For targeting.trigger delay. Default 5."),
        "collect": {"type": ["array", "null"], "items": {"enum": ["email", "phone_number", "first_name"]}, "description": "Older way to pick fields; ignored when fields is set (it's kept in step automatically)."},
        "fields": {"type": ["array", "null"], "maxItems": 12, "items": field, "description": "Ordered list of what the form asks for. Must include email or phone_number."},
        "phone_step": nested(json!({"enabled": {"type": "boolean"}, "headline": {"type": "string", "maxLength": 200}, "body": {"type": "string", "maxLength": 1000}, "button_text": {"type": "string", "maxLength": 60}, "skip_text": {"type": "string", "maxLength": 60}}), "Second screen asking for a phone number after an email sign-up. Skipped when the form already has a phone field."),
        "style": style,
        "teaser": teaser,
        "targeting": targeting,
        "variants": {"type": ["array", "null"], "maxItems": 3, "items": {"type": "object", "additionalProperties": false, "required": ["id"], "properties": variant_props},
            "description": "A/B test: versions b, c, d in that order; the form itself is version A. Only per-version content keys. End a test with []."}
    });
    if let Value::Object(m) = more {
        props.extend(m);
    }
    json!({"type": "object", "additionalProperties": false, "properties": props,
        "description": "Form config. Every key is optional. On update_form each top-level key you send replaces the stored one (objects and lists whole), keys you leave out are kept and null resets a key to its default. Unknown keys are rejected. Full reference: the dumbmail://docs/forms resource."})
}

fn tools() -> Vec<Value> {
    let form_config = form_config_schema();
    let audience_schema = json!({"type": "object", "description": "Who receives it. include: lists/segments to send to (union); exclude: lists/segments to remove.",
        "properties": {
            "include": {"type": "array", "items": {"type": "object", "properties": {"type": {"enum": ["list", "segment"]}, "id": {"type": "string"}}, "required": ["type", "id"]}},
            "exclude": {"type": "array", "items": {"type": "object", "properties": {"type": {"enum": ["list", "segment"]}, "id": {"type": "string"}}, "required": ["type", "id"]}}
        }, "required": ["include"]});
    let segment_def = json!({"type": "object", "description": "Segment rules: {match: 'all'|'any', conditions: [...]}. Condition types: \
property {field, op, value}; \
event {metric, op: at_least|at_most|exactly|zero, count, within_days?, since?, until? (YYYY-MM-DD), where?:[{field, op, value}]}; \
event_value {metric, aggregate: sum|average|min|max, field? (default value = revenue), op: eq|neq|gt|gte|lt|lte|between, value, within_days?, since?, until?, where?}; \
event_time {metric, which: first|last, op: within_last_days|more_than_days_ago|before|after|between, value}; \
list {list_id, op: in|not_in}; segment {segment_id, op: in|not_in}; consent {channel: email|sms, status}; \
can_receive {channel: email|sms, value?: bool}; suppressed {value?: bool}; \
form {form_id? (any form if omitted), op?: has|has_not, within_days?, since?, until?}; \
engagement {channel: email|sms|any, engaged?: bool, within_days}; \
message {action: received|opened|clicked, campaign_id | flow_id, op?: has|has_not, within_days?}; \
prediction {measure: historic_clv|predicted_clv|order_count|average_order_value|days_since_last_order|average_days_between_orders|expected_next_order_in_days|churn_risk, op, value, metric? (default Placed Order)}; \
random {percent, seed?}; group {match, conditions}. \
Property ops: eq, neq, contains, not_contains, starts_with, ends_with, exists, not_exists, gt, gte, lt, lte, between [low, high], in, not_in, within_last_days, more_than_days_ago, anniversary_within_days. \
Read dumbmail://docs/segments for the details and examples.",
        "properties": {"match": {"enum": ["all", "any"]}, "conditions": {"type": "array", "items": {"type": "object"}}}, "required": ["conditions"]});
    let list_props = json!({
        "name": {"type": "string"},
        "double_opt_in": {"type": "boolean", "description": "New sign-ups confirm by email before they join."},
        "show_on_preferences": {"type": "boolean", "description": "List it on the preference page (from the unsubscribe link), where people can join or leave it themselves."},
        "description": {"type": "string", "description": "A line about the list for the preference page."}
    });
    let email_content = json!({"type": "object", "description": "Email: {subject, preheader?, from_name?, from_email?, reply_to?, blocks: [...]} or raw html. Block types: heading{text,level?,align?}, text{html}, image{src,alt?,href?}, button{text,href,color?}, divider, spacer{height?}, columns{columns:[[blocks],[blocks]]}, product{title,image?,price?,href,button_text?}, html{html}. SMS: {body, media_url?}. Jinja personalization: {{ first_name|default('there') }}, {{ person.properties.x }}, {{ event.properties.x }} in flows, {{ organization.name }}."});
    let profile_ref = json!({"type": "object", "description": "Who did it: email, phone_number (E.164) or external_id, plus optional first_name, last_name and custom properties.",
        "properties": {"email": {"type": "string"}, "phone_number": {"type": "string"}, "external_id": {"type": "string"}, "first_name": {"type": "string"}, "last_name": {"type": "string"}, "properties": {"type": "object"}}});
    let trigger = json!({"type": "object", "description": "Flow trigger. {type:'metric', metric:'Placed Order', filters?:[{field, op, value}]} | {type:'list', list_id} | {type:'segment', segment_id} | {type:'date_property', field:'birthday', offset_days?}"});
    let ab_test = json!({"type": ["object", "null"], "description": "A/B test. The campaign's own content is version A; variants are versions b, c, d in order, each with a full content object. A random test_percent (5-100, default 20) of the audience is split evenly across versions; after wait_hours (1-168, default 4) the best version by metric (open_rate (email only), click_rate (default) or revenue) goes to everyone else. pick: auto (default) or manual (call pick_campaign_winner). null removes the test.",
        "properties": {"variants": {"type": "array", "minItems": 1, "maxItems": 3, "items": {"type": "object", "properties": {"id": {"enum": ["b", "c", "d"]}, "content": {"type": "object"}}, "required": ["id", "content"]}},
            "test_percent": {"type": "integer", "minimum": 5, "maximum": 100}, "wait_hours": {"type": "integer", "minimum": 1, "maximum": 168},
            "metric": {"enum": ["open_rate", "click_rate", "revenue"]}, "pick": {"enum": ["auto", "manual"]}}, "required": ["variants"]});
    let flow_def = json!({"type": "object", "description": "Flow graph: {entry: firstNodeId, nodes: [...], settings?: {reentry: 'never'|'always'|{after_days}, exit_condition?: segment rules, profile_filter?: segment rules, smart_sending_hours?}}. Node types (each has a unique id): delay{amount, unit: minutes|hours|days, next}, email{name?, content, next}, sms{name?, content:{body}, next}, condition{condition: segment rules, yes, no}, random_split{percent: 1-99 share sent down yes (path A), yes, no} for A/B testing paths, update_profile{properties, next}, add_to_list|remove_from_list{list_id, next}, webhook{url, method?, body?, next}. next/yes/no are node ids or null to end. Read postlark://docs/flows."});
    vec![
        tool("get_account", "Get account overview", "Account settings, sending readiness (sender, domain, SMS number, mailing address), whether email sending is paused for high bounces/complaints (email_sending_paused; only support can lift it), plan usage, counts of profiles/lists/segments/campaigns/flows and the setup checklist. Call this first.", obj(json!({}), &[]), true, false),
        tool("update_account", "Update account settings", "Update brand and sender settings: name, website, from_name, from_email, reply_to, sms_from (E.164), timezone (IANA), address (mailing address shown in email footers, required to send), brand_color (hex), logo_url, social_links ({instagram|facebook|tiktok|x|youtube|pinterest|linkedin|threads: url}, blank removes), conversion_metric (metric used for revenue attribution, default 'Placed Order').",
            obj(json!({"name": {"type": "string"}, "website": {"type": "string"}, "from_name": {"type": "string"}, "from_email": {"type": "string"}, "reply_to": {"type": "string"}, "sms_from": {"type": "string"}, "timezone": {"type": "string"}, "address": {"type": "string"}, "brand_color": {"type": "string"}, "logo_url": {"type": "string"}, "social_links": {"type": "object", "additionalProperties": {"type": "string"}}, "conversion_metric": {"type": "string"}}), &[]), false, false),
        tool("import_brand", "Import brand from website", "Read the brand's website and return its name, logo URL, brand colors (best first), mailing address, a sender email on its domain and social profile links. With apply=true, saves the website, logo, brand color and social links, plus name and address if the account has none yet; sender email is never changed (it needs a verified domain). Show the user what was found.",
            obj(json!({"url": {"type": "string", "description": "e.g. acme.com"}, "apply": {"type": "boolean"}}), &["url"]), false, false),
        tool("set_sending_domain", "Add a sending domain", "Add a sending domain and start authenticating it (DKIM, return path, DMARC). An account can have several; emails can be sent from any address on a verified one. Returns the DNS records to add under sending.email.domains. Use check_sending_domain after DNS is updated.", obj(json!({"domain": {"type": "string", "description": "e.g. mail.yourbrand.com or yourbrand.com"}}), &["domain"]), false, false),
        tool("check_sending_domain", "Check sending domain DNS", "Re-check a sending domain's DNS records and verification status (all domains when domain is omitted).", obj(json!({"domain": {"type": "string"}}), &[]), false, false),
        tool("remove_sending_domain", "Remove a sending domain", "Remove a sending domain from the account. Without confirm=true it only describes what would happen.", obj(json!({"domain": {"type": "string"}, "confirm": {"type": "boolean"}}), &["domain"]), false, true),
        tool("set_link_domain", "Set a branded link domain", "Use the brand's own subdomain (e.g. links.brand.com) for tracking links, unsubscribe pages and hosted forms. Returns the DNS records to add.", obj(json!({"domain": {"type": "string"}}), &["domain"]), false, false),
        tool("check_link_domain", "Check link domain", "Re-check the branded link domain's DNS and certificate.", obj(json!({}), &[]), false, false),
        tool("get_billing", "Get balance and usage", "Prepaid balance, prices per email/SMS/number, free emails left this month, auto-recharge settings and recent charges.", obj(json!({}), &[]), true, false),
        tool("list_phone_numbers", "List phone numbers", "The account's texting numbers, their verification status and the SMS setup (platform or bring-your-own provider).", obj(json!({}), &[]), true, false),
        tool("find_phone_numbers", "Find numbers to claim", "Search toll-free (recommended) or local numbers that can be claimed.", obj(json!({"type": {"enum": ["toll_free", "local"]}, "area_code": {"type": "string"}}), &[]), true, false),
        tool("claim_phone_number", "Claim a phone number", "Claim a texting number (billed monthly from the balance). Toll-free numbers then need carrier verification in Settings → SMS. Requires confirm=true and the user's approval.", obj(json!({"phone_number": {"type": "string"}, "confirm": {"type": "boolean"}}), &["phone_number"]), false, true),
        tool("get_website_snippet", "Get the website snippet", "The one-line script for the brand's website (automatic shopping events, visitor recognition, sign-up forms), the Shopify checkout pixel, and whether it's installed.", obj(json!({}), &[]), true, false),
        tool("search_profiles", "Search profiles", "Find profiles by email, phone, name or external id, optionally within a list or segment. Returns up to `limit` (default 25) plus the total match count.",
            obj(json!({"query": {"type": "string"}, "list_id": {"type": "string"}, "segment_id": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
        tool("get_profile", "Get a profile", "Full profile: properties, consent, lists, segments, recent events (activity timeline) and messages received. Look up by id, email or phone_number.",
            obj(json!({"id": {"type": "string"}, "email": {"type": "string"}, "phone_number": {"type": "string"}}), &[]), true, false),
        tool("upsert_profile", "Create or update a profile", "Create a profile or update the one matching email/phone_number/external_id. Properties are merged. Set email_consent/sms_consent to 'subscribed' only when the person actually opted in (SMS requires explicit consent by law). Optional list_ids adds them to lists (which can trigger welcome flows).",
            obj(json!({"email": {"type": "string"}, "phone_number": {"type": "string"}, "external_id": {"type": "string"}, "first_name": {"type": "string"}, "last_name": {"type": "string"}, "properties": {"type": "object"}, "email_consent": {"enum": ["subscribed", "unsubscribed", "never_subscribed"]}, "sms_consent": {"enum": ["subscribed", "unsubscribed", "never_subscribed"]}, "list_ids": {"type": "array", "items": {"type": "string"}}}), &[]), false, false),
        tool("import_profiles", "Import profiles from CSV", "Import CSV text with a header row. Email, phone, first/last name and id columns are detected automatically; other columns become properties. Shared inboxes (info@, sales@) and throwaway addresses are skipped. Before importing, ask the user to confirm everyone in the file agreed to get email from them, and pass permission_confirmed=true only when they say so; bought, rented or scraped lists aren't allowed.",
            obj(json!({"csv": {"type": "string"}, "list_id": {"type": "string"}, "email_consent": {"enum": ["subscribed", "never_subscribed", "unsubscribed"]}, "sms_consent": {"enum": ["subscribed", "never_subscribed"]}, "permission_confirmed": {"type": "boolean", "description": "The user confirmed everyone in the file agreed to get email from them. Required unless email_consent is unsubscribed."}}), &["csv"]), false, false),
        tool("track_event", "Track an event", "Record something a person did (e.g. 'Placed Order' with value and items, 'Started Checkout', 'Viewed Product'). Creates the profile if needed and can trigger metric flows. Use unique_id to make it idempotent.",
            obj(json!({"metric": {"type": "string"}, "profile": profile_ref, "properties": {"type": "object"}, "value": {"type": "number", "description": "Revenue or numeric value"}, "time": {"type": "string", "format": "date-time"}, "unique_id": {"type": "string"}}), &["metric", "profile"]), false, false),
        tool("list_metrics", "List metrics", "All event metrics in the account with 30-day counts, the count for the 30 days before, a 30-day daily trend, whether events carry a value, and last-seen time.", obj(json!({}), &[]), true, false),
        tool("query_metric", "Query a metric over time", "Aggregate a metric by day/week/month in the account time zone. measurement: count | unique_profiles | sum_value (revenue). group_by: an event property key, or 'campaign'/'flow' for message events. Defaults to the last 30 days.",
            obj(json!({"metric": {"type": "string"}, "measurement": {"enum": ["count", "unique_profiles", "sum_value"]}, "interval": {"enum": ["day", "week", "month"]}, "start": {"type": "string", "format": "date-time"}, "end": {"type": "string", "format": "date-time"}, "group_by": {"type": "string"}}), &["metric"]), true, false),
        tool("get_events", "Get recent events", "Recent raw events, optionally filtered by metric name or profile.", obj(json!({"metric": {"type": "string"}, "profile_id": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}}), &[]), true, false),
        tool("list_lists", "List lists", "All lists with member counts.", obj(json!({}), &[]), true, false),
        tool("create_list", "Create a list", "Create a static list (e.g. 'Newsletter', 'VIP'). show_on_preferences lists it on the preference page people reach from the unsubscribe link, where they can join or leave it themselves instead of unsubscribing from all email.", obj(list_props.clone(), &["name"]), false, false),
        tool("update_list", "Update a list", "Rename a list or change double opt-in, whether it's on the preference page (show_on_preferences) and its description there. Only the fields given change.", obj(json!({"id": {"type": "string"}, "name": list_props["name"].clone(), "double_opt_in": list_props["double_opt_in"].clone(), "show_on_preferences": list_props["show_on_preferences"].clone(), "description": list_props["description"].clone()}), &["id"]), false, false),
        tool("add_to_list", "Add profiles to a list", "Add existing profiles (profile_ids) or emails (created if needed) to a list. List-triggered flows start for new members.",
            obj(json!({"list_id": {"type": "string"}, "profile_ids": {"type": "array", "items": {"type": "string"}}, "emails": {"type": "array", "items": {"type": "string"}}}), &["list_id"]), false, false),
        tool("remove_from_list", "Remove profiles from a list", "Remove profiles from a list.", obj(json!({"list_id": {"type": "string"}, "profile_ids": {"type": "array", "items": {"type": "string"}}}), &["list_id", "profile_ids"]), false, true),
        tool("list_segments", "List segments", "All segments with definitions and member counts.", obj(json!({}), &[]), true, false),
        tool("preview_segment", "Preview segment rules", "Count and sample the profiles matching segment rules WITHOUT saving. Use this to iterate on rules before create_segment.", obj(json!({"definition": segment_def}), &["definition"]), true, false),
        tool("create_segment", "Create a segment", "Save a live segment (membership updates automatically as data changes).", obj(json!({"name": {"type": "string"}, "definition": segment_def}), &["name", "definition"]), false, false),
        tool("update_segment", "Update a segment", "Rename a segment or replace its rules.", obj(json!({"id": {"type": "string"}, "name": {"type": "string"}, "definition": segment_def}), &["id"]), false, false),
        tool("list_templates", "List templates", "Saved email/SMS templates.", obj(json!({"channel": {"enum": ["email", "sms"]}}), &[]), true, false),
        tool("create_template", "Create a template", "Save reusable email or SMS content.", obj(json!({"name": {"type": "string"}, "channel": {"enum": ["email", "sms"]}, "content": email_content}), &["name", "content"]), false, false),
        tool("render_preview", "Preview a message", "Render email or SMS content as a real recipient would see it (optionally as a specific profile) to check personalization and wording. Returns subject, preheader and plain-text body (set include_html for the full HTML), or the SMS body with segment count.",
            obj(json!({"channel": {"enum": ["email", "sms"]}, "content": email_content, "profile_id": {"type": "string"}, "event": {"type": "object", "description": "Sample trigger event for flow content, e.g. {properties: {checkout_url: ...}}"}, "include_html": {"type": "boolean"}}), &["channel", "content"]), true, false),
        tool("list_campaigns", "List campaigns", "Campaigns with status and performance stats (recipients, open/click rates, revenue).", obj(json!({"status": {"enum": ["draft", "scheduled", "sending", "testing", "sent", "cancelled", "failed"]}, "channel": {"enum": ["email", "sms"]}}), &[]), true, false),
        tool("get_campaign", "Get a campaign", "One campaign with content, audience, stats and readiness problems.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
        tool("create_campaign", "Create a campaign draft", "Create a draft email or SMS campaign. Returns the draft plus estimated recipients and anything still missing before it can send. Nothing is sent.",
            obj(json!({"name": {"type": "string"}, "channel": {"enum": ["email", "sms"]}, "audience": audience_schema, "content": email_content, "ab_test": ab_test, "send_options": crate::features::sendtime::send_options_schema()}), &["channel"]), false, false),
        tool("update_campaign", "Update a campaign draft", "Change a draft or scheduled campaign's name, audience, content, A/B test or send options (content, ab_test and send_options replace the whole object; ab_test null removes the test; send_options null sends to everyone at once).",
            obj(json!({"id": {"type": "string"}, "name": {"type": "string"}, "audience": audience_schema, "content": email_content, "ab_test": ab_test, "send_options": crate::features::sendtime::send_options_schema()}), &["id"]), false, false),
        tool("send_test", "Send a test", "Send a test of a campaign to up to 10 email addresses or phone numbers. Tests are marked [Test] and don't count in reports. For an A/B test, version picks which version (a-d, default a).",
            obj(json!({"campaign_id": {"type": "string"}, "to": {"type": "array", "items": {"type": "string"}, "maxItems": 10}, "version": {"enum": ["a", "b", "c", "d"]}}), &["campaign_id", "to"]), false, false),
        tool("send_campaign", "Send or schedule a campaign", "Send a campaign now or schedule it (send_at in the future). With send_options timing local_time, the day of send_at (in the account's time zone, or today when omitted) is the day it goes out at local_time in each person's zone, starting when the first zone reaches it. send_options given here are saved on the campaign first. This messages real people and can't be undone once sent. Without confirm=true it returns a dry run with the recipient count, readiness problems and when it would go out (send_plan). Get the user's explicit approval before confirming.",
            obj(json!({"campaign_id": {"type": "string"}, "send_at": {"type": "string", "format": "date-time", "description": "Omit to send now"}, "send_options": crate::features::sendtime::send_options_schema(), "confirm": {"type": "boolean"}}), &["campaign_id"]), false, true),
        tool("cancel_campaign", "Cancel a campaign", "Unschedule a scheduled campaign (back to draft) or stop a campaign that is mid-send or waiting on its A/B test.", obj(json!({"campaign_id": {"type": "string"}}), &["campaign_id"]), false, true),
        tool("pick_campaign_winner", "Pick an A/B test winner", "For a campaign in status testing: send the chosen version (or, if version is omitted, the current leader) to the rest of the audience now. This messages real people; requires confirm=true and the user's approval. Check get_campaign_report's ab_test first.",
            obj(json!({"campaign_id": {"type": "string"}, "version": {"enum": ["a", "b", "c", "d"]}, "confirm": {"type": "boolean"}}), &["campaign_id"]), false, true),
        tool("get_campaign_report", "Campaign report", "Detailed results: delivered, bounces, unique opens/clicks, unsubscribes, attributed orders and revenue, top links, an hourly engagement series and, for A/B tests, per-version stats with the leader, confidence and winner.", obj(json!({"campaign_id": {"type": "string"}}), &["campaign_id"]), true, false),
        tool("campaign_performance", "Compare campaign performance", "Table of campaigns sent in the last `days` (default 90) with recipients, open rate, click rate, unsubscribes and revenue, sorted by `sort` (default revenue).",
            obj(json!({"days": {"type": "integer", "minimum": 1, "maximum": 365}, "channel": {"enum": ["email", "sms"]}, "sort": {"enum": ["revenue", "open_rate", "click_rate", "recipients", "sent_at"]}, "limit": {"type": "integer", "minimum": 1, "maximum": 100}}), &[]), true, false),
        tool("list_flows", "List flows", "All flows with status, trigger and performance.", obj(json!({}), &[]), true, false),
        tool("get_flow", "Get a flow", "One flow's trigger, full step graph, settings and per-step stats.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
        tool("list_flow_blueprints", "List flow blueprints", "Proven starter flows (welcome series, abandoned checkout, browse abandonment, post-purchase, winback, back in stock, price drop, birthday) with their full definitions.", obj(json!({}), &[]), true, false),
        tool("create_flow_from_blueprint", "Create a flow from a blueprint", "Create a draft flow from a blueprint key (creates the list/segment it needs). Edit its content with update_flow, then set_flow_status live.", obj(json!({"key": {"type": "string"}, "name": {"type": "string"}}), &["key"]), false, false),
        tool("create_flow", "Create a custom flow", "Create a draft flow from a trigger and a step graph. Validated for missing fields, dangling edges and loops. Nothing runs until it is set live.", obj(json!({"name": {"type": "string"}, "trigger": trigger, "definition": flow_def}), &["name", "trigger", "definition"]), false, false),
        tool("update_flow", "Update a flow", "Rename a flow or replace its trigger or definition (the whole graph). Live flows apply changes to people already waiting in them.", obj(json!({"id": {"type": "string"}, "name": {"type": "string"}, "trigger": trigger, "definition": flow_def}), &["id"]), false, false),
        tool("set_flow_status", "Turn a flow on, pause or draft", "live starts sending to people who trigger it; paused holds people in place; draft stops and exits everyone. Turning live requires confirm=true and the user's approval.", obj(json!({"id": {"type": "string"}, "status": {"enum": ["live", "paused", "draft"]}, "confirm": {"type": "boolean"}}), &["id", "status"]), false, true),
        tool("get_flow_report", "Flow report", "Flow totals plus per-step entered counts, opens, clicks and revenue.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
        tool("list_forms", "List sign-up forms", "Sign-up forms with views, submits, per-version A/B test stats (variant_stats), preview_url, hosted_url and install snippets.", obj(json!({}), &[]), true, false),
        tool("get_form", "Get a sign-up form", "One form: full config, stats, per-version A/B stats (variant_stats), preview_url (the hosted page showing drafts too, counting and saving nothing: open or share it to review before publishing), hosted_url and install snippets.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
        tool("create_form", "Create a sign-up form", "Create a popup, flyout, embed or hosted page form that adds sign-ups to a list. config covers content, fields (email, phone, names, birthday, custom questions), an optional phone step, style, teaser, targeting and schedule, and A/B versions; read the dumbmail://docs/forms resource for every key. Upload images with upload_image first. Starts as a draft unless status is live (publishing shows it to real visitors; check preview_url and get the user's go-ahead first).",
            obj(json!({"name": {"type": "string"}, "kind": {"enum": ["popup", "embed", "flyout", "page"], "description": "Default popup"}, "list_id": {"type": "string"}, "config": form_config.clone(), "status": {"enum": ["draft", "live"]}}), &["list_id"]), false, false),
        tool("update_form", "Update a sign-up form", "Change a form's name, kind, list, status (live publishes, draft unpublishes) or config. config merges one level deep: each top-level key sent replaces the stored one (style, targeting, teaser, phone_step, fields and variants are replaced whole, so send the complete object), keys left out are kept, null resets a key to its default. A/B test: set config.variants; end it with variants [] after copying the winning version's keys into the base config. See the dumbmail://docs/forms resource.",
            obj(json!({"id": {"type": "string"}, "name": {"type": "string"}, "kind": {"enum": ["popup", "embed", "flyout", "page"]}, "list_id": {"type": "string"}, "config": form_config, "status": {"enum": ["draft", "live"]}}), &["id"]), false, false),
        tool("upload_image", "Upload an image", "Store a PNG, JPEG, GIF or WebP image (up to 5 MB) and get a permanent public URL for a form's image_url or an email image block. Give url to copy an image from the web, or data_base64 with the file's contents. Uploading the same file again returns the existing image. Each account keeps up to 500 images.",
            obj(json!({"url": {"type": "string", "description": "Public http(s) URL of the image"}, "data_base64": {"type": "string", "description": "Base64 file contents (a data: URL works too)"}}), &[]), false, false),
        tool("list_images", "List uploaded images", "The account's uploaded images, newest first (up to 200), with their public URLs.", obj(json!({}), &[]), true, false),
        tool("get_analytics", "Analytics overview", "Dashboard for the last `days` (default 30): total and attributed revenue (campaigns vs flows) and orders, emails/SMS sent, open/click/unsubscribe/bounce/complaint rates, profile growth, the same totals for the previous `days` (`previous`) for comparison, a daily series and top campaigns and flows. With scope=flows: flow-only revenue, orders, share of store revenue, messages sent, recipients, open/click rates and revenue per recipient, with `previous` and daily flow revenue and sends. With scope=forms: sign-up form views, sign-ups and sign-up rate with `previous`, by day, and per form.", obj(json!({"days": {"type": "integer", "minimum": 1, "maximum": 365}, "scope": {"enum": ["overview", "flows", "forms"], "description": "Default overview."}}), &[]), true, false),
        tool("delete", "Delete something", "Delete a draft campaign, a non-live flow, a segment, a list, a template, a form, an uploaded image (anything still using its URL shows a broken image) or a profile. Permanent.", obj(json!({"type": {"enum": ["campaign", "flow", "segment", "list", "template", "form", "image", "profile"]}, "id": {"type": "string"}}), &["type", "id"]), false, true),
    ]
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().ok_or_else(|| AppError::bad(format!("`{k}` is required")))?.parse().map_err(|_| AppError::bad(format!("`{k}` must be an id")))
}

fn opt_uuid(a: &Value, k: &str) -> AppResult<Option<Uuid>> {
    match a[k].as_str() {
        Some(s) if !s.is_empty() => Ok(Some(s.parse().map_err(|_| AppError::bad(format!("`{k}` must be an id")))?)),
        _ => Ok(None),
    }
}

pub fn from<T: serde::de::DeserializeOwned>(v: Value) -> AppResult<T> {
    serde_json::from_value(v).map_err(|e| AppError::bad(format!("invalid arguments: {e}")))
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: Value) -> AppResult<Value> {
    let acct = ctx.account_id;
    match name {
        "get_account" => {
            let account = load_account(&state.db, acct).await?;
            let counts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM profiles WHERE account_id = $1), (SELECT count(*) FROM lists WHERE account_id = $1),
                        (SELECT count(*) FROM segments WHERE account_id = $1), (SELECT count(*) FROM campaigns WHERE account_id = $1),
                        (SELECT count(*) FROM flows WHERE account_id = $1), (SELECT count(*) FROM flows WHERE account_id = $1 AND status = 'live')",
            )
            .bind(acct)
            .fetch_one(&state.db)
            .await?;
            let mut missing = Vec::new();
            if account.from_email.is_none() { missing.push("sender email (update_account from_email)"); }
            if account.address.is_none() { missing.push("mailing address (update_account address; required in every marketing email)"); }
            if account.verified_domains.is_empty() { missing.push("verified sending domain (set_sending_domain) for best deliverability"); }
            if account.sms_from.is_none() { missing.push("texting number (find_phone_numbers + claim_phone_number, or connect your own provider in Settings → SMS)"); }
            let paused = account.sending_paused_at.map(|at| json!({
                "paused_at": at,
                "reason": account.sending_paused_reason,
                "note": crate::deliverability::paused_message(account.sending_paused_reason.as_deref()),
                "support_email": state.cfg.support_email,
            }));
            Ok(json!({
                "account": account,
                "email_sending_paused": paused,
                "sending": crate::api::account::sending_json(state, &account).await?,
                "billing": crate::api::account::billing_json(state, acct).await?,
                "counts": {"profiles": counts.0, "lists": counts.1, "segments": counts.2, "campaigns": counts.3, "flows": counts.4, "live_flows": counts.5},
                "missing_setup": missing,
            }))
        }
        "update_account" => Ok(serde_json::to_value(crate::api::account::patch_account(state, acct, from(a)?).await?)?),
        "import_brand" => crate::api::account::import_brand(state, acct, a["url"].as_str().unwrap_or(""), a["apply"].as_bool().unwrap_or(false)).await,
        "set_sending_domain" => crate::api::account::set_domain(state, acct, a["domain"].as_str().unwrap_or("")).await,
        "check_sending_domain" => {
            match a["domain"].as_str().filter(|d| !d.trim().is_empty()) {
                Some(d) => crate::domains::verify_sending_domain(state, acct, d).await?,
                None => crate::domains::verify_sending_domains(state, acct, true).await?,
            }
            crate::api::account::sending_json(state, &load_account(&state.db, acct).await?).await
        }
        "remove_sending_domain" => {
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "note": "Nothing removed. Emails from addresses on this domain stop sending until it's added and verified again. Call again with confirm=true once the user approves."}));
            }
            crate::domains::remove_sending_domain(state, acct, a["domain"].as_str().unwrap_or("")).await?;
            crate::api::account::sending_json(state, &load_account(&state.db, acct).await?).await
        }
        "set_link_domain" => {
            crate::domains::set_link_domain(state, acct, a["domain"].as_str().unwrap_or("")).await?;
            crate::api::account::sending_json(state, &load_account(&state.db, acct).await?).await
        }
        "check_link_domain" => {
            crate::domains::verify_link_domain(state, acct).await?;
            crate::api::account::sending_json(state, &load_account(&state.db, acct).await?).await
        }
        "get_billing" => crate::billing::billing_json(state, acct).await,
        "list_phone_numbers" => crate::api::account::numbers_json(state, acct).await,
        "find_phone_numbers" => crate::phone::available(state, a["type"].as_str().unwrap_or("toll_free"), a["area_code"].as_str()).await,
        "claim_phone_number" => {
            if a["confirm"].as_bool() != Some(true) {
                let price = crate::billing::to_usd(crate::billing::prices(state).number_monthly);
                return Ok(json!({"dry_run": true, "note": format!("Nothing claimed. This number costs ${price:.2}/month from the account balance. Call again with confirm=true once the user approves.")}));
            }
            Ok(crate::phone::row_json(&crate::phone::claim(state, acct, a["phone_number"].as_str().unwrap_or("")).await?))
        }
        "get_website_snippet" => {
            let (pk, seen, url): (String, Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
                sqlx::query_as("SELECT public_key, onsite_last_seen_at, onsite_last_url FROM accounts WHERE id = $1").bind(acct).fetch_one(&state.db).await?;
            Ok(json!({"snippet": crate::api::account::snippet(state, &pk), "installed": seen.is_some(), "last_seen_at": seen, "last_seen_url": url,
                "shopify_checkout_pixel": crate::public::shopify_pixel(state, &pk),
                "usage": "Paste the snippet in the site's <head> or footer on every page. It tracks Active on Site, Viewed Product, Added to Cart, Started Checkout and Placed Order by itself on Shopify, WooCommerce and any site with product structured data (JSON-LD) or Google Analytics ecommerce events, and recognizes visitors who give their email at checkout or in sign-up, login or contact forms. On Shopify, also add shopify_checkout_pixel (the only source of Started Checkout and Placed Order there) under Settings → Customer events → Add custom pixel (checkout pages can't run theme scripts). Custom sites can call dumbmail.identify({email}), dumbmail.viewedProduct({name, price, image_url, url}), dumbmail.addedToCart(item), dumbmail.startedCheckout({items, total}), dumbmail.placedOrder({order_id, total, items}) or dumbmail.track(metric, properties, value). Embedded forms: <div data-dumbmail-form=\"FORM_ID\"></div>. To check a live site, use Settings → Website → Test your setup in the dashboard."}))
        }
        "search_profiles" => {
            let q = aud::ProfileQuery {
                q: a["query"].as_str().map(String::from),
                list_id: opt_uuid(&a, "list_id")?,
                segment_id: opt_uuid(&a, "segment_id")?,
                limit: Some(a["limit"].as_i64().unwrap_or(25)),
                offset: a["offset"].as_i64(),
            };
            let (rows, total) = aud::list_profiles(state, acct, &q).await?;
            Ok(json!({"total": total, "profiles": rows}))
        }
        "get_profile" => {
            let id = match opt_uuid(&a, "id")? {
                Some(id) => id,
                None => {
                    let input = crate::audience::ProfileInput { email: a["email"].as_str().map(String::from), phone_number: a["phone_number"].as_str().map(String::from), ..Default::default() };
                    if input.email.is_none() && input.phone_number.is_none() {
                        return Err(AppError::bad("give id, email or phone_number"));
                    }
                    crate::audience::find_profile(&state.db, acct, &input).await?.ok_or_else(|| AppError::not_found("profile"))?.id
                }
            };
            aud::profile_detail(state, acct, id).await
        }
        "upsert_profile" => Ok(serde_json::to_value(aud::create_profile(state, acct, a).await?)?),
        "import_profiles" => aud::import(state, acct, from(a)?).await,
        "track_event" => aud::track(state, acct, from(a)?).await,
        "list_metrics" => aud::metrics(state, acct).await,
        "query_metric" => aud::query_metric(state, acct, from(a)?).await,
        "get_events" => {
            let q = aud::EventsQuery { metric: a["metric"].as_str().map(String::from), profile_id: opt_uuid(&a, "profile_id")?, limit: Some(a["limit"].as_i64().unwrap_or(50)), offset: None };
            let (rows, total) = aud::events(state, acct, &q).await?;
            Ok(json!({"total": total, "events": rows}))
        }
        "list_lists" => Ok(json!({"lists": aud::lists(state, acct).await?})),
        "create_list" => Ok(serde_json::to_value(aud::create_list(state, acct, from(a)?).await?)?),
        "update_list" => {
            let id = uuid_arg(&a, "id")?;
            let mut body = a.clone();
            if let Some(o) = body.as_object_mut() {
                o.remove("id");
            }
            Ok(serde_json::to_value(aud::update_list(state, acct, id, from(body)?).await?)?)
        }
        "add_to_list" => {
            let list_id = uuid_arg(&a, "list_id")?;
            let added = aud::add_members(state, acct, list_id, from(json!({"profile_ids": a["profile_ids"].clone().as_array().cloned().unwrap_or_default(), "emails": a["emails"].as_array().cloned().unwrap_or_default()}))?).await?;
            Ok(json!({"added": added}))
        }
        "remove_from_list" => {
            let list_id = uuid_arg(&a, "list_id")?;
            crate::audience::list_belongs(&state.db, acct, list_id).await?;
            let ids: Vec<Uuid> = from(a["profile_ids"].clone())?;
            let n = sqlx::query("DELETE FROM list_members WHERE list_id = $1 AND profile_id = ANY($2)").bind(list_id).bind(&ids).execute(&state.db).await?.rows_affected();
            Ok(json!({"removed": n}))
        }
        "list_segments" => Ok(json!({"segments": aud::segments_list(state, acct).await?})),
        "preview_segment" => aud::preview(state, acct, &a["definition"]).await,
        "create_segment" => Ok(serde_json::to_value(aud::create_segment(state, acct, from(a)?).await?)?),
        "update_segment" => {
            let id = uuid_arg(&a, "id")?;
            Ok(serde_json::to_value(aud::update_segment(state, acct, id, aud::SegmentBody { name: a["name"].as_str().map(String::from), definition: a.get("definition").cloned() }).await?)?)
        }
        "list_templates" => {
            let rows: Vec<crate::models::Template> = sqlx::query_as("SELECT id, name, channel, content, created_at, updated_at FROM templates WHERE account_id = $1 AND ($2::text IS NULL OR channel = $2) ORDER BY updated_at DESC")
                .bind(acct)
                .bind(a["channel"].as_str())
                .fetch_all(&state.db)
                .await?;
            Ok(json!({"templates": rows}))
        }
        "create_template" => Ok(serde_json::to_value(msg::create_template(state, acct, from(a)?).await?)?),
        "render_preview" => {
            let include_html = a["include_html"].as_bool().unwrap_or(false);
            let mut r = msg::render_preview(state, acct, msg::RenderBody { channel: a["channel"].as_str().unwrap_or("email").into(), content: a["content"].clone(), profile_id: opt_uuid(&a, "profile_id")?, event: a.get("event").cloned() }).await?;
            if !include_html {
                if let Some(o) = r.as_object_mut() {
                    if let Some(h) = o.remove("html") {
                        o.insert("html_bytes".into(), json!(h.as_str().map(str::len).unwrap_or(0)));
                    }
                }
            }
            Ok(r)
        }
        "list_campaigns" => {
            let rows = msg::list_campaigns(state, acct, &msg::ChannelQuery { channel: a["channel"].as_str().map(String::from), status: a["status"].as_str().map(String::from) }).await?;
            Ok(json!({"campaigns": rows}))
        }
        "get_campaign" => campaign_with_readiness(state, acct, uuid_arg(&a, "id")?).await,
        "create_campaign" => {
            let c = msg::create_campaign(state, acct, from(a)?).await?;
            campaign_with_readiness(state, acct, c.id).await
        }
        "update_campaign" => {
            let id = uuid_arg(&a, "id")?;
            msg::update_campaign(state, acct, id, msg::CampaignBody { name: a["name"].as_str().map(String::from), channel: None, audience: a.get("audience").cloned(), content: a.get("content").cloned(), send_at: None, ab_test: a.get("ab_test").cloned(), send_options: a.get("send_options").cloned() }).await?;
            campaign_with_readiness(state, acct, id).await
        }
        "send_test" => {
            let c = campaigns::load(&state.db, acct, uuid_arg(&a, "campaign_id")?).await?;
            let account = load_account(&state.db, acct).await?;
            let to: Vec<String> = from(a["to"].clone())?;
            Ok(json!({"sent": campaigns::send_test(state, &account, &c, &to, a["version"].as_str()).await?}))
        }
        "send_campaign" => {
            let id = uuid_arg(&a, "campaign_id")?;
            let send_at: Option<DateTime<Utc>> = a["send_at"].as_str().map(|s| s.parse().map_err(|_| AppError::bad("send_at must be an ISO 8601 date-time"))).transpose()?;
            if let Some(opts) = a.get("send_options") {
                crate::features::sendtime::checked(Some(opts))?;
            }
            if a["confirm"].as_bool() != Some(true) {
                let mut dry = campaign_with_readiness(state, acct, id).await?;
                let c = campaigns::load(&state.db, acct, id).await?;
                let mut opts = match a.get("send_options") {
                    Some(v) => crate::features::sendtime::options::parse(v)?,
                    None => crate::features::sendtime::options::of(c.send_options.as_ref()),
                };
                if let Some(o) = opts.as_mut() {
                    o.local_date = None;
                }
                let audience: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
                if !audience.include.is_empty() {
                    dry["send_plan"] = crate::features::sendtime::plan::preview(state, acct, &c.channel, &audience, opts, send_at).await?;
                }
                dry["dry_run"] = json!(true);
                dry["note"] = json!(format!("Nothing sent. This would {} to {} recipients. Call again with confirm=true once the user approves.",
                    if send_at.is_some() { "schedule the campaign" } else { "send now" }, dry["estimated_recipients"]));
                return Ok(dry);
            }
            let c = msg::launch(state, acct, id, send_at, a.get("send_options")).await?;
            msg::campaign_json(state, &c).await
        }
        "cancel_campaign" => {
            let c = msg::cancel(state, acct, uuid_arg(&a, "campaign_id")?).await?;
            msg::campaign_json(state, &c).await
        }
        "pick_campaign_winner" => {
            let c = campaigns::load(&state.db, acct, uuid_arg(&a, "campaign_id")?).await?;
            if a["confirm"].as_bool() != Some(true) {
                let report = msg::report(state, acct, c.id).await?;
                return Ok(json!({"dry_run": true, "ab_test": report["ab_test"],
                    "note": "Nothing sent. Call again with confirm=true once the user approves sending this version to the rest of the audience."}));
            }
            let c = campaigns::pick_winner(state, &c, a["version"].as_str()).await?;
            msg::campaign_json(state, &c).await
        }
        "get_campaign_report" => msg::report(state, acct, uuid_arg(&a, "campaign_id")?).await,
        "campaign_performance" => {
            let days = a["days"].as_i64().unwrap_or(90).clamp(1, 365);
            let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM campaigns WHERE account_id = $1 AND status IN ('sent','sending','testing') AND sent_at > now() - make_interval(days => $2) AND ($3::text IS NULL OR channel = $3) ORDER BY sent_at DESC LIMIT 200")
                .bind(acct)
                .bind(days as i32)
                .bind(a["channel"].as_str())
                .fetch_all(&state.db)
                .await?;
            let mut out = Vec::new();
            for (id,) in rows {
                let c = campaigns::load(&state.db, acct, id).await?;
                let s = campaigns::stats(&state.db, acct, id).await?;
                out.push(json!({"id": id, "name": c.name, "channel": c.channel, "sent_at": c.sent_at, "subject": c.content.get("subject"),
                    "recipients": s.recipients, "delivered": s.delivered, "open_rate": s.open_rate, "click_rate": s.click_rate,
                    "unsubscribes": s.unsubscribes, "orders": s.orders, "revenue": s.revenue,
                    "revenue_per_recipient": if s.recipients > 0 { s.revenue / s.recipients as f64 } else { 0.0 }}));
            }
            let key = a["sort"].as_str().unwrap_or("revenue").to_string();
            if key != "sent_at" {
                out.sort_by(|x, y| y[&key].as_f64().partial_cmp(&x[&key].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
            }
            out.truncate(a["limit"].as_u64().unwrap_or(25) as usize);
            Ok(json!({"days": days, "campaigns": out}))
        }
        "list_flows" => Ok(json!({"flows": msg::list_flows(state, acct).await?})),
        "get_flow" => {
            let id = uuid_arg(&a, "id")?;
            let f = msg::load_flow(state, acct, id).await?;
            let mut v = msg::flow_json(state, &f).await?;
            v["steps"] = msg::flow_report(state, acct, id).await?["nodes"].clone();
            Ok(v)
        }
        "list_flow_blueprints" => msg::blueprints_json(state, acct).await,
        "create_flow_from_blueprint" => {
            let f = crate::flows::from_blueprint(&state.db, acct, a["key"].as_str().unwrap_or(""), a["name"].as_str().map(String::from)).await?;
            msg::flow_json(state, &f).await
        }
        "create_flow" => {
            let f = msg::create_flow(state, acct, from(a)?).await?;
            msg::flow_json(state, &f).await
        }
        "update_flow" => {
            let id = uuid_arg(&a, "id")?;
            let f = msg::update_flow(state, acct, id, msg::FlowBody { name: a["name"].as_str().map(String::from), trigger: a.get("trigger").cloned(), definition: a.get("definition").cloned() }).await?;
            msg::flow_json(state, &f).await
        }
        "set_flow_status" => {
            let id = uuid_arg(&a, "id")?;
            let status = a["status"].as_str().unwrap_or("");
            if status == "live" && a["confirm"].as_bool() != Some(true) {
                let f = msg::load_flow(state, acct, id).await?;
                return Ok(json!({"dry_run": true, "flow": {"id": f.id, "name": f.name, "trigger": f.trigger, "status": f.status},
                    "note": "Nothing changed. Turning this flow live sends real messages to everyone who triggers it from now on. Call again with confirm=true once the user approves."}));
            }
            let f = msg::set_flow_status(state, acct, id, status).await?;
            msg::flow_json(state, &f).await
        }
        "get_flow_report" => msg::flow_report(state, acct, uuid_arg(&a, "id")?).await,
        "list_forms" => {
            let rows: Vec<crate::models::FormRow> = sqlx::query_as("SELECT * FROM forms WHERE account_id = $1 ORDER BY updated_at DESC").bind(acct).fetch_all(&state.db).await?;
            let forms = msg::forms_json(state, &rows).await?;
            Ok(json!({"forms": rows.iter().zip(forms).map(|(f, form)| form_detail(state, f, form)).collect::<Vec<_>>()}))
        }
        "get_form" => {
            let f = msg::load_form(state, acct, uuid_arg(&a, "id")?).await?;
            let form = msg::form_one(state, &f).await?;
            Ok(form_detail(state, &f, form))
        }
        "create_form" => {
            let f = msg::create_form(state, acct, from(json!({"name": a["name"], "kind": a["kind"], "list_id": a["list_id"], "config": a["config"], "status": a["status"]}))?).await?;
            let form = msg::form_one(state, &f).await?;
            Ok(form_detail(state, &f, form))
        }
        "update_form" => {
            let id = uuid_arg(&a, "id")?;
            let f = msg::update_form(state, acct, id, from(json!({"name": a["name"], "kind": a["kind"], "list_id": a["list_id"], "config": a["config"], "status": a["status"]}))?).await?;
            let form = msg::form_one(state, &f).await?;
            Ok(form_detail(state, &f, form))
        }
        "upload_image" => crate::api::images::store_json(state, acct, &a).await,
        "list_images" => {
            let (images, total) = crate::api::images::list(state, acct).await?;
            Ok(json!({"images": images, "total": total}))
        }
        "get_analytics" => match a["scope"].as_str() {
            Some("flows") => analytics::flows(state, acct, a["days"].as_i64().unwrap_or(30)).await,
            Some("forms") => analytics::forms(state, acct, a["days"].as_i64().unwrap_or(30)).await,
            _ => analytics::overview(state, acct, a["days"].as_i64().unwrap_or(30)).await,
        },
        "delete" => delete(state, acct, a["type"].as_str().unwrap_or(""), uuid_arg(&a, "id")?).await,
        other => Err(AppError::bad(format!("unknown tool {other:?}"))),
    }
}

/// A form with its links and how to put it on a site.
fn form_detail(state: &AppState, f: &crate::models::FormRow, form: Value) -> Value {
    let base = state.cfg.public_url();
    let install = match f.kind.as_str() {
        "embed" => format!("<div data-dumbmail-form=\"{}\"></div> where it should appear (with the website snippet from get_website_snippet), or <script async src=\"{base}/f/{}/embed.js\"></script> on its own", f.id, f.id),
        "page" => "Share hosted_url, or link to it from the site, emails or social profiles.".to_string(),
        _ => format!("Shows automatically on sites with the website snippet (get_website_snippet), using the form's targeting rules. Any element with data-dumbmail-open=\"{}\" opens it on click.", f.id),
    };
    json!({"form": form, "hosted_url": format!("{base}/f/{}", f.id), "preview_url": msg::form_preview_url(state, f.id), "status": f.status, "install": install})
}

async fn campaign_with_readiness(state: &AppState, acct: Uuid, id: Uuid) -> AppResult<Value> {
    let c = campaigns::load(&state.db, acct, id).await?;
    let account = load_account(&state.db, acct).await?;
    let mut v = msg::campaign_json(state, &c).await?;
    let audience: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
    let est = if audience.include.is_empty() { 0 } else { campaigns::estimate(&state.db, acct, &c.channel, &audience).await? };
    v["estimated_recipients"] = json!(est);
    v["ready_to_send"] = json!(campaigns::readiness(&account, &c, state.email.name() == "ses").is_empty() && est > 0);
    v["problems"] = json!(campaigns::readiness(&account, &c, state.email.name() == "ses"));
    Ok(v)
}

async fn delete(state: &AppState, acct: Uuid, kind: &str, id: Uuid) -> AppResult<Value> {
    let table = match kind {
        "campaign" => {
            let c = campaigns::load(&state.db, acct, id).await?;
            if matches!(c.status.as_str(), "sending" | "testing" | "sent") {
                return Err(AppError::Conflict("sent campaigns are kept for reporting".into()));
            }
            "campaigns"
        }
        "flow" => {
            if msg::load_flow(state, acct, id).await?.status == "live" {
                return Err(AppError::Conflict("turn the flow off first".into()));
            }
            "flows"
        }
        "segment" => "segments",
        "list" => "lists",
        "template" => "templates",
        "form" => "forms",
        "image" => "images",
        "profile" => "profiles",
        _ => return Err(AppError::bad("type must be campaign, flow, segment, list, template, form, image or profile")),
    };
    let n = sqlx::query(&format!("DELETE FROM {table} WHERE id = $1 AND account_id = $2")).bind(id).bind(acct).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found(kind));
    }
    Ok(json!({"deleted": true, "type": kind, "id": id}))
}

fn prompts() -> Value {
    json!([
        {"name": "plan_campaign", "title": "Plan and build a campaign", "description": "Research the audience and past results, then draft, preview and test a campaign for approval.",
         "arguments": [{"name": "goal", "description": "What the campaign should achieve, e.g. 'launch our fall collection'", "required": true}]},
        {"name": "audit_account", "title": "Audit my marketing", "description": "Review setup, flows, deliverability and performance and recommend the highest-impact fixes.", "arguments": []},
        {"name": "build_core_flows", "title": "Set up core flows", "description": "Create and customize the essential automations (welcome, abandoned checkout, post-purchase, winback) for this brand.",
         "arguments": [{"name": "brand_voice", "description": "Tone of voice, e.g. 'warm and playful'", "required": false}]}
    ])
}

fn prompt(name: &str, args: &Value) -> Option<Value> {
    let text = match name {
        "plan_campaign" => format!(
            "Goal: {}.\n1. get_account and fix anything blocking sending.\n2. campaign_performance (days 180) to learn what subject lines, timing and audiences worked.\n3. list_segments / preview_segment to pick the best audience; create a segment if needed.\n4. create_campaign with on-brand blocks and a strong subject + preheader; render_preview it.\n5. send_test to the user, summarize the plan (audience size, send time, content) and ask for approval before send_campaign with confirm=true.",
            args["goal"].as_str().unwrap_or("(ask the user)")
        ),
        "audit_account" => "Audit this DumbMail account: get_account (setup gaps), list_flows + get_flow_report (missing core flows, weak steps), get_analytics days=90 (revenue split, open/click/unsubscribe/bounce rates), campaign_performance (best and worst). Give the top 5 fixes ranked by expected revenue impact, each with the exact tool calls to make it.".to_string(),
        "build_core_flows" => format!(
            "Brand voice: {}. list_flow_blueprints, then for welcome_series, abandoned_checkout, post_purchase and winback: create_flow_from_blueprint, rewrite every email/SMS in the brand voice with update_flow, render_preview each step, and show the user a summary. Only set_flow_status live with confirm=true after they approve.",
            args["brand_voice"].as_str().unwrap_or("match the brand's website")
        ),
        _ => return None,
    };
    Some(json!({"description": name, "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
}

const FORMS_DOC: &str = include_str!("../docs/FORMS.md");

const SEGMENT_DOC: &str = r#"# Segment rules

A definition is `{"match": "all" | "any", "conditions": [...]}`. Groups nest up to 4 levels, with at most 50 conditions per group. The same rules drive segments, campaign audiences and flow conditions, filters and exit conditions. Always try rules with `preview_segment` before saving.

```json
{"match": "all", "conditions": [
  {"type": "event_value", "metric": "Placed Order", "aggregate": "sum", "op": "gte", "value": 500},
  {"type": "event_time", "metric": "Placed Order", "which": "last", "op": "more_than_days_ago", "value": 60},
  {"type": "can_receive", "channel": "email"},
  {"type": "segment", "segment_id": "...", "op": "not_in"},
  {"type": "group", "match": "any", "conditions": [
    {"type": "property", "field": "tags", "op": "contains", "value": "vip"},
    {"type": "prediction", "measure": "churn_risk", "op": "in", "value": ["medium", "high"]}
  ]}
]}
```

## Profile properties

`{"type": "property", "field": "...", "op": "...", "value": ...}`

- Built-in fields: `email`, `phone_number`, `first_name`, `last_name`, `external_id`, `created_at`, `updated_at`, `last_event_at`, `email_consent`, `sms_consent`, `email_suppressed_reason`. Anything else is a custom property.
- Location uses the standard properties `country` (ISO code like `US`), `region`, `city`, `zip` and `timezone` (IANA, e.g. `America/Chicago`). Form sign-ups fill `timezone` and `country` from the visitor's browser when they're missing. Set the rest from imports, the API or your store.
- Ops: `eq`, `neq` (case-insensitive), `contains` / `not_contains` (text or an array element such as tags), `starts_with`, `ends_with`, `exists`, `not_exists`, `gt` / `gte` / `lt` / `lte` (numbers or dates), `between` (`[low, high]`, inclusive; `YYYY-MM-DD` dates are whole days in the account's time zone), `in` / `not_in` (arrays), `within_last_days` / `more_than_days_ago` (dates), `anniversary_within_days` (month and day fall in the next N days, 0 = today, e.g. birthdays).

## What someone has done

- **Count:** `{"type": "event", "metric": "Placed Order", "op": "at_least" | "at_most" | "exactly" | "zero", "count": 2}`.
- **Totals:** `{"type": "event_value", "metric": "Placed Order", "aggregate": "sum" | "average" | "min" | "max", "field": "value", "op": "gt", "value": 500}`. `field` defaults to `value` (the event's revenue) and can be a numeric event property. Ops: `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `between` `[low, high]`. With no events, a sum is 0 and the others match nothing.
- **First or last time:** `{"type": "event_time", "metric": "Placed Order", "which": "first" | "last", "op": "within_last_days" | "more_than_days_ago" | "before" | "after" | "between", "value": 30 | "2026-11-01" | ["2026-11-01", "2026-11-30"]}`. People who never did it don't match.
- **Time window** (event and event_value): `within_days`, and/or `since` and `until` as `YYYY-MM-DD` (both days included, in the account's time zone).
- **Event filters:** `"where": [{"field": "value" | "<event property>", "op": "...", "value": ...}]`. Email and SMS events carry `campaign_id`, `flow_id` and `message_id`. "Subscribed to List" carries `form_id`, `form`, `list_id` and `form_version`.
- Built-in metrics: Placed Order, Started Checkout, Added to Cart, Viewed Product, Active on Site, Subscribed to List, Received Email, Opened Email, Clicked Email, Bounced Email, Marked Email as Spam, Unsubscribed, Received SMS, Clicked SMS, Received SMS Reply, Unsubscribed from SMS. Custom metrics work the same way (`list_metrics`).

## Shortcuts

- `{"type": "engagement", "channel": "email" | "sms" | "any", "engaged": true, "within_days": 90}`. Email counts opens and clicks (Apple Mail Privacy Protection can inflate opens). SMS counts clicks and replies. `any` adds site activity, product views, carts, checkouts, orders and sign-ups. `engaged: false` finds people with none of these.
- `{"type": "message", "action": "received" | "opened" | "clicked", "campaign_id": "..." | "flow_id": "...", "op": "has" | "has_not", "within_days": 30}`.
- `{"type": "form", "form_id": "...", "op": "has" | "has_not", "within_days": 30}`: signed up through a form (any form when `form_id` is left out). `since` and `until` work too.
- `{"type": "can_receive", "channel": "email" | "sms", "value": true}`: a send would reach them now. Email needs an address, consent not unsubscribed or pending, and no bounce or complaint suppression. SMS needs a phone and subscribed consent.
- `{"type": "suppressed", "value": true}`: email suppressed after a hard bounce or spam complaint.
- `{"type": "consent", "channel": "email" | "sms", "status": "subscribed" | "unsubscribed" | "never_subscribed" | "pending"}`.

## Lists and other segments

- `{"type": "list", "list_id": "...", "op": "in" | "not_in"}`.
- `{"type": "segment", "segment_id": "...", "op": "in" | "not_in"}` uses the other segment's current rules, live. A segment can't include itself, even through another segment. A segment can't be deleted while another segment or a running flow uses it. One set of rules can use other segments at most 50 times and expand to at most 1,000 rules in total, counting the rules inside them.

## Predictions

`{"type": "prediction", "measure": "...", "op": "...", "value": ..., "metric": "Placed Order"}` is computed from the order metric's events (`metric`, default Placed Order) and their values.

- `historic_clv`: revenue so far.
- `predicted_clv`: revenue so far, plus average order value × expected orders over the next 365 days. Expected orders are 365 ÷ the average days between orders, at most 52. One-time buyers and customers at high churn risk get nothing extra.
- `order_count`, `average_order_value`, `days_since_last_order`.
- `average_days_between_orders` and `expected_next_order_in_days` (negative when overdue) need 2+ orders.
- `churn_risk`: `low`, `medium` or `high`. It compares days since the last order with the usual gap: the average gap with 2+ orders, else 90 days. Up to 1× the gap is low, up to 2× medium, beyond that high. Compare with `eq`, `neq` or `in` (`["medium", "high"]`).

Numeric measures use `eq`, `neq`, `gt`, `gte`, `lt`, `lte` and `between`. People with no orders match only where a figure exists for them: `historic_clv` and `predicted_clv` are 0 and `order_count` is 0.

## Products

`{"type": "product", "action": "ordered" | "viewed" | "carted" | "checkout", "product_id": "TEE-1", "category": "Shirts", "tag": "organic", "brand": "Acme", "op": "at_least" | "at_most" | "exactly" | "zero", "count": 1, "within_days": 90}` counts the events in which someone did that with one product (the store's id) or any product in a category, with a tag or of a brand. Leave all four out for any product. `since` and `until` work too. It reads the product catalog (dumbmail://docs/catalog), so products a store sells but the catalog doesn't know yet don't count. To match on a property the event carries itself, use an `event` rule with `where`, e.g. Viewed Product where `product_id` eq `TEE-1`.

## Random sample

`{"type": "random", "percent": 10, "seed": "holdout-nov"}` keeps a stable share of profiles: the same seed always picks the same people. Use a seed per test, and pair it with `"op": "not_in"` on a saved segment (or a group) for the rest.
"#;

const EMAIL_DOC: &str = r#"# Email content

```json
{"subject": "New arrivals for you, {{ first_name|default('friend') }}",
 "preheader": "Fresh picks inside",
 "blocks": [
  {"type": "image", "src": "https://.../hero.jpg", "alt": "Fall collection", "href": "https://shop.example.com/fall"},
  {"type": "heading", "text": "Fall is here", "align": "center"},
  {"type": "text", "html": "<p>Hi {{ first_name|default('there') }}, our new collection just dropped.</p>"},
  {"type": "button", "text": "Shop the collection", "href": "https://shop.example.com/fall", "align": "center"},
  {"type": "columns", "columns": [
    [{"type": "product", "title": "Wool scarf", "image": "https://...", "price": "$48", "href": "https://..."}],
    [{"type": "product", "title": "Rain jacket", "image": "https://...", "price": "$148", "href": "https://..."}]
  ]},
  {"type": "divider"},
  {"type": "text", "html": "<p>Questions? Just reply to this email.</p>"}
 ]}
```

Products from the catalog (dumbmail://docs/catalog): `{"type": "products", "source": "handpicked" | "collection" | "best_sellers" | "newest" | "on_sale" | "event_items" | "recently_viewed" | "recommended", "count": 4, "columns": 2, "fallback": "best_sellers"}` is filled for each person when the email sends: `event_items` shows the triggering event's cart, viewed product or order; `recommended` picks for the person. Options: `product_ids` (handpicked), `category` / `tag` / `brand` (collection), `days` (best_sellers), `show_price`, `show_compare_at`, `show_button`, `button_text`, `hide_out_of_stock`. Templates can also use `{{ organization.catalog.products["TEE-1"].title }}`, `organization.catalog.best_sellers`, `.newest`, `.on_sale` and `person.recommendations` (lists of products with `title`, `url`, `image_url`, `price_text`, ...). The short names `catalog` and `recommendations` work too unless a profile on the account has a property with that name (then the property keeps it).

More blocks (full reference: dumbmail://docs/content):

```json
{"type": "social", "links": [{"network": "instagram", "url": "https://..."}], "style": "color" | "dark" | "light" | "white", "size": 24 | 32 | 40, "align": "center"}
{"type": "video", "url": "https://youtu.be/...", "thumbnail": "https://...", "alt": "Watch the launch", "caption": "1 minute"}
{"type": "countdown", "ends_at": "2026-12-01T08:00:00-08:00", "ended_text": "This sale has ended"}
{"type": "table", "columns": [{"label": "Item"}, {"label": "Price", "align": "right"}], "rows": [["Gift wrap", "$0"]],
 "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}", "{{ item.price }}"]}, "footer": [["Total", "{{ event.value }}"]]}
{"type": "quote", "text": "Love it.", "author": "Jo", "role": "Customer", "image": "https://...", "style": "plain" | "card"}
{"type": "header", "logo": "https://...", "links": [{"text": "Shop", "href": "https://..."}]}
{"type": "nav", "links": [{"text": "New", "href": "https://..."}]}
{"type": "footer", "text": "Questions? Reply.", "links": [{"text": "FAQ", "href": "https://..."}], "show_social": true}
{"type": "image_text", "src": "https://...", "alt": "Tote", "heading": "Built to last", "html": "<p>...</p>", "button_text": "Shop", "button_href": "https://...", "image_position": "left" | "right"}
{"type": "saved", "saved_block_id": "..."}
```

Leave social `links` out to use the account's social links. Countdown and menu bar colors (`background`, `color`, `label_color`) are hex colors. Make a video thumbnail with make_video_thumbnail. Image blocks take `radius` (0-40). Any block can take `"show_if": {"field": "person.properties.tier", "op": "eq", "value": "gold"}` (ops `set`, `not_set`, `eq`, `neq`, `contains`, `gt`, `lt`) or `"show_if": {"expr": "event.value > 100"}` to show it only to some people. `saved` blocks reuse a saved block (list_saved_blocks); starter designs come from list_starter_templates; check an email with check_email before sending.

Variables (Jinja): `first_name`, `last_name`, `email`, `phone_number`, any custom property by name, `person.properties.x`, `event.properties.x` and `event.value` (flows), `organization.name`, `organization.website`, `unsubscribe_url`. Filters: `default('x')`, `upper`, `title`, `round`. The footer with the mailing address and unsubscribe link is added automatically; links are click-tracked automatically.

SMS: `{"body": "Hi {{ first_name|default('there') }}! 20% off today: https://..."}`. The brand name and "Reply STOP to opt out." are added when needed; 160 GSM characters per segment (70 with emoji).
"#;

const FLOW_DOC: &str = r#"# Flows

Triggers: `{"type": "metric", "metric": "Started Checkout", "filters": [{"field": "value", "op": "gt", "value": 50}]}`, `{"type": "list", "list_id": "..."}`, `{"type": "segment", "segment_id": "..."}`, `{"type": "date_property", "field": "birthday", "offset_days": -7}`.

```json
{"entry": "d1",
 "settings": {"reentry": {"after_days": 7},
              "exit_condition": {"match": "all", "conditions": [{"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1, "within_days": 2}]},
              "smart_sending_hours": 16},
 "nodes": [
  {"id": "d1", "type": "delay", "amount": 4, "unit": "hours", "next": "e1"},
  {"id": "e1", "type": "email", "name": "Reminder", "content": {"subject": "...", "blocks": [...]}, "next": "s1"},
  {"id": "s1", "type": "condition", "condition": {"match": "all", "conditions": [{"type": "consent", "channel": "sms", "status": "subscribed"}]}, "yes": "t1", "no": null},
  {"id": "t1", "type": "sms", "content": {"body": "Your cart is waiting: {{ event.properties.checkout_url }}"}, "next": null}
 ]}
```

- `reentry`: `"never"` (default), `"always"` (unless already in the flow) or `{"after_days": N}`.
- `exit_condition`: checked before every step; people who match leave the flow (e.g. they purchased).
- `profile_filter`: only people matching it enter.
- `smart_sending_hours`: skip a message if the person got one on that channel in the last N hours.
- `random_split {percent, yes, no}`: an A/B test inside a flow. Each person goes down `yes` (path A) with `percent`% chance, otherwise `no` (path B). Compare the paths' steps in get_flow's per-step stats, then replace the split with the winning path.
- Other steps: `update_profile {properties}`, `add_to_list`/`remove_from_list {list_id}`, `webhook {url, method, body}`.
- In metric-triggered flows, `{{ event.properties.* }}` is the triggering event (e.g. cart items, checkout_url).
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_schemas_are_objects() {
        let t = tools();
        // A null schema keyword (e.g. "maxLength": null) makes strict clients reject the whole tool list.
        fn no_null_keywords(v: &Value, path: &str) {
            match v {
                Value::Object(o) => o.iter().for_each(|(k, x)| {
                    assert!(!x.is_null() || k == "default", "null schema value at {path}.{k}");
                    no_null_keywords(x, &format!("{path}.{k}"));
                }),
                Value::Array(a) => a.iter().enumerate().for_each(|(i, x)| no_null_keywords(x, &format!("{path}[{i}]"))),
                _ => {}
            }
        }
        t.iter().for_each(|x| no_null_keywords(&x["inputSchema"], x["name"].as_str().unwrap()));
        let mut names: Vec<&str> = t.iter().map(|x| x["name"].as_str().unwrap()).collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(n, names.len());
        for x in &t {
            assert_eq!(x["inputSchema"]["type"], "object", "{}", x["name"]);
        }
        for name in ["get_form", "upload_image", "list_images"] {
            assert!(t.iter().any(|x| x["name"] == name), "{name}");
        }
    }

    /// The config schema lists exactly the keys the server accepts.
    #[test]
    fn form_config_schema_matches_validation() {
        let schema = form_config_schema();
        let keys: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 25);
        for k in &keys {
            let probe = serde_json::json!({ k.as_str(): null });
            assert!(crate::forms::check_config(&probe).is_ok(), "{k} is in the schema but rejected");
        }
        assert_eq!(schema["properties"]["variants"]["items"]["properties"].as_object().unwrap().len(), crate::forms::VARIANT_FIELDS.len() + 1);
        assert!(FORMS_DOC.contains("phone_step"));
    }
}

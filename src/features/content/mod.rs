//! Email content: more blocks (social links, video, countdown timer, table, quote, header, menu bar,
//! footer, image beside text, display rules on any block), saved blocks reused across emails, a
//! starter template library, checks before sending, and the hosted images those need.
//! `docs/CONTENT.md` is the reference.

pub mod blocks;
pub mod check;
pub mod countdown;
pub mod net;
pub mod prepare;
pub mod saved;
pub mod starters;
pub mod video;

use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

const BODY_LIMIT: usize = 512 * 1024;

// ---------- Hosted images ----------

macro_rules! icon {
    ($s:literal, $n:literal) => {
        (concat!($s, "-", $n, ".png"), include_bytes!(concat!("assets/icons/", $s, "-", $n, ".png")) as &[u8])
    };
}

macro_rules! icon_table {
    ($($n:literal),*) => {
        &[$(icon!("color", $n),)* $(icon!("dark", $n),)* $(icon!("light", $n),)* $(icon!("white", $n),)*]
    };
}

static ICONS: &[(&str, &[u8])] = icon_table!("instagram", "facebook", "tiktok", "x", "youtube", "pinterest", "linkedin", "threads", "website", "email");
static ASSETS: &[(&str, &[u8])] = &[
    ("placeholder-hero.png", include_bytes!("assets/placeholder-hero.png")),
    ("placeholder-square.png", include_bytes!("assets/placeholder-square.png")),
];

fn png(found: Option<&&[u8]>) -> Response {
    match found {
        Some(bytes) => (StatusCode::OK, [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=604800, immutable")], bytes.to_vec()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn icon_h(Path(file): Path<String>) -> Response {
    png(ICONS.iter().find(|(n, _)| *n == file).map(|(_, b)| b))
}

async fn asset_h(Path(file): Path<String>) -> Response {
    png(ASSETS.iter().find(|(n, _)| *n == file).map(|(_, b)| b))
}

// ---------- Saved blocks ----------

async fn list_saved_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(saved::list(&state, ctx.account_id).await?))
}

async fn create_saved_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<saved::Input>) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(saved::create(&state, ctx.account_id, ctx.user_id, b).await?)))
}

#[derive(Deserialize, Default)]
struct SavedQuery {
    /// Also a deleted saved block (emails that still use one show its last content).
    #[serde(default)]
    include_deleted: bool,
}

async fn get_saved_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<SavedQuery>) -> AppResult<Json<Value>> {
    Ok(Json(saved::get_with(&state, ctx.account_id, id, q.include_deleted).await?))
}

async fn patch_saved_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<saved::Input>) -> AppResult<Json<Value>> {
    Ok(Json(saved::update(&state, ctx.account_id, id, b).await?))
}

async fn delete_saved_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    saved::delete(&state, ctx.account_id, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn usage_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    saved::get(&state, ctx.account_id, id).await?;
    Ok(Json(saved::usage(&state, ctx.account_id, id).await?))
}

// ---------- Starter templates ----------

#[derive(Deserialize, Default)]
struct StarterQuery {
    /// Include each starter's rendered HTML (for a gallery).
    #[serde(default)]
    preview: bool,
}

fn starter_meta(s: &starters::Starter) -> Value {
    json!({"key": s.key, "name": s.name, "description": s.description, "kind": s.kind, "trigger": s.trigger})
}

async fn starter_content(state: &AppState, account_id: Uuid, key: &str) -> AppResult<(&'static starters::Starter, Value)> {
    let s = starters::find(key).ok_or_else(|| AppError::not_found("starter template"))?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let brand = starters::Brand::of(&account, state.cfg.public_url());
    let content = starters::content(key, &brand).ok_or_else(|| AppError::not_found("starter template"))?;
    Ok((s, content))
}

pub async fn list_starters(state: &AppState, account_id: Uuid, preview: bool) -> AppResult<Value> {
    let mut out = Vec::new();
    for s in starters::STARTERS {
        let mut v = starter_meta(s);
        if preview {
            let (_, content) = starter_content(state, account_id, s.key).await?;
            let r = crate::api::messaging::render_preview(state, account_id, crate::api::messaging::RenderBody { channel: "email".into(), content: content.clone(), profile_id: None, event: starters::sample_event(s.key) }).await?;
            v["html"] = r["html"].clone();
            v["subject"] = content["subject"].clone();
        }
        out.push(v);
    }
    Ok(json!({"data": out}))
}

async fn starters_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<StarterQuery>) -> AppResult<Json<Value>> {
    Ok(Json(list_starters(&state, ctx.account_id, q.preview).await?))
}

async fn starter_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>) -> AppResult<Json<Value>> {
    let (s, content) = starter_content(&state, ctx.account_id, &key).await?;
    let mut v = starter_meta(s);
    v["content"] = content;
    v["sample_event"] = starters::sample_event(s.key).unwrap_or(Value::Null);
    Ok(Json(v))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FromStarter {
    name: Option<String>,
}

pub async fn template_from_starter(state: &AppState, account_id: Uuid, key: &str, name: Option<String>) -> AppResult<Value> {
    let (s, content) = starter_content(state, account_id, key).await?;
    let t = crate::api::messaging::create_template(
        state,
        account_id,
        crate::api::messaging::TemplateBody { name: Some(name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| s.name.to_string())), channel: Some("email".into()), content: Some(content) },
    )
    .await?;
    Ok(serde_json::to_value(t)?)
}

async fn from_starter_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>, body: axum::body::Bytes) -> AppResult<(StatusCode, Json<Value>)> {
    let b: FromStarter = if body.iter().all(u8::is_ascii_whitespace) { FromStarter::default() } else { serde_json::from_slice(&body)? };
    Ok((StatusCode::CREATED, Json(template_from_starter(&state, ctx.account_id, &key, b.name).await?)))
}

// ---------- Checks, video thumbnails ----------

async fn check_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<check::Input>) -> AppResult<Json<Value>> {
    Ok(Json(check::run(&state, ctx.account_id, b).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThumbBody {
    video_url: Option<String>,
    image_url: Option<String>,
}

async fn thumbnail_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ThumbBody>) -> AppResult<Json<Value>> {
    Ok(Json(video::thumbnail(&state, ctx.account_id, b.video_url.as_deref(), b.image_url.as_deref()).await?))
}

fn id_arg(a: &Value) -> AppResult<Uuid> {
    a["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("id must be a saved block id"))
}

const BLOCK_TYPES: &[&str] = &[
    "heading", "text", "image", "button", "divider", "spacer", "columns", "product", "products", "html", "social", "video", "countdown", "table", "quote", "header", "nav", "footer", "image_text",
];

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "content"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/content/saved-blocks", get(list_saved_h).post(create_saved_h))
            .route("/api/content/saved-blocks/{id}", get(get_saved_h).patch(patch_saved_h).delete(delete_saved_h))
            .route("/api/content/saved-blocks/{id}/usage", get(usage_h))
            .route("/api/content/starters", get(starters_h))
            .route("/api/content/starters/{key}", get(starter_h))
            .route("/api/content/starters/{key}/template", post(from_starter_h))
            .route("/api/content/check", post(check_h))
            .route("/api/content/video-thumbnail", post(thumbnail_h))
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
            .route("/e/icons/{file}", get(icon_h))
            .route("/e/assets/{file}", get(asset_h))
            .route("/e/countdown/{file}", get(countdown::image_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let block = json!({"type": "object", "required": ["type"], "properties": {"type": {"enum": BLOCK_TYPES}}, "description": "An email block (see dumbmail://docs/email and dumbmail://docs/content)."});
        let blocks = json!({"type": "array", "minItems": 1, "maxItems": saved::MAX_BLOCKS, "items": block});
        vec![
            tool("list_saved_blocks", "List saved blocks", "Saved blocks (reusable groups of email blocks) with their content and how many unsent emails use each. Put one in an email as {\"type\": \"saved\", \"saved_block_id\": \"...\"}; it shows the saved block's current content until the email is sent.", obj(json!({}), &[]), true, false),
            tool("save_block", "Save a block", "Save one or more email blocks under a name so they can be reused in any email (a header, a footer, a sign-off). Names are unique per account; up to 200 saved blocks.", obj(json!({"name": {"type": "string", "maxLength": 100}, "blocks": blocks}), &["name", "blocks"]), false, false),
            tool("update_saved_block", "Update a saved block", "Rename a saved block or replace its blocks. Every template, draft or scheduled campaign and flow email that uses it shows the new content; campaigns already sending or sent keep what they sent. Refused (naming them) when the new content would stop a scheduled campaign or live flow that uses it from sending. Returns where it's used.", obj(json!({"id": {"type": "string"}, "name": {"type": "string", "maxLength": 100}, "blocks": blocks}), &["id"]), false, false),
            tool("delete_saved_block", "Delete a saved block", "Remove a saved block from the list. Emails that use it keep showing its last content. Without confirm=true it only says where it's used.", obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
            tool("list_starter_templates", "List starter templates", "Ready-made email designs (welcome, newsletter, product launch, sale, abandoned cart, order confirmation, shipping, review request, win-back, birthday, back in stock, personal note, booking confirmation, pre-arrival info) in the account's brand. Give key to get one's full content, to use in a campaign, flow email or template.", obj(json!({"key": {"type": "string"}}), &[]), true, false),
            tool("create_template_from_starter", "Create a template from a starter", "Create an email template from a starter template (keys from list_starter_templates), filled in with the account's brand.", obj(json!({"key": {"type": "string"}, "name": {"type": "string", "maxLength": 200}}), &["key"]), false, false),
            tool("check_email", "Check an email before sending", "Check an email for problems: broken links and images, missing alt text, subject and preview text, images that are too heavy, Gmail clipping (over 102 KB), personalization that comes out empty for some recipients (with counts), an empty or unsubscribed audience, spam signals and the sending domain. Give content, or campaign_id (optionally with an unsaved audience), or flow_id with node_id. Findings marked blocks_sending stop a send; the rest are advice. Also counts how many people display rules would hide every block for. Requests the email's links unless skip_links is true (DumbMail's own tracking and unsubscribe links are never requested).",
                obj(json!({"content": {"type": "object"}, "campaign_id": {"type": "string"}, "audience": {"type": "object"}, "flow_id": {"type": "string"}, "node_id": {"type": "string"}, "skip_links": {"type": "boolean"}}), &[]), true, false),
            tool("make_video_thumbnail", "Make a video thumbnail", "Make a thumbnail with a play button for a video block: from a YouTube or Vimeo link (video_url), or from an image (image_url). Stores it as one of the account's images and returns thumbnail_url.", obj(json!({"video_url": {"type": "string", "format": "uri"}, "image_url": {"type": "string", "format": "uri"}}), &[]), false, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        const NAMES: [&str; 8] = ["list_saved_blocks", "save_block", "update_saved_block", "delete_saved_block", "list_starter_templates", "create_template_from_starter", "check_email", "make_video_thumbnail"];
        if !NAMES.contains(&name) {
            return None;
        }
        let acct = ctx.account_id;
        let a = args.clone();
        let r = async {
            match name {
                "list_saved_blocks" => saved::list(state, acct).await,
                "save_block" => saved::create(state, acct, ctx.user_id, from(json!({"name": a["name"], "blocks": a["blocks"]}))?).await,
                "update_saved_block" => {
                    let id = id_arg(&a)?;
                    let mut b = a.clone();
                    if let Some(o) = b.as_object_mut() {
                        o.remove("id");
                    }
                    saved::update(state, acct, id, from(b)?).await
                }
                "delete_saved_block" => {
                    let id = id_arg(&a)?;
                    let block = saved::get(state, acct, id).await?;
                    if a["confirm"].as_bool() != Some(true) {
                        let n = block["used_in"].as_array().map(|u| u.len()).unwrap_or(0);
                        return Ok(json!({"dry_run": true, "used_in": block["used_in"], "note": format!("Nothing deleted. This removes the saved block {:?} from the list; the {n} email(s) using it keep its current content. Call again with confirm=true once the user approves.", block["name"].as_str().unwrap_or(""))}));
                    }
                    saved::delete(state, acct, id).await?;
                    Ok(json!({"ok": true}))
                }
                "list_starter_templates" => match a["key"].as_str() {
                    Some(key) => {
                        let (s, content) = starter_content(state, acct, key).await?;
                        let mut v = starter_meta(s);
                        v["content"] = content;
                        Ok(v)
                    }
                    None => list_starters(state, acct, false).await,
                },
                "create_template_from_starter" => {
                    let key = a["key"].as_str().ok_or_else(|| AppError::bad("key is required"))?;
                    template_from_starter(state, acct, key, a["name"].as_str().map(String::from)).await
                }
                "check_email" => check::run(state, acct, from(a.clone())?).await,
                "make_video_thumbnail" => video::thumbnail(state, acct, a["video_url"].as_str(), a["image_url"].as_str()).await,
                _ => unreachable!("checked above"),
            }
        }
        .await;
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "content",
            name: "Email content: blocks, saved blocks, starters, checks",
            description: "Social, video, countdown, table, quote, header, menu, footer and image-beside-text blocks; display rules; saved blocks; starter templates; checks before sending",
            text: include_str!("../../../docs/CONTENT.md"),
        }]
    }
}

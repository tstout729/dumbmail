//! Product and stay reviews: asking for them, the hosted review page, moderation, website widgets,
//! email blocks, segments, reports, import and export. See docs/REVIEWS.md.
//!
//! - `store`: settings, the things reviewed and their published summaries, reviews and moderation.
//! - `requests`: review links, the `review_request` flow step, the email blocks and `{% review_link %}`.
//! - `page`: the hosted review page, photo uploads, the author's edits and deletes, public reviews.
//! - `photos`: re-encoding and serving review photos.
//! - `widget` (+ `widget.js`): the public reads behind the website widgets, and structured data.
//! - `segment`, `report`, `io`: the segment condition, reports, CSV import and export.

pub mod api;
pub mod hooks;
pub mod io;
pub mod page;
pub mod photos;
pub mod report;
pub mod requests;
pub mod segment;
pub mod store;
pub mod widget;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub use requests::{check_step, flow_step, personalize, READY_METRIC, SUBMITTED_METRIC};

use Perm::{AnalyticsRead, CatalogDelete, ContentRead, Export, FlowEdit, ReviewsManage};

// Reading reviews (text and display names) needs content.read; who wrote them also needs
// profiles.read (checked where the author is added). Moderating, replying, products and settings:
// reviews.manage (Managers and up). Import and delete: catalog.delete (Admins and owners). Export:
// export. Sending a request now also needs campaigns.send (checked in the handler).
static ROUTE_RULES: &[RouteRule] = &[
    rule("GET", "/api/reviews", ContentRead),
    rule("POST", "/api/reviews/moderate", ReviewsManage),
    rule("GET", "/api/reviews/settings", ContentRead),
    rule("PATCH", "/api/reviews/settings", ReviewsManage),
    rule("GET", "/api/reviews/products", ContentRead),
    rule("POST", "/api/reviews/products", ReviewsManage),
    rule("POST", "/api/reviews/products/rebuild", ReviewsManage),
    rule("POST", "/api/reviews/requests", ReviewsManage),
    rule("GET", "/api/reviews/report", AnalyticsRead),
    rule("POST", "/api/reviews/import", CatalogDelete),
    rule("GET", "/api/reviews/imports/{id}", CatalogDelete),
    rule("GET", "/api/reviews/export", Export),
    rule("POST", "/api/reviews/flows", FlowEdit),
    rule("GET", "/api/reviews/{id}", ContentRead),
    rule("DELETE", "/api/reviews/{id}", CatalogDelete),
    rule("POST", "/api/reviews/{id}/reply", ReviewsManage),
    rule("DELETE", "/api/reviews/{id}/photos/{photo_id}", ReviewsManage),
    public("GET", "/e/review/{token}"),
    public("POST", "/e/review/{token}"),
    public("POST", "/e/review/{token}/photos"),
    public("DELETE", "/e/review/{token}/photos/{photo_id}"),
    public("POST", "/e/review/{token}/delete"),
    public("GET", "/e/review-confirm/{token}"),
    public("POST", "/e/review-confirm/{token}"),
    public("GET", "/i/rv/{file}"),
    public("GET", "/client/reviews.js"),
    public("GET", "/client/reviews"),
    public("OPTIONS", "/client/reviews"),
    public("GET", "/client/reviews/summary"),
    public("OPTIONS", "/client/reviews/summary"),
    public("GET", "/client/reviews/all"),
    public("OPTIONS", "/client/reviews/all"),
    public("GET", "/client/reviews/jsonld"),
    public("OPTIONS", "/client/reviews/jsonld"),
    public("POST", "/client/reviews/write"),
    public("OPTIONS", "/client/reviews/write"),
];

static TOOL_RULES: &[ToolRule] = &[
    ToolRule { name: "list_reviews", perm: ContentRead },
    ToolRule { name: "get_review", perm: ContentRead },
    ToolRule { name: "moderate_reviews", perm: ReviewsManage },
    ToolRule { name: "reply_to_review", perm: ReviewsManage },
    ToolRule { name: "remove_review_photo", perm: ReviewsManage },
    ToolRule { name: "delete_review", perm: CatalogDelete },
    ToolRule { name: "get_review_settings", perm: ContentRead },
    ToolRule { name: "update_review_settings", perm: ReviewsManage },
    ToolRule { name: "list_review_products", perm: ContentRead },
    ToolRule { name: "save_review_product", perm: ReviewsManage },
    ToolRule { name: "rebuild_review_summaries", perm: ReviewsManage },
    ToolRule { name: "create_review_request", perm: ReviewsManage },
    ToolRule { name: "get_review_report", perm: AnalyticsRead },
    ToolRule { name: "import_reviews_csv", perm: CatalogDelete },
    ToolRule { name: "get_review_import", perm: CatalogDelete },
    ToolRule { name: "export_reviews_csv", perm: Export },
    ToolRule { name: "create_review_flow", perm: FlowEdit },
];

pub struct Module;

const DOC: &str = include_str!("../../../docs/REVIEWS.md");

fn filter_props() -> Value {
    json!({
        "status": {"enum": store::STATUSES},
        "rating": {"type": "integer", "minimum": 1, "maximum": 5},
        "product_id": {"type": "string", "description": "The store's product id (or listing or service key)."},
        "verified": {"type": "boolean"},
        "has_photos": {"type": "boolean"},
        "source": {"enum": ["request", "public", "import"]},
        "unconfirmed": {"type": "boolean", "description": "true: public reviews still waiting for their email confirmation."},
        "since": {"type": "string", "description": "YYYY-MM-DD, in the account's time zone."},
        "until": {"type": "string", "description": "YYYY-MM-DD, in the account's time zone (inclusive)."},
        "q": {"type": "string", "description": "Words in the title, text or name."}
    })
}

fn tools() -> Vec<Value> {
    let mut list_props = filter_props();
    list_props["limit"] = json!({"type": "integer", "maximum": 200});
    list_props["offset"] = json!({"type": "integer"});
    vec![
        tool("list_reviews", "List reviews", "Reviews with filters (status, rating, product, verified, photos, source, dates, words), newest first, with counts per status. Who wrote each one is included only for callers who may read profiles.",
            obj(list_props, &[]), true, false),
        tool("get_review", "Get a review", "One review with its product, photos, flags, reply and status.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
        tool("moderate_reviews", "Publish, reject, hold or feature reviews",
            "Change up to 500 reviews: publish, reject (with a reason: spam, offensive, personal_info, not_about_this, fake, other), hold (back to pending), feature or unfeature. Never decide by rating: hiding negative reviews is unlawful in the US and EU. Publishing or rejecting more than one review needs confirm=true; without it you get a dry run.",
            obj(json!({"ids": {"type": "array", "items": {"type": "string"}, "maxItems": 500}, "action": {"enum": ["publish", "reject", "hold", "feature", "unfeature"]},
                "reason": {"enum": store::REJECT_REASONS}, "confirm": {"type": "boolean"}}), &["ids", "action"]), false, false),
        tool("reply_to_review", "Reply to a review", "The business's public reply, shown under the review. Empty text removes it. Reviews themselves can't be edited.",
            obj(json!({"id": {"type": "string"}, "text": {"type": "string", "maxLength": 2000}}), &["id", "text"]), false, false),
        tool("remove_review_photo", "Remove a review photo", "Take one photo off a review. The file stops being served at once.", obj(json!({"id": {"type": "string"}, "photo_id": {"type": "string"}}), &["id", "photo_id"]), false, false),
        tool("delete_review", "Delete a review", "Delete a review and its photos for good. To take one off the site but keep it, reject it instead. Without confirm=true it only describes what would happen.",
            obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("get_review_settings", "Get review settings", "Auto-publish rule, flag words, questions, public reviews, link lifetime, thank-you coupon, sign-up form and widget look.", obj(json!({}), &[]), true, false),
        tool("update_review_settings", "Change review settings", "Fields left out stay. Auto-publish only ever applies to verified reviews with nothing flagged; everything else is held for a person.",
            obj(json!({
                "auto_publish": {"type": "boolean"},
                "auto_publish_min_rating": {"type": "integer", "minimum": 1, "maximum": 5, "description": "Verified reviews with at least this many stars and nothing flagged go live at once (default 4)."},
                "flag_words": {"type": "array", "items": {"type": "string"}, "maxItems": 200, "description": "Your own words that hold a review for a person (the built-in list catches profanity, emails, phone numbers and links)."},
                "questions": {"type": "array", "maxItems": 3, "items": {"type": "object", "properties": {"id": {"type": "string"}, "label": {"type": "string"}, "kind": {"enum": ["choice", "text"]}, "options": {"type": "array", "items": {"type": "string"}}}}},
                "public_reviews": {"type": "boolean", "description": "Let visitors write a review on your site without a link (unverified, confirmed by email, always held). Off by default."},
                "request_days": {"type": "integer", "minimum": 7, "maximum": 365, "description": "How long a review link works (default 60)."},
                "thank_you_coupon": {"type": ["string", "null"], "description": "A coupon's name: its code is emailed as a thank-you for any review, whatever the rating, and those reviews say so."},
                "subscribe_form_id": {"type": ["string", "null"], "description": "A sign-up form: reviewers may tick a box to join its list (through the form's own consent rules)."},
                "widget": {"type": "object", "description": "heading, star_color, text_color, accent_color, per_page."}
            }), &[]), false, false),
        tool("list_review_products", "List reviewed products", "Products, listings and services with their published star summary and how many reviews wait for moderation.",
            obj(json!({"q": {"type": "string"}, "limit": {"type": "integer", "maximum": 200}, "offset": {"type": "integer"}}), &[]), true, false),
        tool("save_review_product", "Name something to review", "For accounts without a catalog (a rental listing, a service): its name, image, page and kind. Catalog products get these from the catalog.",
            obj(json!({"product_id": {"type": "string"}, "name": {"type": "string"}, "image_url": {"type": "string"}, "url": {"type": "string"}, "kind": {"enum": ["product", "listing", "service"]}}), &["product_id"]), false, false),
        tool("rebuild_review_summaries", "Recount star summaries", "Recount every product's published average and histogram from its reviews, in the background.", obj(json!({}), &[]), false, false),
        tool("create_review_request", "Make a review link",
            "Make a signed review link for one person and one or more things they bought or booked. Returns the links (it needs permission to see profiles). Reviews from it are verified only when you pass order_id, your reference for the order or booking; without one the request is unverified. With send=email or sms it also sends the ask now, only if they're subscribed on that channel (smart sending over smart_sending_hours, default 16, and quiet hours apply; a text that must wait goes later); that needs confirm=true.",
            obj(json!({
                "profile_id": {"type": "string"}, "email": {"type": "string"},
                "product_id": {"type": "string"}, "name": {"type": "string"}, "image_url": {"type": "string"}, "url": {"type": "string"}, "kind": {"enum": ["product", "listing", "service"]},
                "items": {"type": "array", "maxItems": 6, "items": {"type": "object"}, "description": "Instead of product_id: items like {product_id, name, image_url}."},
                "order_id": {"type": "string", "description": "Your reference for the order or booking: it makes the request verified, and each order line is only ever asked about once."},
                "verified": {"type": "boolean", "description": "false: unverified even with an order_id."}, "send": {"enum": ["email", "sms"]},
                "smart_sending_hours": {"type": "integer", "minimum": 0, "maximum": 168}, "confirm": {"type": "boolean"}
            }), &[]), false, false),
        tool("get_review_report", "Review report", "Requests sent, response rate, average rating over time (account time zone) and rating by product.",
            obj(json!({"days": {"type": "integer", "minimum": 1, "maximum": 730}}), &[]), true, false),
        tool("import_reviews_csv", "Import reviews from CSV",
            "Import existing reviews from another app's CSV export (product id or handle, rating, title, text, name, email, date, verified, photo URLs, reply). Without confirm=true it's a dry run showing the column mapping, a sample and the rows it would skip. Imported reviews are marked as imported.",
            obj(json!({"csv": {"type": "string"}, "mapping": {"type": "object", "description": "Our field -> your column name. Fields: product_id, product_handle, rating, title, body, name, email, date, verified, photos, reply."},
                "publish": {"type": "boolean", "description": "Publish them (default true) or hold them for moderation."}, "confirm": {"type": "boolean"}}), &["csv"]), false, true),
        tool("get_review_import", "Check a review import", "How far an import got, with the rows it skipped and why.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
        tool("export_reviews_csv", "Export reviews as CSV", "Reviews matching the filters as CSV text (at most 50,000). with_people=true adds each reviewer's email, which needs an Admin or Owner (exports.manage) and permission to see profiles.",
            obj({ let mut p = filter_props(); p["with_people"] = json!({"type": "boolean"}); p }, &[]), true, false),
        tool("create_review_flow", "Set up the review request flow", "Create the starter flow as a draft. kind=order (default): ask 14 days after Placed Order (5 days after Fulfilled Order or Delivered, when you record those). kind=stay: ask the day after check-out, from the Guesty recipe's Reservation Confirmed events (a cancelled booking leaves the flow). Both remind once a week later if they haven't reviewed.",
            obj(json!({"kind": {"enum": ["order", "stay"]}}), &[]), false, false),
    ]
}

fn id_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("{k} must be an id")))
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    match name {
        "list_reviews" => api::list(state, ctx, &from(a.clone())?).await,
        "get_review" => api::get_one(state, ctx, id_arg(a, "id")?).await,
        "moderate_reviews" => api::moderate(state, ctx, from(a.clone())?).await,
        "reply_to_review" => api::reply(state, ctx, id_arg(a, "id")?, a["text"].as_str().unwrap_or("")).await,
        "remove_review_photo" => api::remove_photo(state, ctx, id_arg(a, "id")?, id_arg(a, "photo_id")?).await,
        "delete_review" => api::delete_review(state, ctx, id_arg(a, "id")?, a["confirm"].as_bool() == Some(true)).await,
        "get_review_settings" => api::settings_json(state, ctx.account_id).await,
        "update_review_settings" => api::update_settings(state, ctx, from(a.clone())?).await,
        "list_review_products" => api::products(state, ctx.account_id, &from(a.clone())?).await,
        "save_review_product" => api::save_product(state, ctx, from(a.clone())?).await,
        "rebuild_review_summaries" => api::rebuild(state, ctx).await,
        "create_review_request" => api::create_request(state, ctx, from(a.clone())?).await,
        "get_review_report" => {
            let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
            report::report(&state.db, ctx.account_id, &account.timezone, a["days"].as_i64().unwrap_or(90)).await
        }
        "import_reviews_csv" => api::import(state, ctx, from(a.clone())?).await,
        "get_review_import" => io::get_import(state, ctx.account_id, id_arg(a, "id")?).await,
        "export_reviews_csv" => {
            let mut q = a.clone();
            let with_people = q.as_object_mut().and_then(|o| o.remove("with_people")).and_then(|v| v.as_bool()) == Some(true);
            Ok(json!({"csv": api::export(state, ctx, &from(q)?, with_people).await?}))
        }
        "create_review_flow" => api::create_flow(state, ctx, from(a.clone())?).await,
        _ => Err(AppError::not_found("tool")),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "reviews"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn access(&self) -> Access {
        Access { routes: ROUTE_RULES, tools: TOOL_RULES }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !TOOL_RULES.iter().any(|t| t.name == name) {
            return None;
        }
        Some(call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "reviews",
            name: "Reviews",
            description: "Asking for product and stay reviews, the hosted review page, moderation and honesty rules, website widgets, review email blocks, segments, reports, import and export.",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        Some(match kind {
            "reviews_submitted" => page::after_submit(state, payload).await,
            "reviews_send" => requests::run_send_job(state, payload).await,
            "reviews_confirm" => page::send_confirmation(state, payload).await,
            "reviews_import" => io::run_import(state, payload).await,
            "reviews_rebuild" => match payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                Some(a) => store::rebuild_summaries(&state.db, a).await.map(|_| ()).map_err(|e| anyhow::anyhow!(e.public_message())),
                None => Ok(()),
            },
            _ => return None,
        })
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        // Requests carried by a block or tag count as sent once their message was handed over.
        if tick.is_multiple_of(3) {
            requests::settle_carried(state).await?;
        }
        if tick.is_multiple_of(180) {
            // Public reviews nobody confirmed, and photos uploaded for reviews never sent.
            sqlx::query("DELETE FROM reviews WHERE confirmed_at IS NULL AND source = 'public' AND created_at < now() - make_interval(days => $1)")
                .bind(page::CONFIRM_DAYS as i32)
                .execute(&state.db)
                .await?;
            sqlx::query("DELETE FROM review_photos WHERE review_id IS NULL AND created_at < now() - interval '2 days'").execute(&state.db).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_rules_schemas_and_docs() {
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            assert_eq!(t["inputSchema"]["type"], "object", "{name}");
            assert!(TOOL_RULES.iter().any(|r| r.name == name), "{name} needs a permission rule");
            assert!(DOC.contains(name), "docs/REVIEWS.md should mention {name}");
        }
        assert_eq!(tools().len(), TOOL_RULES.len());
    }
}

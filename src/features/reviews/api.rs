//! REST routes for reviews. The MCP tools call the same functions.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::store::{self, Review, Subject};
use super::{io, page, photos, report, requests, widget};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::Perm;
use crate::features::team::audit;
use crate::models::{Profile, PROFILE_COLS};
use crate::state::AppState;

/// The most reviews one moderation call changes.
pub const MAX_BULK: usize = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/reviews", get(list_h))
        .route("/api/reviews/moderate", post(moderate_h))
        .route("/api/reviews/settings", get(settings_h).patch(update_settings_h))
        .route("/api/reviews/products", get(products_h).post(save_product_h))
        .route("/api/reviews/products/rebuild", post(rebuild_h))
        .route("/api/reviews/requests", post(create_request_h))
        .route("/api/reviews/report", get(report_h))
        .route("/api/reviews/import", post(import_h).layer(DefaultBodyLimit::max(io::MAX_CSV + 64 * 1024)))
        .route("/api/reviews/imports/{id}", get(get_import_h))
        .route("/api/reviews/export", get(export_h))
        .route("/api/reviews/flows", post(flow_h))
        .route("/api/reviews/{id}", get(get_h).delete(delete_h))
        .route("/api/reviews/{id}/reply", post(reply_h))
        .route("/api/reviews/{id}/photos/{photo_id}", delete(remove_photo_h))
        // Public: the hosted review page and its confirmation, photos, and the widgets' reads.
        .route("/e/review/{token}", get(page::page).post(page::submit).layer(DefaultBodyLimit::max(64 * 1024)))
        .route("/e/review/{token}/photos", post(page::upload_photo).layer(DefaultBodyLimit::max(photos::MAX_UPLOAD + 1024)))
        .route("/e/review/{token}/photos/{photo_id}", delete(page::delete_photo))
        .route("/e/review/{token}/delete", post(page::delete_own))
        .route("/e/review-confirm/{token}", get(page::confirm_page).post(page::confirm_post))
        .route("/i/rv/{file}", get(photos::serve))
        .route("/client/reviews.js", get(widget::widget_js))
        .route("/client/reviews", get(widget::product_reviews).options(widget::preflight))
        .route("/client/reviews/summary", get(widget::summaries).options(widget::preflight))
        .route("/client/reviews/all", get(widget::all_reviews).options(widget::preflight))
        .route("/client/reviews/jsonld", get(widget::json_ld_h).options(widget::preflight))
        .route("/client/reviews/write", post(page::write_public_h).options(widget::preflight).layer(DefaultBodyLimit::max(16 * 1024)))
}

fn tz(account: &crate::models::Account) -> String {
    account.timezone.parse::<chrono_tz::Tz>().map(|_| account.timezone.clone()).unwrap_or_else(|_| "America/Los_Angeles".into())
}

/// A review as the dashboard shows it. Who wrote it (email, profile) only for callers who may read profiles.
pub async fn review_json(state: &AppState, ctx: &Ctx, r: &Review, s: &Subject, photo_ids: &[(Uuid, i32, i32)]) -> AppResult<Value> {
    let mut v = json!({
        "id": r.id, "status": r.status, "reject_reason": r.reject_reason, "rating": r.rating, "title": r.title, "body": r.body, "display_name": r.display_name,
        "recommend": r.recommend, "answers": r.answers, "verified": r.verified, "source": r.source, "imported": r.source == "import", "incentivized": r.incentivized,
        "flags": r.flags, "featured": r.featured, "reply": r.reply, "replied_at": r.replied_at, "awaiting_confirmation": r.confirmed_at.is_none(),
        "reviewed_at": r.reviewed_at, "created_at": r.created_at, "edited_at": r.edited_at, "published_at": r.published_at, "moderated_at": r.moderated_at,
        "product": {"id": s.id, "product_id": s.external_id, "name": s.name, "image_url": s.image_url, "url": s.url, "kind": s.kind},
        "photos": photo_ids.iter().map(|(id, w, h)| json!({"id": id, "url": photos::signed_url(state, *id), "width": w, "height": h})).collect::<Vec<_>>(),
    });
    if ctx.can(Perm::ProfilesRead) {
        if let Some(pid) = r.profile_id {
            let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as("SELECT email, first_name, last_name FROM profiles WHERE id = $1").bind(pid).fetch_optional(&state.db).await?;
            if let Some((email, first, last)) = row {
                v["author"] = json!({"profile_id": pid, "email": email, "first_name": first, "last_name": last});
            }
        }
    }
    Ok(v)
}

pub async fn list(state: &AppState, ctx: &Ctx, q: &store::ListQuery) -> AppResult<Value> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let (rows, total) = store::list(&state.db, ctx.account_id, &tz(&account), q).await?;
    let photos = store::photos_of(&state.db, &rows.iter().map(|(r, _, _)| r.id).collect::<Vec<_>>()).await?;
    let mut data = Vec::new();
    for (r, s, _) in &rows {
        data.push(review_json(state, ctx, r, s, photos.get(&r.id).map(|p| p.as_slice()).unwrap_or(&[])).await?);
    }
    let counts: Vec<(String, i64)> = sqlx::query_as("SELECT status, count(*) FROM reviews WHERE account_id = $1 AND confirmed_at IS NOT NULL GROUP BY status").bind(ctx.account_id).fetch_all(&state.db).await?;
    let unconfirmed: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews WHERE account_id = $1 AND confirmed_at IS NULL").bind(ctx.account_id).fetch_one(&state.db).await?;
    let mut by = serde_json::Map::new();
    for (s, n) in counts {
        by.insert(s, json!(n));
    }
    by.insert("awaiting_confirmation".into(), json!(unconfirmed));
    Ok(json!({"data": data, "total": total, "counts": by, "note": HONESTY}))
}

/// Shown with the moderation queue.
pub const HONESTY: &str = "Publish or reject a review for what it says, never for its rating: hiding negative reviews is unlawful in the US (FTC rule on consumer reviews) and the EU. Reject only spam, offensive content, personal details, fakes or reviews about something else.";

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<store::ListQuery>) -> AppResult<Json<Value>> {
    Ok(Json(list(&state, &ctx, &q).await?))
}

pub async fn get_one(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let r = store::get(&state.db, ctx.account_id, id).await?;
    let s = store::subject_by_id(&state.db, r.subject_id).await?;
    let photos = store::photos_of(&state.db, &[r.id]).await?;
    review_json(state, ctx, &r, &s, photos.get(&r.id).map(|p| p.as_slice()).unwrap_or(&[])).await
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(get_one(&state, &ctx, id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerateBody {
    pub ids: Vec<Uuid>,
    /// publish, reject, hold (back to pending), feature or unfeature.
    pub action: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confirm: bool,
}

pub async fn moderate(state: &AppState, ctx: &Ctx, b: ModerateBody) -> AppResult<Value> {
    let mut ids = b.ids.clone();
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() > MAX_BULK {
        return Err(AppError::bad(format!("give 1 to {MAX_BULK} review ids")));
    }
    let status = match b.action.as_str() {
        "publish" => Some("published"),
        "reject" => Some("rejected"),
        "hold" => Some("pending"),
        "feature" | "unfeature" => None,
        other => return Err(AppError::bad(format!("action must be publish, reject, hold, feature or unfeature (got {other:?})"))),
    };
    let reason = b.reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if b.action == "reject" && !reason.is_some_and(|r| store::REJECT_REASONS.contains(&r)) {
        return Err(AppError::bad(format!("say why you're rejecting it: one of {} (never the rating)", store::REJECT_REASONS.join(", "))));
    }
    let found: Vec<(Uuid, i16, String)> = sqlx::query_as("SELECT id, rating, status FROM reviews WHERE account_id = $1 AND id = ANY($2)").bind(ctx.account_id).bind(&ids).fetch_all(&state.db).await?;
    if found.len() != ids.len() {
        return Err(AppError::not_found("review"));
    }
    if ids.len() > 1 && !b.confirm && matches!(b.action.as_str(), "publish" | "reject") {
        let low = found.iter().filter(|(_, r, _)| *r <= 2).count();
        return Ok(json!({"dry_run": true, "reviews": ids.len(), "low_ratings": low,
            "note": format!("Nothing changed. This would {} {} review(s){}. {HONESTY} Call again with confirm=true once the user approves.", b.action, ids.len(),
                if low > 0 { format!(", {low} of them rated 1 or 2 stars") } else { String::new() })}));
    }
    let mut tx = state.db.begin().await?;
    let changed: Vec<Uuid> = match status {
        Some(st) => store::set_status(&mut tx, ctx.account_id, &ids, st, reason).await?,
        None => sqlx::query_scalar("UPDATE reviews SET featured = $3, updated_at = now() WHERE account_id = $1 AND id = ANY($2) AND featured <> $3 AND status <> 'removed' RETURNING id")
            .bind(ctx.account_id)
            .bind(&ids)
            .bind(b.action == "feature")
            .fetch_all(&mut *tx)
            .await?,
    };
    tx.commit().await?;
    let target = if ids.len() == 1 { Some(ids[0].to_string()) } else { None };
    audit(state, ctx, &format!("review.{}", b.action), target.as_deref(), json!({"reviews": ids.len(), "changed": changed.len(), "reason": reason})).await;
    let skipped = ids.len() - changed.len();
    Ok(json!({"ok": true, "changed": changed, "unchanged": skipped,
        "note": if skipped > 0 { Some("Some reviews were already like that, were deleted by their author, or are still waiting for their email confirmation.") } else { None }}))
}

async fn moderate_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ModerateBody>) -> AppResult<Json<Value>> {
    Ok(Json(moderate(&state, &ctx, b).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyBody {
    pub text: String,
}

/// The business's public reply under a review (empty text removes it). The review itself can't be edited.
pub async fn reply(state: &AppState, ctx: &Ctx, id: Uuid, text: &str) -> AppResult<Value> {
    let t = store::plain(text, store::MAX_REPLY);
    let n = sqlx::query("UPDATE reviews SET reply = $3, replied_at = CASE WHEN $3::text IS NULL THEN NULL ELSE now() END, updated_at = now() WHERE id = $1 AND account_id = $2 AND status <> 'removed'")
        .bind(id)
        .bind(ctx.account_id)
        .bind(if t.is_empty() { None } else { Some(&t) })
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("review"));
    }
    audit(state, ctx, if t.is_empty() { "review.reply_remove" } else { "review.reply" }, Some(&id.to_string()), json!({"chars": t.chars().count()})).await;
    get_one(state, ctx, id).await
}

async fn reply_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ReplyBody>) -> AppResult<Json<Value>> {
    Ok(Json(reply(&state, &ctx, id, &b.text).await?))
}

pub async fn remove_photo(state: &AppState, ctx: &Ctx, id: Uuid, photo: Uuid) -> AppResult<Value> {
    let n = sqlx::query("DELETE FROM review_photos WHERE id = $1 AND review_id = $2 AND account_id = $3").bind(photo).bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("photo"));
    }
    audit(state, ctx, "review.photo_remove", Some(&id.to_string()), json!({"photo": photo})).await;
    Ok(json!({"ok": true}))
}

async fn remove_photo_h(State(state): State<AppState>, ctx: Ctx, Path((id, photo)): Path<(Uuid, Uuid)>) -> AppResult<Json<Value>> {
    Ok(Json(remove_photo(&state, &ctx, id, photo).await?))
}

pub async fn delete_review(state: &AppState, ctx: &Ctx, id: Uuid, confirm: bool) -> AppResult<Value> {
    let r = store::get(&state.db, ctx.account_id, id).await?;
    if !confirm {
        return Ok(json!({"dry_run": true, "note": format!("Nothing deleted. This deletes the {}-star review by {:?} and its photos for good; it can't be undone. To take it off the site but keep it, reject it instead. Call again with confirm=true once the user approves.", r.rating, r.display_name)}));
    }
    let mut tx = state.db.begin().await?;
    store::delete_reviews(&mut tx, ctx.account_id, &[id]).await?;
    tx.commit().await?;
    audit(state, ctx, "review.delete", Some(&id.to_string()), json!({"rating": r.rating, "status": r.status})).await;
    Ok(json!({"ok": true, "deleted": id}))
}

#[derive(Deserialize)]
struct ConfirmQuery {
    #[serde(default)]
    confirm: bool,
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<ConfirmQuery>) -> AppResult<Json<Value>> {
    Ok(Json(delete_review(&state, &ctx, id, q.confirm).await?))
}

pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = store::settings(&state.db, account_id).await?;
    let mut v = json!(s);
    v["reject_reasons"] = json!(store::REJECT_REASONS);
    v["honesty"] = json!(HONESTY);
    Ok(v)
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}

pub async fn update_settings(state: &AppState, ctx: &Ctx, p: store::SettingsPatch) -> AppResult<Value> {
    let fields: Vec<&str> = [
        ("auto_publish", p.auto_publish.is_some()),
        ("auto_publish_min_rating", p.auto_publish_min_rating.is_some()),
        ("flag_words", p.flag_words.is_some()),
        ("questions", p.questions.is_some()),
        ("public_reviews", p.public_reviews.is_some()),
        ("request_days", p.request_days.is_some()),
        ("thank_you_coupon", p.thank_you_coupon.is_some()),
        ("subscribe_form_id", p.subscribe_form_id.is_some()),
        ("widget", p.widget.is_some()),
    ]
    .into_iter()
    .filter_map(|(k, set)| set.then_some(k))
    .collect();
    store::update_settings(&state.db, ctx.account_id, p).await?;
    audit(state, ctx, "reviews.settings_change", None, json!({"fields": fields})).await;
    settings_json(state, ctx.account_id).await
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(p): ApiJson<store::SettingsPatch>) -> AppResult<Json<Value>> {
    Ok(Json(update_settings(&state, &ctx, p).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct ProductsQuery {
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn products(state: &AppState, account_id: Uuid, q: &ProductsQuery) -> AppResult<Value> {
    let like = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.chars().take(100).collect::<String>().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
    let sql = format!(
        "SELECT {}, count(*) OVER () AS total, (SELECT count(*) FROM reviews r WHERE r.subject_id = s.id AND r.status = 'pending' AND r.confirmed_at IS NOT NULL) AS pending
         FROM review_subjects s WHERE s.account_id = $1 AND ($2::text IS NULL OR s.name ILIKE $2 OR s.external_id ILIKE $2)
         ORDER BY s.review_count DESC, s.name LIMIT $3 OFFSET $4",
        store::SUBJECT_COLS
    );
    use sqlx::Row;
    let rows = sqlx::query(&sql).bind(account_id).bind(&like).bind(q.limit.unwrap_or(50).clamp(1, 200)).bind(q.offset.unwrap_or(0).max(0)).fetch_all(&state.db).await?;
    let mut total = 0i64;
    let mut data = Vec::new();
    for row in &rows {
        total = row.try_get("total")?;
        let s = <Subject as sqlx::FromRow<sqlx::postgres::PgRow>>::from_row(row)?;
        let pending: i64 = row.try_get("pending")?;
        data.push(json!({"id": s.id, "product_id": s.external_id, "name": s.name, "image_url": s.image_url, "url": s.url, "kind": s.kind, "summary": s.summary(), "pending": pending}));
    }
    Ok(json!({"data": data, "total": total}))
}

async fn products_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ProductsQuery>) -> AppResult<Json<Value>> {
    Ok(Json(products(&state, ctx.account_id, &q).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductBody {
    pub product_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// product (default), listing or service.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Name a thing to review that isn't in the catalog (a listing, a service), or set its kind.
pub async fn save_product(state: &AppState, ctx: &Ctx, b: ProductBody) -> AppResult<Value> {
    if b.kind.as_deref().is_some_and(|k| !["product", "listing", "service"].contains(&k)) {
        return Err(AppError::bad("kind must be product, listing or service"));
    }
    let s = store::subject_for(&state.db, ctx.account_id, &b.product_id, &store::Hint { name: b.name, image_url: b.image_url, url: b.url, kind: b.kind, trusted: true }).await?;
    audit(state, ctx, "review_product.save", Some(&s.id.to_string()), json!({"product_id": s.external_id})).await;
    Ok(json!({"id": s.id, "product_id": s.external_id, "name": s.name, "image_url": s.image_url, "url": s.url, "kind": s.kind, "summary": s.summary()}))
}

async fn save_product_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ProductBody>) -> AppResult<Json<Value>> {
    Ok(Json(save_product(&state, &ctx, b).await?))
}

pub async fn rebuild(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    crate::jobs::enqueue_background(&state.db, "reviews_rebuild", json!({"account_id": ctx.account_id}), None).await?;
    audit(state, ctx, "reviews.rebuild", None, json!({})).await;
    Ok(json!({"ok": true, "queued": true, "note": "Star summaries are being recounted from the published reviews in the background."}))
}

async fn rebuild_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(rebuild(&state, &ctx).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBody {
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub email: Option<String>,
    /// One product (or listing or service) by the store's id ...
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// ... or up to 6 items in the catalog's item shape.
    #[serde(default)]
    pub items: Option<Vec<Value>>,
    #[serde(default)]
    pub order_id: Option<String>,
    /// The purchase is real (default true: you're telling us through the API).
    #[serde(default)]
    pub verified: Option<bool>,
    /// "email" or "sms": also send the ask now (needs confirm).
    #[serde(default)]
    pub send: Option<String>,
    #[serde(default)]
    pub confirm: bool,
}

/// Review links for one person (made, or the open ones they have), optionally sent now.
pub async fn create_request(state: &AppState, ctx: &Ctx, b: RequestBody) -> AppResult<Value> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.account_id = $1 AND (p.id = $2 OR ($3::text IS NOT NULL AND p.email = $3)) LIMIT 1");
    let email = b.email.as_deref().map(|e| crate::models::normalize_email(e).ok_or_else(|| AppError::bad("email isn't valid"))).transpose()?;
    if b.profile_id.is_none() && email.is_none() {
        return Err(AppError::bad("say who to ask: profile_id or email"));
    }
    let person: Profile = sqlx::query_as(&q).bind(ctx.account_id).bind(b.profile_id.unwrap_or(Uuid::nil())).bind(&email).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("profile"))?;
    let mut props = json!({"order_id": b.order_id});
    match (&b.items, &b.product_id) {
        (Some(items), _) => {
            if items.is_empty() || items.len() > requests::MAX_ITEMS {
                return Err(AppError::bad(format!("give 1 to {} items", requests::MAX_ITEMS)));
            }
            props["items"] = json!(items);
        }
        (None, Some(p)) => props["items"] = json!([{"product_id": p, "name": b.name, "image_url": b.image_url, "url": b.url, "kind": b.kind}]),
        (None, None) => return Err(AppError::bad("say what to ask about: product_id or items")),
    }
    let items = requests::items_from_event(Some(&json!({"properties": props})));
    if items.is_empty() {
        return Err(AppError::bad("each item needs a product_id"));
    }
    let channel = match b.send.as_deref() {
        None | Some("") => None,
        Some(c @ ("email" | "sms")) => Some(c),
        Some(other) => return Err(AppError::bad(format!("send must be email or sms (got {other:?})"))),
    };
    if channel.is_some() {
        ctx.require(Perm::CampaignSend)?;
        if !b.confirm {
            return Ok(json!({"dry_run": true, "note": format!("Nothing made or sent. This makes review links for {} item(s) and sends {} the ask by {} now (only if they're subscribed). Call again with confirm=true once the user approves.",
                items.len(), person.email.as_deref().or(person.phone_number.as_deref()).unwrap_or("this person"), channel.unwrap_or("email"))}));
        }
    }
    let m = requests::Make { source: "api", verified: b.verified.unwrap_or(true), batch_id: Uuid::now_v7(), flow_run_id: None };
    let mut out = Vec::new();
    let mut made = Vec::new();
    for item in &items {
        match requests::make(state, &account, person.id, item, &m).await? {
            requests::Made::New(r) | requests::Made::Existing(r) => {
                out.push(json!({"request_id": r.id, "product_id": item.key, "url": requests::link(state, &account, r.id), "expires_at": r.expires_at, "verified": r.verified}));
                made.push(r);
            }
            requests::Made::Skipped(why) => out.push(json!({"product_id": item.key, "skipped": why})),
        }
    }
    let mut sent = Value::Null;
    if let (Some(ch), false) = (channel, made.is_empty()) {
        let o = requests::send_ask(state, &account, &person, ch, &made).await?;
        sent = json!(format!("{o:?}"));
        if matches!(o, crate::sending::Outcome::Sent(_)) {
            for r in &made {
                if let Ok(s) = store::subject_by_id(&state.db, r.subject_id).await {
                    requests::ready_event(state, r, &s).await?;
                }
            }
        }
    } else {
        for r in &made {
            if let Ok(s) = store::subject_by_id(&state.db, r.subject_id).await {
                requests::ready_event(state, r, &s).await?;
            }
        }
    }
    audit(state, ctx, "review_request.create", Some(&person.id.to_string()), json!({"items": items.len(), "made": made.len(), "send": channel})).await;
    Ok(json!({"profile_id": person.id, "requests": out, "sent": sent}))
}

async fn create_request_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<RequestBody>) -> AppResult<Json<Value>> {
    Ok(Json(create_request(&state, &ctx, b).await?))
}

#[derive(Deserialize)]
struct DaysQuery {
    days: Option<i64>,
}

async fn report_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<DaysQuery>) -> AppResult<Json<Value>> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    Ok(Json(report::report(&state.db, ctx.account_id, &tz(&account), q.days.unwrap_or(90)).await?))
}

pub async fn import(state: &AppState, ctx: &Ctx, b: io::ImportInput) -> AppResult<Value> {
    let confirm = b.confirm;
    let v = io::import(state, ctx.account_id, b).await?;
    if confirm {
        audit(state, ctx, "reviews.import", v["id"].as_str(), json!({"rows": v["total"]})).await;
    }
    Ok(v)
}

async fn import_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<io::ImportInput>) -> AppResult<Json<Value>> {
    Ok(Json(import(&state, &ctx, b).await?))
}

async fn get_import_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(io::get_import(&state, ctx.account_id, id).await?))
}

pub async fn export(state: &AppState, ctx: &Ctx, q: &store::ListQuery) -> AppResult<String> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let csv = io::export(state, ctx.account_id, &tz(&account), q, ctx.can(Perm::ProfilesRead)).await?;
    audit(state, ctx, "reviews.export", None, json!({"status": q.status, "product_id": q.product_id})).await;
    Ok(csv)
}

async fn export_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<store::ListQuery>) -> AppResult<axum::response::Response> {
    let csv = export(&state, &ctx, &q).await?;
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"reviews.csv\"")], Bytes::from(csv)).into_response())
}

pub async fn create_flow(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let f = requests::starter_flow(&state.db, ctx.account_id).await?;
    audit(state, ctx, "flow.create", Some(&f.id.to_string()), json!({"name": f.name, "from": "reviews"})).await;
    Ok(json!({"id": f.id, "name": f.name, "status": f.status, "trigger": f.trigger, "note": "Created as a draft. Check the emails, then turn it on in Flows."}))
}

async fn flow_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(create_flow(&state, &ctx).await?))
}

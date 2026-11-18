//! Public, read-only review endpoints for the website widgets (by site key), the widget script
//! itself, and the product's structured data.
//!
//! Only published reviews and their display names, dates, photos and replies leave here: never an
//! email address, a profile id or anything unpublished. Answers are cacheable (ETag, short
//! max-age), paged and bounded. Summaries come from the stored summary row, never a count.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::photos;
use super::store::{self, Review, Subject, SUBJECT_COLS};
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

/// The most pages a widget may ask for.
const MAX_PAGE: i64 = 500;

pub fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
    r
}

pub async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn account(state: &AppState, k: &str) -> AppResult<Account> {
    if k.is_empty() || k.len() > 100 {
        return Err(AppError::Unauthorized);
    }
    sqlx::query_as("SELECT * FROM accounts WHERE public_key = $1").bind(k).fetch_optional(&state.db).await?.ok_or(AppError::Unauthorized)
}

/// A JSON answer with an ETag of its bytes (304 when the browser has it) and a short public cache.
fn cached(headers: &HeaderMap, body: &Value, content_type: &'static str) -> Response {
    let bytes = body.to_string();
    let tag = format!("\"{}\"", &hex::encode(Sha256::digest(bytes.as_bytes()))[..32]);
    let same = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/") == tag));
    let mut r = if same { StatusCode::NOT_MODIFIED.into_response() } else { ([(header::CONTENT_TYPE, content_type)], bytes).into_response() };
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
    cors(r)
}

fn fail(e: AppError) -> Response {
    cors(e.into_response())
}

fn date_in(account: &Account, at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&crate::sending::account_tz(account)).format("%Y-%m-%d").to_string()
}

/// A published review as the public sees it.
pub fn public_review(state: &AppState, account: &Account, r: &Review, photo_ids: &[(Uuid, i32, i32)], subject: Option<&Subject>) -> Value {
    let base = crate::domains::link_base(state, account);
    let mut v = json!({
        "id": r.id, "rating": r.rating, "title": r.title, "body": r.body, "name": r.display_name, "date": date_in(account, r.reviewed_at),
        "verified": r.verified, "incentivized": r.incentivized, "recommend": r.recommend, "featured": r.featured,
        "answers": r.answers.as_array().into_iter().flatten().map(|a| json!({"label": a["label"], "answer": a["answer"]})).collect::<Vec<_>>(),
        "photos": photo_ids.iter().map(|(id, w, h)| json!({"url": photos::public_url(&base, *id), "width": w, "height": h})).collect::<Vec<_>>(),
        "reply": r.reply.as_ref().map(|t| json!({"text": t, "date": r.replied_at.map(|d| date_in(account, d))})),
    });
    if let Some(s) = subject {
        v["product"] = s.public_card();
    }
    v
}

async fn published(state: &AppState, account: &Account, where_sql: &str, binds: (Option<Uuid>, Option<i16>, bool), order: &str, limit: i64, offset: i64) -> AppResult<Vec<Review>> {
    let q = format!(
        "SELECT {} FROM reviews r WHERE r.account_id = $1 AND r.status = 'published' {where_sql}
           AND ($2::uuid IS NULL OR r.subject_id = $2) AND ($3::smallint IS NULL OR r.rating = $3)
           AND (NOT $4 OR EXISTS (SELECT 1 FROM review_photos ph WHERE ph.review_id = r.id))
         ORDER BY {order} LIMIT $5 OFFSET $6",
        store::REVIEW_COLS
    );
    Ok(sqlx::query_as(&q).bind(account.id).bind(binds.0).bind(binds.1).bind(binds.2).bind(limit).bind(offset).fetch_all(&state.db).await?)
}

fn order_for(sort: Option<&str>) -> &'static str {
    match sort {
        Some("highest") => "r.rating DESC, r.reviewed_at DESC, r.id",
        Some("lowest") => "r.rating ASC, r.reviewed_at DESC, r.id",
        Some("photos") => "(EXISTS (SELECT 1 FROM review_photos ph WHERE ph.review_id = r.id)) DESC, r.reviewed_at DESC, r.id",
        _ => "r.featured DESC, r.reviewed_at DESC, r.id",
    }
}

fn per_page(s: &store::Settings) -> i64 {
    s.widget["per_page"].as_i64().unwrap_or(5).clamp(3, 20)
}

fn widget_settings(account: &Account, s: &store::Settings) -> Value {
    json!({
        "heading": s.widget["heading"].as_str().filter(|h| !h.is_empty()).unwrap_or("Customer reviews"),
        "star_color": s.widget["star_color"].as_str().filter(|c| !c.is_empty()).unwrap_or("#f59e0b"),
        "text_color": s.widget["text_color"].as_str().filter(|c| !c.is_empty()).unwrap_or("#111827"),
        "accent_color": s.widget["accent_color"].as_str().filter(|c| !c.is_empty()).map(String::from).unwrap_or_else(|| crate::render::safe_color(&account.brand_color, "#1f6f5c")),
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    k: String,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    rating: Option<i16>,
    #[serde(default)]
    photos: Option<bool>,
    #[serde(default)]
    page: Option<i64>,
}

/// One product's published reviews with its summary.
pub async fn product_reviews(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    let res: AppResult<Value> = async {
        let account = account(&state, &q.k).await?;
        let key = q.product_id.as_deref().and_then(store::valid_key).ok_or_else(|| AppError::bad("product_id is missing"))?;
        let s = store::settings(&state.db, account.id).await?;
        let page = q.page.unwrap_or(1).clamp(1, MAX_PAGE);
        let per = per_page(&s);
        let subject = store::find_subject(&state.db, account.id, &key).await?;
        let questions: Vec<Value> = s.questions.as_array().cloned().unwrap_or_default();
        let base = json!({"widget": widget_settings(&account, &s), "can_write": s.public_reviews, "questions": questions, "page": page, "per_page": per});
        let Some(subject) = subject else {
            let mut v = base;
            v["product"] = json!({"product_id": key});
            v["summary"] = json!({"average": null, "count": 0, "histogram": {"5": 0, "4": 0, "3": 0, "2": 0, "1": 0}});
            v["reviews"] = json!([]);
            v["pages"] = json!(0);
            return Ok(v);
        };
        let rows = published(&state, &account, "", (Some(subject.id), q.rating.filter(|r| (1..=5).contains(r)), q.photos == Some(true)), order_for(q.sort.as_deref()), per, (page - 1) * per).await?;
        let photos = store::photos_of(&state.db, &rows.iter().map(|r| r.id).collect::<Vec<_>>()).await?;
        let filtered_total = match (q.rating.filter(|r| (1..=5).contains(r)), q.photos == Some(true)) {
            (Some(r), false) => i64::from(subject.rating_counts.get(r as usize - 1).copied().unwrap_or(0)),
            (None, false) => i64::from(subject.review_count),
            _ => sqlx::query_scalar(
                "SELECT count(*) FROM reviews r WHERE r.subject_id = $1 AND r.status = 'published' AND ($2::smallint IS NULL OR r.rating = $2)
                   AND EXISTS (SELECT 1 FROM review_photos ph WHERE ph.review_id = r.id)",
            )
            .bind(subject.id)
            .bind(q.rating)
            .fetch_one(&state.db)
            .await?,
        };
        let mut v = base;
        v["product"] = subject.public_card();
        v["summary"] = subject.summary();
        v["reviews"] = json!(rows.iter().map(|r| public_review(&state, &account, r, photos.get(&r.id).map(|p| p.as_slice()).unwrap_or(&[]), None)).collect::<Vec<_>>());
        v["pages"] = json!(((filtered_total + per - 1) / per).min(MAX_PAGE));
        Ok(v)
    }
    .await;
    match res {
        Ok(v) => cached(&headers, &v, "application/json"),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    k: String,
    ids: String,
}

/// Star badges for product cards: average and count for up to 50 products at once.
pub async fn summaries(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<SummaryQuery>) -> Response {
    let res: AppResult<Value> = async {
        let account = account(&state, &q.k).await?;
        let ids: Vec<String> = q.ids.split(',').filter_map(store::valid_key).take(50).collect();
        let sql = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.account_id = $1 AND s.external_id = ANY($2)");
        let rows: Vec<Subject> = sqlx::query_as(&sql).bind(account.id).bind(&ids).fetch_all(&state.db).await?;
        let mut data = serde_json::Map::new();
        for s in rows {
            data.insert(s.external_id.clone(), json!({"average": s.average(), "count": s.review_count}));
        }
        Ok(json!({"data": data, "widget": widget_settings(&account, &store::settings(&state.db, account.id).await?)}))
    }
    .await;
    match res {
        Ok(v) => cached(&headers, &v, "application/json"),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct AllQuery {
    k: String,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    featured: Option<bool>,
    #[serde(default)]
    limit: Option<i64>,
}

/// Every product's published reviews (the all-reviews page), or the featured ones (the carousel;
/// the newest when none are featured).
pub async fn all_reviews(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<AllQuery>) -> Response {
    let res: AppResult<Value> = async {
        let account = account(&state, &q.k).await?;
        let s = store::settings(&state.db, account.id).await?;
        let page = q.page.unwrap_or(1).clamp(1, MAX_PAGE);
        let per = q.limit.unwrap_or(per_page(&s)).clamp(1, 20);
        let featured = q.featured == Some(true);
        let mut rows = if featured { published(&state, &account, "AND r.featured", (None, None, false), "r.reviewed_at DESC, r.id", per, 0).await? } else { Vec::new() };
        if !featured || rows.is_empty() {
            rows = published(&state, &account, "", (None, None, false), order_for(q.sort.as_deref()), per, if featured { 0 } else { (page - 1) * per }).await?;
        }
        let photos = store::photos_of(&state.db, &rows.iter().map(|r| r.id).collect::<Vec<_>>()).await?;
        let sids: Vec<Uuid> = rows.iter().map(|r| r.subject_id).collect();
        let sql = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.id = ANY($1)");
        let subjects: Vec<Subject> = sqlx::query_as(&sql).bind(&sids).fetch_all(&state.db).await?;
        let (count, sum): (i64, i64) = sqlx::query_as("SELECT COALESCE(sum(review_count), 0)::bigint, COALESCE(sum(rating_sum), 0)::bigint FROM review_subjects WHERE account_id = $1")
            .bind(account.id)
            .fetch_one(&state.db)
            .await?;
        let list: Vec<Value> = rows.iter().map(|r| public_review(&state, &account, r, photos.get(&r.id).map(|p| p.as_slice()).unwrap_or(&[]), subjects.iter().find(|x| x.id == r.subject_id))).collect();
        Ok(json!({"widget": widget_settings(&account, &s), "reviews": list, "page": page, "per_page": per,
            "pages": if featured { 1 } else { ((count + per - 1) / per).min(MAX_PAGE) },
            "summary": {"count": count, "average": (count > 0).then(|| (sum as f64 / count as f64 * 10.0).round() / 10.0)}}))
    }
    .await;
    match res {
        Ok(v) => cached(&headers, &v, "application/json"),
        Err(e) => fail(e),
    }
}

/// Google's review snippet markup for one product: a `Product` with its `AggregateRating` and up
/// to 10 `Review`s, from published reviews only. Products only: Google doesn't show stars for a
/// business's reviews of itself (LocalBusiness, Organization), so listings and services get none.
pub async fn json_ld(state: &AppState, account: &Account, key: &str) -> AppResult<Option<Value>> {
    let Some(s) = store::find_subject(&state.db, account.id, key).await? else { return Ok(None) };
    if s.kind != "product" || s.review_count == 0 || !s.trusted {
        return Ok(None);
    }
    let rows = published(state, account, "", (Some(s.id), None, false), "r.featured DESC, r.reviewed_at DESC, r.id", 10, 0).await?;
    let reviews: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({"@type": "Review", "author": {"@type": "Person", "name": r.display_name.chars().take(99).collect::<String>()},
                "datePublished": date_in(account, r.reviewed_at), "reviewRating": {"@type": "Rating", "ratingValue": r.rating, "bestRating": 5, "worstRating": 1},
                "name": (!r.title.is_empty()).then_some(&r.title), "reviewBody": (!r.body.is_empty()).then_some(&r.body)})
        })
        .collect();
    let mut v = json!({"@context": "https://schema.org", "@type": "Product", "name": s.name, "sku": s.external_id,
        "aggregateRating": {"@type": "AggregateRating", "ratingValue": s.average(), "reviewCount": s.review_count, "bestRating": 5, "worstRating": 1},
        "review": reviews});
    if let Some(i) = &s.image_url {
        v["image"] = json!(i);
    }
    if let Some(u) = &s.url {
        v["url"] = json!(u);
    }
    Ok(Some(v))
}

#[derive(Deserialize)]
pub struct LdQuery {
    k: String,
    product_id: String,
}

pub async fn json_ld_h(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<LdQuery>) -> Response {
    let res: AppResult<Value> = async {
        let account = account(&state, &q.k).await?;
        let key = store::valid_key(&q.product_id).ok_or_else(|| AppError::bad("product_id is missing"))?;
        json_ld(&state, &account, &key).await?.ok_or_else(|| AppError::not_found("reviews for this product"))
    }
    .await;
    match res {
        Ok(v) => cached(&headers, &v, "application/ld+json"),
        Err(e) => fail(e),
    }
}

const WIDGET_JS: &str = include_str!("widget.js");

#[derive(Deserialize)]
pub struct KeyQuery {
    k: String,
}

/// The widget script. The website script loads it when a page has a review widget, so sites never
/// add a second script tag.
pub async fn widget_js(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<KeyQuery>) -> Response {
    let base = state.cfg.public_base_for(&crate::site::request_host(&headers));
    let js = WIDGET_JS.replacen("__BASE__", &serde_json::to_string(base).unwrap_or_default(), 1).replacen("__KEY__", &serde_json::to_string(&q.k).unwrap_or_default(), 1);
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], js).into_response()
}

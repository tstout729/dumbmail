//! Back-in-stock and price-drop alerts: settings, the jobs that emit "Back in Stock" and
//! "Price Dropped" events, and the public endpoints behind the on-site "Notify me" widget.
//!
//! These only record events. Flows triggered by those events do the sending, so the normal
//! consent, suppression, billing and pause rules apply to every message.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{self, Change, Product, PRODUCT_COLS};
use crate::audience::{self, record_event, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const SUBSCRIBED_METRIC: &str = "Subscribed to Back in Stock";
pub const BACK_IN_STOCK_METRIC: &str = "Back in Stock";
pub const PRICE_DROP_METRIC: &str = "Price Dropped";
/// People told per job run; the job queues itself again for the rest.
const BATCH: i64 = 200;
/// Most products one person can wait on at once.
const MAX_WAITING_PER_PERSON: i64 = 25;
/// Back-in-stock requests one visitor (IP address) can make per hour.
const MAX_PER_VISITOR_HOUR: i64 = 30;
/// New back-in-stock requests an account takes per hour, whatever addresses they come from. High
/// enough that a real store never meets it; it only stops a flood.
const MAX_NEW_PER_ACCOUNT_HOUR: i64 = 5000;
/// Requests an account takes per hour from visitors it can't vouch for. Each one sends an unbilled
/// confirmation email to an address a stranger typed, so this matches the double opt-in limit.
const MAX_UNCONFIRMED_PER_ACCOUNT_HOUR: i64 = 200;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Settings {
    pub learn_from_events: bool,
    pub back_in_stock_enabled: bool,
    pub back_in_stock_limit: Option<i32>,
    pub price_drop_enabled: bool,
    pub price_drop_percent: Option<f64>,
    pub price_drop_amount: Option<f64>,
    pub price_drop_lookback_days: i32,
    pub price_drop_cooldown_days: i32,
    pub widget: Value,
}

pub async fn settings(db: &PgPool, account_id: Uuid) -> AppResult<Settings> {
    sqlx::query("INSERT INTO catalog_settings (account_id) VALUES ($1) ON CONFLICT DO NOTHING").bind(account_id).execute(db).await?;
    Ok(sqlx::query_as(
        "SELECT learn_from_events, back_in_stock_enabled, back_in_stock_limit, price_drop_enabled, price_drop_percent, price_drop_amount,
                price_drop_lookback_days, price_drop_cooldown_days, widget
         FROM catalog_settings WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?)
}

fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Ok(Some(Option::<T>::deserialize(d)?))
}

/// A change to the settings. Fields left out stay; `null` clears the nullable ones.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub learn_from_events: Option<bool>,
    pub back_in_stock_enabled: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    pub back_in_stock_limit: Option<Option<i32>>,
    pub price_drop_enabled: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    pub price_drop_percent: Option<Option<f64>>,
    #[serde(default, deserialize_with = "nullable")]
    pub price_drop_amount: Option<Option<f64>>,
    pub price_drop_lookback_days: Option<i32>,
    pub price_drop_cooldown_days: Option<i32>,
    pub widget: Option<Value>,
}

fn check_widget(w: &Value) -> AppResult<Value> {
    let o = w.as_object().ok_or_else(|| AppError::bad("widget must be an object"))?;
    let mut out = serde_json::Map::new();
    for (k, v) in o {
        let max = match k.as_str() {
            "button_text" => 40,
            "heading" => 100,
            "success_message" => 160,
            "color" | "text_color" => {
                let c = v.as_str().unwrap_or("");
                if !c.is_empty() && crate::render::safe_color(c, "") != c {
                    return Err(AppError::bad(format!("widget {k} must be a hex color like #111827")));
                }
                out.insert(k.clone(), json!(c));
                continue;
            }
            other => return Err(AppError::bad(format!("unknown widget setting {other:?} (use button_text, heading, success_message, color, text_color)"))),
        };
        let s = v.as_str().unwrap_or("").trim();
        if s.chars().count() > max {
            return Err(AppError::bad(format!("widget {k} must be at most {max} characters")));
        }
        out.insert(k.clone(), json!(s));
    }
    Ok(Value::Object(out))
}

pub async fn update_settings(db: &PgPool, account_id: Uuid, p: SettingsPatch) -> AppResult<Settings> {
    let cur = settings(db, account_id).await?;
    let limit = p.back_in_stock_limit.unwrap_or(cur.back_in_stock_limit);
    let percent = p.price_drop_percent.unwrap_or(cur.price_drop_percent);
    let amount = p.price_drop_amount.unwrap_or(cur.price_drop_amount);
    let lookback = p.price_drop_lookback_days.unwrap_or(cur.price_drop_lookback_days);
    let cooldown = p.price_drop_cooldown_days.unwrap_or(cur.price_drop_cooldown_days);
    let drop_on = p.price_drop_enabled.unwrap_or(cur.price_drop_enabled);
    if limit.is_some_and(|l| !(1..=100_000).contains(&l)) {
        return Err(AppError::bad("the restock limit must be between 1 and 100,000 people (or empty for everyone)"));
    }
    if percent.is_some_and(|x| !(x.is_finite() && (1.0..=90.0).contains(&x))) {
        return Err(AppError::bad("the price drop percent must be between 1 and 90"));
    }
    if amount.is_some_and(|x| !(x.is_finite() && x > 0.0 && x <= 1_000_000.0)) {
        return Err(AppError::bad("the price drop amount must be more than 0"));
    }
    if drop_on && percent.is_none() && amount.is_none() {
        return Err(AppError::bad("set a percent or an amount for price drops (or turn them off)"));
    }
    if !(1..=90).contains(&lookback) {
        return Err(AppError::bad("look back between 1 and 90 days for people who viewed the product"));
    }
    if !(1..=365).contains(&cooldown) {
        return Err(AppError::bad("wait between 1 and 365 days before telling someone about the same product again"));
    }
    let widget = match &p.widget {
        Some(w) => check_widget(w)?,
        None => cur.widget.clone(),
    };
    sqlx::query(
        "UPDATE catalog_settings SET learn_from_events = $2, back_in_stock_enabled = $3, back_in_stock_limit = $4, price_drop_enabled = $5,
            price_drop_percent = $6, price_drop_amount = $7, price_drop_lookback_days = $8, price_drop_cooldown_days = $9, widget = $10, updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .bind(p.learn_from_events.unwrap_or(cur.learn_from_events))
    .bind(p.back_in_stock_enabled.unwrap_or(cur.back_in_stock_enabled))
    .bind(limit)
    .bind(drop_on)
    .bind(percent)
    .bind(amount)
    .bind(lookback)
    .bind(cooldown)
    .bind(&widget)
    .execute(db)
    .await?;
    settings(db, account_id).await
}

/// A drop counts when it meets every threshold that's set (percent and/or amount).
pub fn drop_qualifies(s: &Settings, old: f64, new: f64) -> bool {
    if !(old > 0.0 && new < old) || (s.price_drop_percent.is_none() && s.price_drop_amount.is_none()) {
        return false;
    }
    let pct_ok = s.price_drop_percent.is_none_or(|p| (old - new) / old * 100.0 + 1e-9 >= p);
    let amt_ok = s.price_drop_amount.is_none_or(|a| old - new + 1e-9 >= a);
    pct_ok && amt_ok
}

/// Queue the job for a restock or price drop, inside the transaction that saved the change, so the
/// change and its alert are saved together. A restock nobody waits for queues nothing.
pub async fn enqueue_change(conn: &mut sqlx::PgConnection, account_id: Uuid, ch: &Change) -> sqlx::Result<()> {
    match ch {
        Change::Restock { product_id, variant_id } => {
            // A request racing this change waits on the product row this transaction holds, so it's
            // either seen here or sees the product in stock and isn't taken.
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM catalog_stock_subscriptions WHERE product_id = $1 AND notified_at IS NULL AND confirmed_at IS NOT NULL
                                AND (variant_id IS NULL OR variant_id = $2))",
            )
            .bind(product_id)
            .bind(variant_id)
            .fetch_one(&mut *conn)
            .await?;
            if !waiting {
                return Ok(());
            }
            let cap: Option<i32> = sqlx::query_scalar("SELECT back_in_stock_limit FROM catalog_settings WHERE account_id = $1")
                .bind(account_id)
                .fetch_optional(&mut *conn)
                .await?
                .flatten();
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO catalog_restocks (id, account_id, product_id, variant_id, cap) VALUES ($1, $2, $3, $4, $5)")
                .bind(id)
                .bind(account_id)
                .bind(product_id)
                .bind(variant_id)
                .bind(cap)
                .execute(&mut *conn)
                .await?;
            crate::jobs::enqueue(&mut *conn, "catalog_back_in_stock", json!({"restock_id": id}), None).await?;
        }
        Change::PriceDrop { product_id, old, new } => {
            let payload = json!({"account_id": account_id, "product_id": product_id, "old": old, "new": new, "drop_id": Uuid::now_v7(), "after": null});
            crate::jobs::enqueue(&mut *conn, "catalog_price_drop", payload, None).await?;
        }
    }
    Ok(())
}

async fn product_by_id(db: &PgPool, id: Uuid) -> AppResult<Option<Product>> {
    let q = format!("SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.id = $1");
    Ok(sqlx::query_as::<_, Product>(&q).bind(id).fetch_optional(db).await?)
}

/// What a Back in Stock / Price Dropped / Subscribed event says about the product, named like
/// the website's Viewed Product events so the same templates work, plus `items` for product blocks.
fn product_props(p: &Product, variant: Option<&store::Variant>) -> Value {
    let item = json!({
        "name": p.title, "product_id": p.external_id, "variant_id": variant.map(|v| v.external_id.clone()),
        "variant": variant.and_then(|v| v.title.clone()), "sku": variant.and_then(|v| v.sku.clone()),
        "price": variant.and_then(|v| v.price).or(p.price), "image_url": variant.and_then(|v| v.image_url.clone()).or_else(|| p.image_url.clone()),
        "url": p.url, "brand": p.brand, "categories": p.categories, "currency": p.currency,
    });
    let mut props = item.clone();
    props["items"] = json!([item]);
    props
}

fn err(e: AppError) -> anyhow::Error {
    anyhow::anyhow!(e.public_message())
}

#[derive(Deserialize)]
struct RestockJob {
    restock_id: Uuid,
}

/// Job: tell the people waiting on a restocked product, oldest request first, in batches.
///
/// Each batch is claimed in one transaction against the restock's budget (stored on the restock, so
/// retries and parallel runs can't exceed it), then told. A retry first finishes its own claims; the
/// event's unique id stops anyone being told twice.
pub async fn run_back_in_stock(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let j: RestockJob = serde_json::from_value(payload.clone())?;
    let row: Option<(Uuid, Uuid, Option<Uuid>, Option<i32>)> = sqlx::query_as("SELECT account_id, product_id, variant_id, cap FROM catalog_restocks WHERE id = $1")
        .bind(j.restock_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((account_id, product_id, variant_id, cap)) = row else { return Ok(()) };
    let release = || async {
        sqlx::query("UPDATE catalog_stock_subscriptions SET restock_id = NULL, claimed_at = NULL WHERE restock_id = $1 AND notified_at IS NULL")
            .bind(j.restock_id)
            .execute(&state.db)
            .await
    };
    let s = settings(&state.db, account_id).await.map_err(err)?;
    let Some(p) = product_by_id(&state.db, product_id).await.map_err(err)? else { return Ok(()) };
    let variants = store::variants(&state.db, p.id).await.map_err(err)?;
    let variant = variant_id.and_then(|v| variants.iter().find(|x| x.id == v));
    // Sold out again (or hidden, or alerts turned off) before the job ran: nobody to tell this time.
    let still_in = match (variant_id, variant) {
        (Some(_), Some(v)) => v.in_stock,
        (Some(_), None) => false,
        (None, _) => p.in_stock,
    };
    if !s.back_in_stock_enabled || !p.published || !still_in {
        release().await?;
        return Ok(());
    }

    let mut tx = state.db.begin().await?;
    let claimed: i32 = sqlx::query_scalar("SELECT claimed FROM catalog_restocks WHERE id = $1 FOR UPDATE").bind(j.restock_id).fetch_one(&mut *tx).await?;
    let room = cap.map(|c| i64::from(c - claimed)).unwrap_or(i64::MAX).max(0);
    let take = room.min(BATCH);
    // A request for one variant waits for that variant; a request for the product takes any. People
    // who unsubscribed or whose address is suppressed aren't told (and don't use up the budget).
    let new: Vec<Uuid> = if take > 0 {
        sqlx::query_scalar(
            "UPDATE catalog_stock_subscriptions SET restock_id = $1, claimed_at = now() WHERE id IN (
                SELECT c.id FROM catalog_stock_subscriptions c JOIN profiles pr ON pr.id = c.profile_id
                WHERE c.product_id = $2 AND c.notified_at IS NULL AND c.confirmed_at IS NOT NULL AND (c.variant_id IS NULL OR c.variant_id = $3)
                  AND (c.restock_id IS NULL OR (c.restock_id <> $1 AND c.claimed_at < now() - interval '1 hour'))
                  AND pr.email_consent <> 'unsubscribed' AND pr.email_suppressed_reason IS NULL
                ORDER BY c.created_at, c.id LIMIT $4 FOR UPDATE OF c SKIP LOCKED)
             RETURNING id",
        )
        .bind(j.restock_id)
        .bind(p.id)
        .bind(variant_id)
        .bind(take)
        .fetch_all(&mut *tx)
        .await?
    } else {
        Vec::new()
    };
    sqlx::query("UPDATE catalog_restocks SET claimed = claimed + $2 WHERE id = $1").bind(j.restock_id).bind(new.len() as i32).execute(&mut *tx).await?;
    tx.commit().await?;

    let subs: Vec<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, profile_id, variant_id FROM catalog_stock_subscriptions WHERE restock_id = $1 AND notified_at IS NULL ORDER BY created_at, id",
    )
    .bind(j.restock_id)
    .fetch_all(&state.db)
    .await?;
    for (sub_id, profile_id, sub_variant) in &subs {
        let v = sub_variant.and_then(|id| variants.iter().find(|x| x.id == id)).or(variant);
        let mut props = product_props(&p, v);
        props["subscription_id"] = json!(sub_id);
        record_event(
            &state.db,
            NewEvent {
                account_id,
                profile_id: *profile_id,
                metric: BACK_IN_STOCK_METRIC,
                properties: props,
                value: None,
                occurred_at: None,
                // A retried job must not tell anyone twice.
                unique_id: Some(format!("bis:{sub_id}")),
                message_id: None,
            },
        )
        .await
        .map_err(err)?;
        sqlx::query("UPDATE catalog_stock_subscriptions SET notified_at = now() WHERE id = $1 AND restock_id = $2").bind(sub_id).bind(j.restock_id).execute(&state.db).await?;
    }
    if new.len() as i64 == take && take > 0 && room > take {
        crate::jobs::enqueue(&state.db, "catalog_back_in_stock", payload.clone(), None).await?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct DropJob {
    account_id: Uuid,
    product_id: Uuid,
    old: f64,
    new: f64,
    drop_id: Uuid,
    after: Option<Uuid>,
}

/// Job: a product's price fell. Tell people who looked at it or carted it recently, didn't buy it
/// and are subscribed to email marketing, at most once per product per person in the cooldown.
///
/// Each person is claimed atomically (the cooldown row is written with a check), so two price
/// drops handled at once can't both tell the same person. A retry may re-claim its own people.
pub async fn run_price_drop(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let j: DropJob = serde_json::from_value(payload.clone())?;
    let s = settings(&state.db, j.account_id).await.map_err(err)?;
    if !s.price_drop_enabled || !drop_qualifies(&s, j.old, j.new) {
        return Ok(());
    }
    let Some(p) = product_by_id(&state.db, j.product_id).await.map_err(err)? else { return Ok(()) };
    // Back up since (or sold out, or hidden): skip.
    if !p.published || !p.in_stock || p.price.is_none_or(|now| now > j.new + 1e-9) {
        return Ok(());
    }
    let key = j.drop_id.to_string();
    // A price drop is marketing: only people subscribed to email (and not suppressed) qualify.
    let people: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT ci.profile_id FROM catalog_interactions ci JOIN profiles pr ON pr.id = ci.profile_id
         WHERE ci.product_id = $1 AND ci.kind IN ('viewed', 'carted', 'checkout')
           AND ci.occurred_at > now() - make_interval(days => $2)
           AND ($3::uuid IS NULL OR ci.profile_id > $3)
           AND pr.email_consent = 'subscribed' AND pr.email_suppressed_reason IS NULL AND pr.email IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM catalog_interactions o WHERE o.profile_id = ci.profile_id AND o.product_id = ci.product_id
                           AND o.kind = 'ordered' AND o.occurred_at > now() - make_interval(days => $2))
           AND NOT EXISTS (SELECT 1 FROM catalog_price_drop_notices n WHERE n.profile_id = ci.profile_id AND n.product_id = ci.product_id
                           AND n.notified_at > now() - make_interval(days => $4) AND n.drop_key IS DISTINCT FROM $5)
         ORDER BY ci.profile_id LIMIT $6",
    )
    .bind(p.id)
    .bind(s.price_drop_lookback_days)
    .bind(j.after)
    .bind(s.price_drop_cooldown_days)
    .bind(&key)
    .bind(BATCH)
    .fetch_all(&state.db)
    .await?;
    let claimed = claim_price_drop(&state.db, p.id, &key, s.price_drop_cooldown_days, &people).await?;
    for pid in &claimed {
        let mut props = product_props(&p, None);
        props["old_price"] = json!(j.old);
        props["new_price"] = json!(j.new);
        props["price"] = json!(j.new);
        props["drop_amount"] = json!(((j.old - j.new) * 100.0).round() / 100.0);
        props["drop_percent"] = json!(((j.old - j.new) / j.old * 1000.0).round() / 10.0);
        props["old_price_text"] = json!(store::money(j.old, p.currency.as_deref()));
        props["new_price_text"] = json!(store::money(j.new, p.currency.as_deref()));
        record_event(
            &state.db,
            NewEvent {
                account_id: j.account_id,
                profile_id: *pid,
                metric: PRICE_DROP_METRIC,
                properties: props,
                value: None,
                occurred_at: None,
                unique_id: Some(format!("pd:{}:{}", j.drop_id.simple(), pid.simple())),
                message_id: None,
            },
        )
        .await
        .map_err(err)?;
    }
    if people.len() as i64 == BATCH {
        let mut next = payload.clone();
        next["after"] = json!(people.last());
        crate::jobs::enqueue(&state.db, "catalog_price_drop", next, None).await?;
    }
    Ok(())
}

/// Claim `people` for the price drop `key` on a product: only those not told about it within
/// `cooldown_days` (or already claimed by this same drop, for a retry). The cooldown row is written
/// with the check, so two drops can't both claim a person.
pub async fn claim_price_drop(db: &PgPool, product_id: Uuid, key: &str, cooldown_days: i32, people: &[Uuid]) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "INSERT INTO catalog_price_drop_notices (profile_id, product_id, notified_at, drop_key) SELECT pid, $2, now(), $3 FROM unnest($1::uuid[]) pid
         ON CONFLICT (profile_id, product_id) DO UPDATE SET notified_at = now(), drop_key = EXCLUDED.drop_key
         WHERE catalog_price_drop_notices.notified_at <= now() - make_interval(days => $4) OR catalog_price_drop_notices.drop_key = EXCLUDED.drop_key
         RETURNING profile_id",
    )
    .bind(people)
    .bind(product_id)
    .bind(key)
    .bind(cooldown_days)
    .fetch_all(db)
    .await
}

// ---------- Public endpoints for the on-site widget ----------

#[derive(Deserialize)]
pub struct KeyQuery {
    k: String,
}

async fn account_by_key(db: &PgPool, k: &str) -> AppResult<Uuid> {
    sqlx::query_scalar("SELECT id FROM accounts WHERE public_key = $1").bind(k).fetch_optional(db).await?.ok_or(AppError::Unauthorized)
}

pub fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS".parse().expect("static header"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type".parse().expect("static header"));
    r
}

pub async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

/// The published product a page is about: by the store's product id, a variant id or SKU, or the page URL.
async fn locate(db: &PgPool, account_id: Uuid, product_id: Option<&str>, variant_id: Option<&str>, url: Option<&str>) -> AppResult<Option<(Product, Option<store::Variant>)>> {
    let clean = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty() && s.len() <= 500).map(String::from);
    let (pid, vid, url) = (clean(product_id), clean(variant_id), clean(url).map(|u| u.split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_string()));
    let q = format!(
        "SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND (
            cp.external_id = $2
            OR EXISTS (SELECT 1 FROM catalog_variants v WHERE v.product_id = cp.id AND (v.external_id = $3 OR v.sku = $3 OR v.sku = $2))
            OR ($4::text <> '' AND rtrim(split_part(cp.url, '?', 1), '/') = $4))
         ORDER BY (cp.external_id = $2) DESC NULLS LAST LIMIT 1"
    );
    let Some(p) = sqlx::query_as::<_, Product>(&q).bind(account_id).bind(&pid).bind(&vid).bind(url.unwrap_or_default()).fetch_optional(db).await? else {
        return Ok(None);
    };
    let vs = store::variants(db, p.id).await?;
    let v = vid.as_deref().and_then(|id| vs.into_iter().find(|v| v.external_id == id || v.sku.as_deref() == Some(id)));
    Ok(Some((p, v)))
}

#[derive(Deserialize)]
pub struct StockQuery {
    k: String,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(default)]
    variant_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Whether the product on this page is sold out, so the widget knows to show itself.
pub async fn stock_status(State(state): State<AppState>, Query(q): Query<StockQuery>) -> Response {
    let res: AppResult<Value> = async {
        let account_id = account_by_key(&state.db, &q.k).await?;
        let s = settings(&state.db, account_id).await?;
        let Some((p, v)) = locate(&state.db, account_id, q.product_id.as_deref(), q.variant_id.as_deref(), q.url.as_deref()).await? else {
            return Ok(json!({"found": false, "enabled": s.back_in_stock_enabled}));
        };
        let vs = store::variants(&state.db, p.id).await?;
        Ok(json!({
            "found": true, "enabled": s.back_in_stock_enabled, "widget": s.widget,
            "product_id": p.external_id, "title": p.title, "in_stock": p.in_stock,
            "variant": v.map(|v| json!({"id": v.external_id, "title": v.title, "in_stock": v.in_stock})),
            "variants": vs.iter().take(100).map(|v| json!({"id": v.external_id, "title": v.title, "in_stock": v.in_stock})).collect::<Vec<_>>(),
        }))
    }
    .await;
    let mut r = cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    });
    r.headers_mut().insert(header::CACHE_CONTROL, "public, max-age=30".parse().expect("static header"));
    r
}

#[derive(Deserialize)]
struct SubscribeBody {
    #[serde(default)]
    email: Option<String>,
    /// The visitor's link token from one of the store's emails (the site snippet keeps it), which
    /// proves the request comes from the person who owns that address.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(default)]
    variant_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// How long a confirmation link works, and how long an unconfirmed request is kept.
pub const CONFIRM_DAYS: i32 = 7;
/// Most confirmation emails one inbox gets from an account in a day.
const MAX_CONFIRMS_PER_INBOX_DAY: i64 = 3;

/// Someone asked to be told when a sold-out product (or one variant) is back.
pub struct Request<'a> {
    pub email: Option<&'a str>,
    /// The profile a valid link token names (the visitor came from one of the store's emails).
    pub known_profile: Option<Uuid>,
    pub product_id: Option<&'a str>,
    pub variant_id: Option<&'a str>,
    pub url: Option<&'a str>,
    pub ip: Option<&'a str>,
}

/// Requests from one visitor (by hashed address) in the last hour, across accounts.
async fn from_ip(conn: &mut sqlx::PgConnection, ip_hash: Option<&str>) -> sqlx::Result<i64> {
    let Some(h) = ip_hash else { return Ok(0) };
    sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions WHERE ip_hash = $1 AND created_at > now() - interval '1 hour'").bind(h).fetch_one(conn).await
}

async fn lock(conn: &mut sqlx::PgConnection, key: &str) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(key).execute(conn).await.map(|_| ())
}

/// Record a back-in-stock request. A visitor the store already knows (their link token names the
/// profile with this address) is armed at once with a "Subscribed to Back in Stock" event. Anyone
/// else gets a confirmation email, and the request only counts once they tap it. It never changes
/// their email consent.
///
/// The per-account, per-visitor and per-person limits are checked and the request saved in one
/// transaction under locks on the account and the visitor, so parallel requests can't get past them.
pub async fn subscribe(state: &AppState, account: &crate::models::Account, r: &Request<'_>) -> AppResult<Value> {
    let db = &state.db;
    let account_id = account.id;
    let s = settings(db, account_id).await?;
    if !s.back_in_stock_enabled {
        return Err(AppError::Conflict("Back-in-stock alerts are turned off.".into()));
    }
    let known = match r.known_profile {
        Some(id) => sqlx::query_as::<_, (Uuid, Option<String>)>("SELECT id, email FROM profiles WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(db).await?,
        None => None,
    };
    let email = match r.email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => crate::models::normalize_email(e).ok_or_else(|| AppError::bad("enter a valid email"))?,
        None => known.as_ref().and_then(|k| k.1.clone()).ok_or_else(|| AppError::bad("enter your email"))?,
    };
    // Armed at once only for the address the token's profile already has.
    let trusted = known.as_ref().is_some_and(|k| k.1.as_deref() == Some(email.as_str()));
    let (p, v) = locate(db, account_id, r.product_id, r.variant_id, r.url).await?.ok_or_else(|| AppError::not_found("product"))?;
    if v.as_ref().map(|v| v.in_stock).unwrap_or(p.in_stock) {
        return Err(AppError::Conflict("Good news: it's in stock now.".into()));
    }
    let ip_hash = r.ip.filter(|s| !s.is_empty()).map(crate::auth::sha256_hex);
    // A quick look before creating a profile for the address; the locked check below is the one that counts.
    if from_ip(&mut *db.acquire().await?, ip_hash.as_deref()).await? >= MAX_PER_VISITOR_HOUR {
        return Err(AppError::Conflict("Too many requests from here. Please try again later.".into()));
    }
    let profile = audience::upsert_profile_public(db, account_id, &ProfileInput { email: Some(email.clone()), ..Default::default() }).await?.profile;

    let mut tx = db.begin().await?;
    lock(&mut tx, &format!("bis:{account_id}")).await?;
    if let Some(h) = &ip_hash {
        lock(&mut tx, &format!("bis-ip:{h}")).await?;
    }
    // Waits for a stock change being saved for this product, so this request is either seen by its restock or refused here.
    let now_in: bool = sqlx::query_scalar(
        "SELECT CASE WHEN $2::uuid IS NULL THEN p.in_stock ELSE COALESCE((SELECT v.in_stock FROM catalog_variants v WHERE v.id = $2), false) END
         FROM catalog_products p WHERE p.id = $1 FOR SHARE OF p",
    )
    .bind(p.id)
    .bind(v.as_ref().map(|v| v.id))
    .fetch_one(&mut *tx)
    .await?;
    if now_in {
        return Err(AppError::Conflict("Good news: it's in stock now.".into()));
    }
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions WHERE account_id = $1 AND created_at > now() - interval '1 hour'")
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?;
    if recent >= MAX_NEW_PER_ACCOUNT_HOUR {
        return Err(AppError::Conflict("We're getting a lot of requests right now. Please try again later.".into()));
    }
    if from_ip(&mut tx, ip_hash.as_deref()).await? >= MAX_PER_VISITOR_HOUR {
        return Err(AppError::Conflict("Too many requests from here. Please try again later.".into()));
    }
    if !trusted {
        let unvouched: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalog_stock_subscriptions
             WHERE account_id = $1 AND created_at > now() - interval '1 hour' AND (confirm_sent_at IS NOT NULL OR confirmed_at IS NULL)",
        )
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?;
        if unvouched >= MAX_UNCONFIRMED_PER_ACCOUNT_HOUR {
            return Err(AppError::Conflict("We're getting a lot of requests right now. Please try again later.".into()));
        }
    }
    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions WHERE profile_id = $1 AND notified_at IS NULL").bind(profile.id).fetch_one(&mut *tx).await?;
    let id = Uuid::now_v7();
    let inserted: Option<Uuid> = if waiting >= MAX_WAITING_PER_PERSON {
        None
    } else {
        sqlx::query_scalar(
            "INSERT INTO catalog_stock_subscriptions (id, account_id, profile_id, product_id, variant_id, confirmed_at, email_key, ip_hash)
             VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END, $7, $8) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(id)
        .bind(account_id)
        .bind(profile.id)
        .bind(p.id)
        .bind(v.as_ref().map(|v| v.id))
        .bind(trusted)
        .bind(crate::optin::email_key(&email))
        .bind(&ip_hash)
        .fetch_optional(&mut *tx)
        .await?
    };
    // Asked before: the earlier request stands (a still-unconfirmed one may get its email again).
    let existing: Option<(Uuid, bool)> = match inserted {
        Some(_) => None,
        None => sqlx::query_as(
            "SELECT id, confirmed_at IS NOT NULL FROM catalog_stock_subscriptions
             WHERE profile_id = $1 AND product_id = $2 AND variant_id IS NOT DISTINCT FROM $3 AND notified_at IS NULL",
        )
        .bind(profile.id)
        .bind(p.id)
        .bind(v.as_ref().map(|v| v.id))
        .fetch_optional(&mut *tx)
        .await?,
    };
    tx.commit().await?;
    let done = s.widget["success_message"].as_str().filter(|m| !m.is_empty()).unwrap_or("Thanks! We'll email you when it's back.").to_string();
    let (sub_id, confirmed) = match (inserted, existing) {
        (Some(id), _) => (id, trusted),
        (None, Some(e)) => (e.0, e.1),
        (None, None) => return Err(AppError::Conflict(format!("You're already waiting on {MAX_WAITING_PER_PERSON} products."))),
    };
    if confirmed {
        if inserted.is_some() {
            subscribed_event(db, account_id, profile.id, sub_id, &p, v.as_ref()).await?;
        }
        return Ok(json!({"ok": true, "message": done}));
    }
    match send_confirmation(state, account, profile.id, sub_id, &email, &p).await? {
        Ok(()) => Ok(json!({"ok": true, "confirm": true, "message": "Almost done! Check your inbox and tap the link to confirm."})),
        Err(why) => {
            tracing::warn!(account = %account_id, reason = %why, "back-in-stock confirmation couldn't be sent; request dropped");
            if inserted.is_some() {
                sqlx::query("DELETE FROM catalog_stock_subscriptions WHERE id = $1 AND confirmed_at IS NULL").bind(sub_id).execute(db).await?;
            }
            Err(AppError::Conflict("We couldn't send your confirmation email right now. Please try again later.".into()))
        }
    }
}

async fn subscribed_event(db: &PgPool, account_id: Uuid, profile_id: Uuid, sub_id: Uuid, p: &Product, v: Option<&store::Variant>) -> AppResult<()> {
    let mut props = product_props(p, v);
    props["subscription_id"] = json!(sub_id);
    record_event(db, NewEvent { account_id, profile_id, metric: SUBSCRIBED_METRIC, properties: props, value: None, occurred_at: None, unique_id: Some(format!("bis-sub:{sub_id}")), message_id: None }).await?;
    Ok(())
}

pub fn confirm_token(state: &AppState, sub_id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("bis{}", sub_id.simple()))
}

fn sub_from_token(state: &AppState, token: &str) -> Option<Uuid> {
    crate::auth::verify(&state.cfg.secret_key, token)?.strip_prefix("bis")?.parse().ok()
}

/// Email the confirmation link, at most `MAX_CONFIRMS_PER_INBOX_DAY` a day to one inbox. A
/// suppressed address (it bounced or complained) gets nothing, but the visitor isn't told why.
async fn send_confirmation(state: &AppState, account: &crate::models::Account, profile_id: Uuid, sub_id: Uuid, email: &str, p: &Product) -> AppResult<Result<(), String>> {
    let db = &state.db;
    let suppressed: bool = sqlx::query_scalar("SELECT email_suppressed_reason IS NOT NULL FROM profiles WHERE id = $1").bind(profile_id).fetch_one(db).await?;
    // The per-inbox count and the send mark are taken under the inbox's lock.
    let mut tx = db.begin().await?;
    let key = crate::optin::email_key(email);
    lock(&mut tx, &format!("bis-inbox:{}:{key}", account.id)).await?;
    let sent_today: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM catalog_stock_subscriptions WHERE account_id = $1 AND email_key = $2 AND confirm_sent_at > now() - interval '24 hours'",
    )
    .bind(account.id)
    .bind(&key)
    .fetch_one(&mut *tx)
    .await?;
    if suppressed || sent_today >= MAX_CONFIRMS_PER_INBOX_DAY {
        return Ok(Ok(()));
    }
    sqlx::query("UPDATE catalog_stock_subscriptions SET confirm_sent_at = now() WHERE id = $1").bind(sub_id).execute(&mut *tx).await?;
    tx.commit().await?;
    // Product text from browser events isn't confirmed yet: it mustn't reach someone else's inbox.
    let what = if p.verified { p.title.clone() } else { "the product you asked about".into() };
    let url = format!("{}/client/back-in-stock/confirm/{}", crate::domains::link_base(state, account), confirm_token(state, sub_id));
    let copy = crate::optin::ConfirmCopy {
        subject: &format!("Confirm your back-in-stock alert from {}", account.name),
        heading: "Confirm your alert",
        line: &format!("Tap the button and we'll email you once when {what} is back in stock. This doesn't sign you up for anything else."),
        text_line: &format!("Open this link and we'll email you once when {what} is back in stock"),
        button: "Yes, tell me",
        didnt: "ask for this",
    };
    let res = crate::optin::deliver(state, account, profile_id, email, crate::optin::confirm_email(account, &url, &copy)).await?;
    if res.is_err() {
        sqlx::query("UPDATE catalog_stock_subscriptions SET confirm_sent_at = NULL WHERE id = $1").bind(sub_id).execute(db).await?;
    }
    Ok(res)
}

pub async fn subscribe_public(State(state): State<AppState>, Query(q): Query<KeyQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let res: AppResult<Value> = async {
        let account: crate::models::Account = sqlx::query_as("SELECT * FROM accounts WHERE public_key = $1").bind(&q.k).fetch_optional(&state.db).await?.ok_or(AppError::Unauthorized)?;
        let b: SubscribeBody = serde_json::from_slice(&body).map_err(|_| AppError::bad("send JSON with email and product_id"))?;
        let known_profile = b.token.as_deref().and_then(|t| crate::public::profile_from_token(&state, account.id, t));
        let ip = client_ip(&headers);
        let r = Request { email: b.email.as_deref(), known_profile, product_id: b.product_id.as_deref(), variant_id: b.variant_id.as_deref(), url: b.url.as_deref(), ip: ip.as_deref() };
        subscribe(&state, &account, &r).await
    }
    .await;
    cors(match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    })
}

/// The visitor's address as the hosting proxy saw it: the same rule as the rest of the public
/// endpoints (`X-Real-IP`, else the last `X-Forwarded-For` entry, which the proxy added).
fn client_ip(headers: &HeaderMap) -> Option<String> {
    let get = |h: &str| headers.get(h).and_then(|v| v.to_str().ok());
    let ip = |v: &str| v.trim().parse::<std::net::IpAddr>().ok().map(|a| a.to_string());
    get("x-real-ip").and_then(ip).or_else(|| get("x-forwarded-for").and_then(|v| v.rsplit(',').next()).and_then(ip))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The confirmation link from the email. Like double opt-in, confirming takes a button press (a
/// POST), so link scanners that open every link can't arm an alert.
pub async fn confirm_page(State(state): State<AppState>, axum::extract::Path(token): axum::extract::Path<String>) -> Response {
    confirm_response(&state, &token, false).await
}

pub async fn confirm_post(State(state): State<AppState>, axum::extract::Path(token): axum::extract::Path<String>) -> Response {
    confirm_response(&state, &token, true).await
}

async fn confirm_response(state: &AppState, token: &str, confirm: bool) -> Response {
    let Some(id) = sub_from_token(state, token) else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    match confirm_request(state, id, confirm).await {
        Ok((title, body, color)) => crate::public::page(&title, &body, &color).into_response(),
        Err(e) => {
            tracing::error!(error = %e.public_message(), "back-in-stock confirmation failed");
            crate::public::page("Something went wrong", "<h1>Something went wrong</h1><p>Please try the link again in a minute.</p>", "#1f6f5c").into_response()
        }
    }
}

async fn confirm_request(state: &AppState, id: Uuid, confirm: bool) -> AppResult<(String, String, String)> {
    let db = &state.db;
    let row: Option<(Uuid, Uuid, Uuid, Option<Uuid>, bool, bool, bool)> = sqlx::query_as(
        "SELECT account_id, profile_id, product_id, variant_id, confirmed_at IS NOT NULL, notified_at IS NOT NULL, created_at < now() - make_interval(days => $2)
         FROM catalog_stock_subscriptions WHERE id = $1",
    )
    .bind(id)
    .bind(CONFIRM_DAYS)
    .fetch_optional(db)
    .await?;
    let gone = || ("Link expired".to_string(), "<h1>This link has expired</h1><p>Ask again on the product page and we'll send a new one.</p>".to_string(), "#1f6f5c".to_string());
    let Some((account_id, profile_id, product_id, variant_id, confirmed, notified, expired)) = row else { return Ok(gone()) };
    let account = crate::campaigns::load_account(db, account_id).await?;
    let color = account.brand_color.clone();
    let Some(p) = product_by_id(db, product_id).await? else { return Ok(gone()) };
    let what = if p.verified { esc(&p.title) } else { "it".into() };
    if confirmed || notified {
        return Ok(("Alert on".into(), format!("<h1>You're all set</h1><p>We'll email you when {what} is back in stock.</p>"), color));
    }
    if expired {
        return Ok(gone());
    }
    if !confirm {
        let body = format!(r#"<h1>Turn on your alert?</h1><p>We'll email you once when {what} is back in stock at {}.</p><form method="post"><button type="submit">Yes, tell me</button></form>"#, esc(&account.name));
        return Ok(("Confirm your alert".into(), body, color));
    }
    let variant = match variant_id {
        Some(v) => store::variants(db, p.id).await?.into_iter().find(|x| x.id == v),
        None => None,
    };
    // Under the product row's lock, like a new request: either a restock sees it, or it sees the restock.
    let mut tx = db.begin().await?;
    let now_in: bool = sqlx::query_scalar(
        "SELECT CASE WHEN $2::uuid IS NULL THEN p.in_stock ELSE COALESCE((SELECT v.in_stock FROM catalog_variants v WHERE v.id = $2), false) END
         FROM catalog_products p WHERE p.id = $1 FOR SHARE OF p",
    )
    .bind(p.id)
    .bind(variant_id)
    .fetch_one(&mut *tx)
    .await?;
    let armed = sqlx::query("UPDATE catalog_stock_subscriptions SET confirmed_at = now(), notified_at = CASE WHEN $2 THEN now() END WHERE id = $1 AND confirmed_at IS NULL")
        .bind(id)
        .bind(now_in)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    tx.commit().await?;
    if now_in {
        let link = p.url.as_deref().filter(|_| p.verified).map(|u| format!(r#"<p><a href="{}">Take a look</a></p>"#, esc(u))).unwrap_or_default();
        return Ok(("It's back".into(), format!("<h1>Good news: it's back in stock</h1><p>{} is available again, so there's nothing to wait for.</p>{link}", if p.verified { esc(&p.title) } else { "It".into() }), color));
    }
    if armed {
        subscribed_event(db, account_id, profile_id, id, &p, variant.as_ref()).await?;
    }
    Ok(("Alert on".into(), format!("<h1>You're all set</h1><p>We'll email you when {what} is back in stock.</p>"), color))
}

const WIDGET_JS: &str = include_str!("back_in_stock.js");

/// The widget script, with this server's address and the store's public key filled in.
pub async fn widget_js(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<KeyQuery>) -> Response {
    let base = state.cfg.public_base_for(&crate::site::request_host(&headers));
    let js = WIDGET_JS
        .replacen("__BASE__", &serde_json::to_string(base).unwrap_or_default(), 1)
        .replacen("__KEY__", &serde_json::to_string(&q.k).unwrap_or_default(), 1);
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], js).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(percent: Option<f64>, amount: Option<f64>) -> Settings {
        Settings {
            learn_from_events: true,
            back_in_stock_enabled: true,
            back_in_stock_limit: None,
            price_drop_enabled: true,
            price_drop_percent: percent,
            price_drop_amount: amount,
            price_drop_lookback_days: 14,
            price_drop_cooldown_days: 30,
            widget: json!({}),
        }
    }

    #[test]
    fn price_drop_thresholds() {
        assert!(drop_qualifies(&s(Some(10.0), None), 100.0, 90.0));
        assert!(!drop_qualifies(&s(Some(10.0), None), 100.0, 91.0));
        assert!(drop_qualifies(&s(None, Some(5.0)), 20.0, 15.0));
        assert!(!drop_qualifies(&s(Some(10.0), Some(20.0)), 100.0, 85.0), "both thresholds must be met");
        assert!(!drop_qualifies(&s(None, None), 100.0, 1.0));
        assert!(!drop_qualifies(&s(Some(10.0), None), 100.0, 120.0));
    }
}

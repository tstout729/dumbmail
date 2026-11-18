//! History import (customers, products, then orders) and product reconciles, read with bulk
//! operations.
//!
//! Each phase starts one bulk query, waits for Shopify to finish it, then reads the JSONL result in
//! slices with HTTP range requests: a slice processes whole records and saves the byte offset where
//! the next one starts, so a crash or a pause picks up there, and replaying a slice changes nothing
//! (every write is keyed on Shopify's ids). Imported events are history: no flows, no sends, no
//! billing (see `events`). Memory is bounded by the slice size.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::client::{self, ApiError};
use super::customers::{self, Customer};
use super::events::Metrics;
use super::orders::{self, Order};
use super::products::{self, Node};
use super::shop::{Lease, ShopRow};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Bytes read from a result file per request.
const CHUNK: usize = 4 * 1024 * 1024;
/// Longest JSONL line we accept (a product with every variant fits easily).
const MAX_LINE: usize = 2 * 1024 * 1024;
/// Default order history.
pub const DEFAULT_ORDER_YEARS: i64 = 3;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImportRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub kind: String,
    pub status: String,
    pub phases: Vec<String>,
    pub phase: String,
    pub orders_since: DateTime<Utc>,
    pub bulk_id: Option<String>,
    pub bulk_status: Option<String>,
    pub result_url_enc: Option<String>,
    pub result_offset: i64,
    pub counts: Value,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub generation: Option<i32>,
}

const COLS: &str = "id, account_id, kind, status, phases, phase, orders_since, bulk_id, bulk_status, result_url_enc, result_offset, counts, error, started_at, finished_at, updated_at, generation";

pub fn to_json(i: &ImportRow) -> Value {
    json!({
        "id": i.id,
        "kind": i.kind,
        "status": i.status,
        "phase": i.phase,
        "phases": i.phases,
        "orders_since": i.orders_since,
        "counts": i.counts,
        "shopify_status": i.bulk_status.as_deref().map(str::to_ascii_lowercase),
        "error": i.error,
        "started_at": i.started_at,
        "finished_at": i.finished_at,
        "updated_at": i.updated_at,
    })
}

pub async fn latest(db: &sqlx::PgPool, account_id: Uuid, kind: &str) -> AppResult<Option<ImportRow>> {
    Ok(sqlx::query_as::<_, ImportRow>(&format!("SELECT {COLS} FROM shopify_imports WHERE account_id = $1 AND kind = $2 ORDER BY started_at DESC LIMIT 1"))
        .bind(account_id)
        .bind(kind)
        .fetch_optional(db)
        .await?)
}

pub async fn active(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<Option<ImportRow>> {
    sqlx::query_as::<_, ImportRow>(&format!("SELECT {COLS} FROM shopify_imports WHERE account_id = $1 AND status IN ('running', 'paused')"))
        .bind(account_id)
        .fetch_optional(db)
        .await
}

/// Start a history import (or a product reconcile with `kind = "products"`). One at a time.
pub async fn start(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, kind: &str, order_days: Option<i64>) -> AppResult<ImportRow> {
    let shop = super::shop::connected(&state.db, account_id).await?;
    let phases: Vec<String> = if kind == "products" { vec!["products".into()] } else { vec!["customers".into(), "products".into(), "orders".into()] };
    let days = order_days.unwrap_or(DEFAULT_ORDER_YEARS * 365).clamp(1, 20 * 365);
    let mut tx = state.db.begin().await?;
    let r = sqlx::query_as::<_, ImportRow>(&format!(
        "INSERT INTO shopify_imports (id, account_id, kind, status, phases, phase, orders_since, started_by, counts, generation, shop_domain)
         VALUES ($1, $2, $3, 'running', $4, $5, now() - make_interval(days => $6), $7, '{{}}', $8, $9) RETURNING {COLS}"
    ))
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(kind)
    .bind(&phases)
    .bind(&phases[0])
    .bind(days as i32)
    .bind(user_id)
    .bind(shop.generation)
    .bind(&shop.shop_domain)
    .fetch_one(&mut *tx)
    .await;
    let row = match r {
        Ok(r) => r,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            return Err(AppError::Conflict("an import is already running for this store; wait for it, or cancel it first".into()))
        }
        Err(e) => return Err(e.into()),
    };
    super::shop::kick(&mut tx, account_id).await?;
    tx.commit().await?;
    Ok(row)
}

/// Pause, resume or cancel the running import.
pub async fn set_status(state: &AppState, account_id: Uuid, action: &str) -> AppResult<ImportRow> {
    let (from, to): (&[&str], &str) = match action {
        "pause" => (&["running"], "paused"),
        "resume" => (&["paused"], "running"),
        "cancel" => (&["running", "paused"], "cancelled"),
        _ => return Err(AppError::bad("action must be pause, resume or cancel")),
    };
    let from: Vec<String> = from.iter().map(|s| s.to_string()).collect();
    let row = sqlx::query_as::<_, ImportRow>(&format!(
        "UPDATE shopify_imports SET status = $3, updated_at = now(),
            finished_at = CASE WHEN $3 = 'cancelled' THEN now() ELSE finished_at END,
            result_url_enc = CASE WHEN $3 = 'cancelled' THEN NULL ELSE result_url_enc END
         WHERE account_id = $1 AND status = ANY($2) RETURNING {COLS}"
    ))
    .bind(account_id)
    .bind(&from)
    .bind(to)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::Conflict(format!("there's no import to {action}")))?;
    if to == "running" {
        super::shop::kick_now(&state.db, account_id).await?;
    } else {
        // Paused or cancelled: segments stop absorbing entries (see `events::close_history`).
        super::events::close_history_now(&state.db, account_id).await?;
    }
    if to == "cancelled" {
        // Stop Shopify's side too (best effort; it ends on its own otherwise).
        if let (Some(bulk), Ok(shop)) = (&row.bulk_id, super::shop::connected(&state.db, account_id).await) {
            if row.bulk_status.as_deref().is_some_and(|s| s == "CREATED" || s == "RUNNING") {
                let _ = client::graphql(state, &shop, "mutation($id: ID!) { bulkOperationCancel(id: $id) { bulkOperation { id } userErrors { field message } } }", json!({"id": bulk})).await;
            }
        }
    }
    Ok(row)
}

fn query_for(phase: &str, since: DateTime<Utc>) -> String {
    match phase {
        "customers" => format!("{{ customers {{ edges {{ node {{ {} }} }} }} }}", customers::FIELDS),
        "products" => format!(
            "{{ products {{ edges {{ node {{ {}
                variants {{ edges {{ node {{ {} }} }} }}
                collections {{ edges {{ node {{ id title }} }} }}
                media {{ edges {{ node {{ ... on MediaImage {{ id image {{ url }} }} }} }} }} }} }} }} }}",
            products::PRODUCT_FIELDS,
            products::VARIANT_FIELDS
        ),
        _ => format!(
            "{{ orders(query: \"created_at:>={}\") {{ edges {{ node {{ id name createdAt processedAt updatedAt cancelledAt cancelReason test email phone currencyCode
                displayFinancialStatus displayFulfillmentStatus discountCodes
                totalPriceSet {{ shopMoney {{ amount currencyCode }} }} subtotalPriceSet {{ shopMoney {{ amount }} }} totalDiscountsSet {{ shopMoney {{ amount }} }}
                totalShippingPriceSet {{ shopMoney {{ amount }} }} totalTaxSet {{ shopMoney {{ amount }} }}
                customer {{ id firstName lastName }}
                refunds {{ id }}
                fulfillments {{ id createdAt status }}
                lineItems {{ edges {{ node {{ id title variantTitle sku vendor quantity originalUnitPriceSet {{ shopMoney {{ amount }} }}
                    totalDiscountSet {{ shopMoney {{ amount }} }} product {{ id }} variant {{ id }} image {{ url }} }} }} }} }} }} }} }}",
            since.format("%Y-%m-%d")
        ),
    }
}

pub enum Step {
    /// Did some work; there's more now.
    More,
    /// Waiting for Shopify (a bulk operation running, or the rate limit): look again after this long.
    Wait(Duration),
    /// Nothing to do.
    Idle,
}

/// Save the import's progress, only while this run still holds the account's lease for the install
/// the import belongs to (checked in the same statement), so a run that outlived its lease or an
/// import from an earlier install can't write. False: it couldn't.
async fn save(db: &sqlx::PgPool, i: &ImportRow, lease: &Lease) -> sqlx::Result<bool> {
    let n = sqlx::query(&format!(
        "UPDATE shopify_imports SET phase = $5, bulk_id = $6, bulk_status = $7, result_url_enc = $8, result_offset = $9, counts = $10, error = $11,
            status = CASE WHEN status = 'running' THEN $12 ELSE status END, finished_at = CASE WHEN $12 IN ('done', 'failed') AND status = 'running' THEN now() ELSE finished_at END,
            updated_at = now()
         WHERE id = $4 AND generation = $3 AND {}",
        super::shop::LEASE_GUARD
    ))
    .bind(lease.account_id)
    .bind(lease.token)
    .bind(lease.generation)
    .bind(i.id)
    .bind(&i.phase)
    .bind(&i.bulk_id)
    .bind(&i.bulk_status)
    .bind(&i.result_url_enc)
    .bind(i.result_offset)
    .bind(&i.counts)
    .bind(&i.error)
    .bind(&i.status)
    .execute(db)
    .await?
    .rows_affected();
    Ok(n == 1)
}

fn bump(counts: &mut Value, k: &str, by: u64) {
    let cur = counts[k].as_u64().unwrap_or(0);
    counts[k] = json!(cur + by);
}

fn next_phase(i: &mut ImportRow) {
    let pos = i.phases.iter().position(|p| *p == i.phase).map(|p| p + 1).unwrap_or(i.phases.len());
    i.bulk_id = None;
    i.bulk_status = None;
    i.result_url_enc = None;
    i.result_offset = 0;
    match i.phases.get(pos) {
        Some(p) => i.phase = p.clone(),
        None => {
            i.phase = "done".into();
            i.status = "done".into();
        }
    }
}

/// Do one step of the running import, within `deadline`.
pub async fn step(state: &AppState, shop: &ShopRow, lease: &Lease, metrics: &mut Metrics, deadline: std::time::Instant) -> anyhow::Result<Step> {
    let Some(mut i) = active(&state.db, shop.account_id).await? else { return Ok(Step::Idle) };
    if i.status != "running" {
        return Ok(Step::Idle);
    }
    if i.generation != Some(lease.generation) {
        // Queued for an earlier install of the app: it can't write to this one.
        sqlx::query("UPDATE shopify_imports SET status = 'cancelled', finished_at = now(), result_url_enc = NULL, updated_at = now() WHERE id = $1 AND status IN ('running', 'paused')")
            .bind(i.id)
            .execute(&state.db)
            .await?;
        super::events::close_history_now(&state.db, shop.account_id).await?;
        return Ok(Step::More);
    }
    let r = step_inner(state, shop, lease, metrics, &mut i, deadline).await;
    let out = match r {
        Ok(s) => {
            // A pause or cancel that came in meanwhile is kept (save only changes a running import's status).
            save(&state.db, &i, lease).await?;
            Ok(s)
        }
        Err(e) => match e.retry_after() {
            Some(secs) => {
                save(&state.db, &i, lease).await?;
                Ok(Step::Wait(Duration::seconds(secs as i64)))
            }
            None => {
                i.status = "failed".into();
                i.error = Some(format!("The import stopped: {e}. Start it again from Settings → Shopify; nothing is brought in twice."));
                save(&state.db, &i, lease).await?;
                Ok(Step::Idle)
            }
        },
    };
    // Finished, failed, paused or cancelled meanwhile: the history window closes.
    super::events::close_history_now(&state.db, shop.account_id).await?;
    out
}

async fn step_inner(state: &AppState, shop: &ShopRow, lease: &Lease, metrics: &mut Metrics, i: &mut ImportRow, deadline: std::time::Instant) -> Result<Step, ApiError> {
    // 1. Start this phase's bulk query.
    let Some(bulk_id) = i.bulk_id.clone() else {
        let d = client::graphql(
            state,
            shop,
            "mutation($q: String!) { bulkOperationRunQuery(query: $q, groupObjects: true) { bulkOperation { id status } userErrors { field message code } } }",
            json!({"q": query_for(&i.phase, i.orders_since)}),
        )
        .await?;
        let p = &d["bulkOperationRunQuery"];
        if let Some(code) = client::user_error(p) {
            // Too many bulk operations on this store at once: wait.
            if code.contains("limit") || code.contains("in_progress") || code.contains("already") {
                return Ok(Step::Wait(Duration::seconds(60)));
            }
            return Err(ApiError::Graph(code));
        }
        i.bulk_id = p["bulkOperation"]["id"].as_str().map(String::from);
        i.bulk_status = p["bulkOperation"]["status"].as_str().map(String::from);
        return Ok(if i.bulk_id.is_some() { Step::Wait(Duration::seconds(5)) } else { Step::Wait(Duration::seconds(30)) });
    };
    // 2. Wait for it.
    if i.result_url_enc.is_none() {
        let d = client::graphql(state, shop, "query($id: ID!) { node(id: $id) { ... on BulkOperation { id status errorCode objectCount url } } }", json!({"id": bulk_id})).await?;
        let n = &d["node"];
        let status = n["status"].as_str().unwrap_or("").to_string();
        i.bulk_status = Some(status.clone());
        i.counts["shopify_objects"] = json!(n["objectCount"].as_str().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0));
        match status.as_str() {
            "CREATED" | "RUNNING" | "CANCELING" => return Ok(Step::Wait(Duration::seconds(10))),
            "COMPLETED" => match n["url"].as_str() {
                None => {
                    next_phase(i);
                    return Ok(Step::More);
                }
                Some(u) if client::result_url_ok(state, u) => {
                    i.result_url_enc = Some(crate::crypto::encrypt(&state.cfg.secret_key, u).map_err(|_| ApiError::Network("encrypt"))?);
                    i.result_offset = 0;
                }
                Some(_) => return Err(ApiError::Graph("result link on an unexpected host".into())),
            },
            // Shopify cancelled or expired it: run it again.
            "CANCELED" | "EXPIRED" => {
                i.bulk_id = None;
                return Ok(Step::Wait(Duration::seconds(5)));
            }
            _ => return Err(ApiError::Graph(n["errorCode"].as_str().map(|c| c.to_ascii_lowercase()).unwrap_or_else(|| "bulk operation failed".into()))),
        }
    }
    // 3. Read the result in slices. Bytes after the last whole record are kept and the next read
    // continues after them, so a record split across reads is put back together.
    let url = crate::crypto::decrypt(&state.cfg.secret_key, i.result_url_enc.as_deref().unwrap_or("")).map_err(|_| ApiError::Graph("result link unreadable".into()))?;
    let mut buf: Vec<u8> = Vec::new();
    while std::time::Instant::now() < deadline {
        if !super::shop::renew(state, lease).await.map_err(|_| ApiError::Network("database"))? {
            // Lost the lease (or the install changed): stop without writing.
            return Err(ApiError::Busy);
        }
        let Some((bytes, at_end)) = client::result_chunk(state, &url, i.result_offset as u64 + buf.len() as u64, CHUNK).await? else {
            // The link expired (results last a week): run the phase's query again. Nothing is duplicated.
            i.bulk_id = None;
            i.result_url_enc = None;
            i.result_offset = 0;
            return Ok(Step::More);
        };
        buf.extend_from_slice(&bytes);
        if i.kind == "history" {
            // Segments absorb the entries this page of history causes (see `events::open_history`).
            super::events::open_history(&state.db, i.id, shop.account_id).await.map_err(|_| ApiError::Network("database"))?;
        }
        let consumed = process_chunk(state, shop, metrics, i, &buf, at_end).await.map_err(|_| ApiError::Network("database"))?;
        if consumed > 0 {
            i.result_offset += consumed as i64;
            buf.drain(..consumed);
            if !save(&state.db, i, lease).await.map_err(|_| ApiError::Network("database"))? {
                return Err(ApiError::Busy);
            }
        }
        if at_end {
            if i.phase == "products" {
                let gone = products::finish_scan(&state.db, shop, i.id, i.started_at).await.map_err(|_| ApiError::Network("database"))?;
                bump(&mut i.counts, "products_removed", gone);
            }
            next_phase(i);
            return Ok(Step::More);
        }
        if buf.len() > MAX_LINE + CHUNK {
            return Err(ApiError::Graph("a record in the result was too large".into()));
        }
    }
    Ok(Step::More)
}

/// Group result lines under their parents. With `groupObjects` Shopify puts each object's children
/// right after it, but every child is still checked: it joins the parent its `__parentId` names
/// (looked up by id among the records in this read), never simply the record before it. A child
/// whose parent isn't in this read is counted and left out.
pub fn group(lines: Vec<(usize, Value)>) -> (Vec<(usize, Value, Vec<Value>)>, u64) {
    let mut groups: Vec<(usize, Value, Vec<Value>)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut orphans = 0;
    for (off, v) in lines {
        match v.get("__parentId").and_then(|p| p.as_str()).map(String::from) {
            Some(parent) => match index.get(&parent) {
                Some(&g) => groups[g].2.push(v),
                None => orphans += 1,
            },
            None => {
                if let Some(id) = v["id"].as_str() {
                    index.insert(id.to_string(), groups.len());
                }
                groups.push((off, v, Vec::new()));
            }
        }
    }
    (groups, orphans)
}

/// Process the whole records in `bytes` (a parent and the child lines that follow it). Returns how
/// many bytes were used: the next read starts at the first record not finished here.
async fn process_chunk(state: &AppState, shop: &ShopRow, metrics: &mut Metrics, i: &mut ImportRow, bytes: &[u8], at_end: bool) -> anyhow::Result<usize> {
    // Split into complete lines, remembering where each starts.
    let mut lines: Vec<(usize, Value)> = Vec::new();
    let mut start = 0;
    let mut complete_end = 0;
    while let Some(nl) = bytes[start..].iter().position(|b| *b == b'\n') {
        let end = start + nl;
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes[start..end]) {
            lines.push((start, v));
        }
        start = end + 1;
        complete_end = start;
    }
    if at_end && start < bytes.len() {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes[start..]) {
            lines.push((start, v));
        }
        complete_end = bytes.len();
    }
    // Group: a top-level line (no __parentId) and its children. The last group may continue in the
    // next chunk unless this is the end of the file.
    let (mut groups, orphans) = group(lines);
    if orphans > 0 {
        bump(&mut i.counts, "skipped_children", orphans);
    }
    let used = if at_end {
        complete_end
    } else {
        match groups.pop() {
            Some((off, _, _)) => off,
            None => 0,
        }
    };
    let mut cx = orders::Ctx { state, account_id: shop.account_id, shop: &shop.shop_domain, metrics, quiet: true };
    match i.phase.as_str() {
        "customers" => {
            let mut n = 0;
            for (_, v, _) in &groups {
                if let Some(c) = Customer::from_graphql(v) {
                    customers::apply(&state.db, shop.account_id, &shop.shop_domain, &c).await?;
                    n += 1;
                }
            }
            bump(&mut i.counts, "customers", n);
        }
        "products" => {
            let nodes: Vec<Node> = groups
                .iter()
                .map(|(_, p, kids)| {
                    let kind = |k: &Value, t: &str| k["id"].as_str().is_some_and(|id| id.starts_with(&format!("gid://shopify/{t}/")));
                    Node {
                        product: p.clone(),
                        variants: kids.iter().filter(|k| kind(k, "ProductVariant")).cloned().collect(),
                        collections: kids.iter().filter(|k| kind(k, "Collection")).filter_map(|k| k["title"].as_str().map(String::from)).collect(),
                        images: kids.iter().filter(|k| kind(k, "MediaImage")).filter_map(|k| k["image"]["url"].as_str().map(String::from)).collect(),
                    }
                })
                .collect();
            // A history import never queues back-in-stock or price-drop alerts; a reconcile does
            // (it's catching changes a webhook missed), except on the store's first load.
            let mode = products::SaveMode { quiet: i.kind == "history", scan: Some(i.id) };
            let n = products::save(&state.db, shop, &nodes, mode).await?;
            bump(&mut i.counts, "products", n);
        }
        _ => {
            let mut n = 0;
            for (_, v, kids) in &groups {
                if let Some(o) = Order::from_graphql(v, kids) {
                    orders::apply(&mut cx, &o).await?;
                    n += 1;
                }
            }
            bump(&mut i.counts, "orders", n);
        }
    }
    Ok(used)
}

/// Products are read in full once a day, in case a webhook was missed.
pub async fn queue_reconciles(db: &sqlx::PgPool) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE shopify_shops s SET reconcile_after = now() + interval '1 day'
         WHERE s.status = 'connected' AND s.reconcile_after < now()
           AND NOT EXISTS (SELECT 1 FROM shopify_imports i WHERE i.account_id = s.account_id AND i.status IN ('running', 'paused'))
         RETURNING s.account_id",
    )
    .fetch_all(db)
    .await?;
    for account_id in due {
        let mut tx = db.begin().await?;
        let r = sqlx::query(
            "INSERT INTO shopify_imports (id, account_id, kind, status, phases, phase, orders_since, counts, generation, shop_domain)
             SELECT $1, $2, 'products', 'running', ARRAY['products'], 'products', now(), '{}', generation, shop_domain FROM shopify_shops WHERE account_id = $2
             ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 1 {
            super::shop::kick(&mut tx, account_id).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

//! Orders, refunds, fulfilments and checkouts as events, with the snippet's event names and
//! property names so flows, segments and reports work the same whichever source recorded them.
//!
//! | Event | unique_id | value |
//! |---|---|---|
//! | Placed Order | `order:<id>` (same as the snippet and pixel) | the order total less refunds; 0 once cancelled |
//! | Ordered Product (one per line) | `order:<id>:<line id>` | the line total less its refunds; 0 once cancelled |
//! | Fulfilled Order | `order:<id>` | the order total |
//! | Fulfilled Partial Order (per shipment) | `fulfillment:<id>` | none |
//! | Cancelled Order | `order:<id>` | the order total |
//! | Refunded Order (per refund) | `refund:<id>` | the amount refunded |
//! | Started Checkout | `checkout:<token>` (same as the pixel) | the checkout total |
//!
//! **Refunds and cancellations.** Revenue everywhere (overview, reports, attribution, cohorts, segment
//! totals and predictions) is the sum of `Placed Order` values. So instead of adding negative events
//! that every one of those would have to learn about, the `Placed Order` event's value is kept equal
//! to what the store actually kept: the total less every refund seen so far, and 0 when the order is
//! cancelled. Refunds already seen are kept in the event (`refunds`, by refund id), so applying the
//! same refund twice, or an older webhook after a newer one, never subtracts twice or adds back.
//! The order still counts as an order; `cancelled: true` and `refunded_total` say what happened.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::customers::{amount, num_id, parse_time, profile_for};
use super::events::{self, Metrics, ServerEvent};
use crate::state::AppState;

#[derive(Debug, Clone, Default)]
pub struct Line {
    pub id: i64,
    pub product_id: Option<i64>,
    pub variant_id: Option<i64>,
    pub title: Option<String>,
    pub variant_title: Option<String>,
    pub sku: Option<String>,
    pub vendor: Option<String>,
    pub quantity: i64,
    pub price: Option<f64>,
    pub discount: f64,
    pub image_url: Option<String>,
}

impl Line {
    fn total(&self) -> f64 {
        (self.price.unwrap_or(0.0) * self.quantity as f64 - self.discount).max(0.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Refund {
    pub id: i64,
    pub at: Option<DateTime<Utc>>,
    pub amount: f64,
    /// Refunded amount per line item, when Shopify says (webhooks do; history doesn't).
    pub lines: BTreeMap<i64, f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Order {
    pub id: i64,
    pub name: Option<String>,
    pub number: Option<i64>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub customer_id: Option<i64>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub processed_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub cancel_reason: Option<String>,
    pub currency: Option<String>,
    pub total: f64,
    pub subtotal: Option<f64>,
    pub discounts: Option<f64>,
    pub shipping: Option<f64>,
    pub tax: Option<f64>,
    pub discount_codes: Vec<String>,
    pub financial_status: Option<String>,
    pub fulfillment_status: Option<String>,
    pub lines: Vec<Line>,
    pub refunds: Vec<Refund>,
    /// (id, created at, status)
    pub fulfillments: Vec<(i64, Option<DateTime<Utc>>, Option<String>)>,
    pub status_url: Option<String>,
    pub test: bool,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|x| !x.is_empty()).map(|x| x.chars().take(500).collect())
}

fn t(v: &Value) -> Option<DateTime<Utc>> {
    parse_time(v.as_str())
}

/// Most line items or refunds read from one order (Shopify allows far fewer in practice).
const MAX_LINES: usize = 500;

impl Order {
    /// An order webhook body (`orders/*`), which uses the REST resource's field names.
    pub fn from_webhook(v: &Value) -> Option<Order> {
        let id = num_id(&v["id"])?;
        let c = &v["customer"];
        let lines = v["line_items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(MAX_LINES)
                    .filter_map(|l| {
                        let discount = l["discount_allocations"].as_array().map(|d| d.iter().filter_map(|x| amount(&x["amount"])).sum()).filter(|d: &f64| *d > 0.0).or_else(|| amount(&l["total_discount"])).unwrap_or(0.0);
                        Some(Line {
                            id: num_id(&l["id"])?,
                            product_id: num_id(&l["product_id"]),
                            variant_id: num_id(&l["variant_id"]),
                            title: s(&l["title"]).or_else(|| s(&l["name"])),
                            variant_title: s(&l["variant_title"]),
                            sku: s(&l["sku"]),
                            vendor: s(&l["vendor"]),
                            quantity: l["quantity"].as_i64().unwrap_or(1),
                            price: amount(&l["price"]),
                            discount,
                            image_url: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let refunds = v["refunds"].as_array().map(|a| a.iter().take(MAX_LINES).filter_map(Refund::from_webhook).collect()).unwrap_or_default();
        let fulfillments = v["fulfillments"]
            .as_array()
            .map(|a| a.iter().take(MAX_LINES).filter_map(|f| Some((num_id(&f["id"])?, t(&f["created_at"]), s(&f["status"])))).collect())
            .unwrap_or_default();
        Some(Order {
            id,
            name: s(&v["name"]),
            number: v["order_number"].as_i64(),
            email: s(&v["email"]).or_else(|| s(&v["contact_email"])).or_else(|| s(&c["email"])),
            phone: s(&v["phone"]).or_else(|| s(&c["phone"])),
            customer_id: num_id(&c["id"]),
            first_name: s(&c["first_name"]),
            last_name: s(&c["last_name"]),
            created_at: t(&v["created_at"]),
            processed_at: t(&v["processed_at"]),
            updated_at: t(&v["updated_at"]),
            cancelled_at: t(&v["cancelled_at"]),
            cancel_reason: s(&v["cancel_reason"]),
            currency: s(&v["currency"]),
            total: amount(&v["total_price"]).unwrap_or(0.0),
            subtotal: amount(&v["subtotal_price"]),
            discounts: amount(&v["total_discounts"]),
            shipping: amount(&v["total_shipping_price_set"]["shop_money"]["amount"]),
            tax: amount(&v["total_tax"]),
            discount_codes: v["discount_codes"].as_array().map(|a| a.iter().filter_map(|d| s(&d["code"])).take(20).collect()).unwrap_or_default(),
            financial_status: s(&v["financial_status"]),
            fulfillment_status: s(&v["fulfillment_status"]),
            lines,
            refunds,
            fulfillments,
            status_url: s(&v["order_status_url"]),
            test: v["test"].as_bool().unwrap_or(false),
        })
    }

    /// An order from a bulk read of the GraphQL Admin API, with its line item lines.
    pub fn from_graphql(v: &Value, line_items: &[Value]) -> Option<Order> {
        let id = num_id(&v["id"])?;
        let money = |k: &str| amount(&v[k]["shopMoney"]["amount"]);
        let lines = line_items
            .iter()
            .take(MAX_LINES)
            .filter_map(|l| {
                Some(Line {
                    id: num_id(&l["id"])?,
                    product_id: num_id(&l["product"]["id"]),
                    variant_id: num_id(&l["variant"]["id"]),
                    title: s(&l["title"]),
                    variant_title: s(&l["variantTitle"]),
                    sku: s(&l["sku"]),
                    vendor: s(&l["vendor"]),
                    quantity: l["quantity"].as_i64().unwrap_or(1),
                    price: amount(&l["originalUnitPriceSet"]["shopMoney"]["amount"]),
                    discount: amount(&l["totalDiscountSet"]["shopMoney"]["amount"]).unwrap_or(0.0),
                    image_url: s(&l["image"]["url"]),
                })
            })
            .collect();
        let refunds = v["refunds"]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(MAX_LINES)
                    .filter_map(|r| Some(Refund { id: num_id(&r["id"])?, at: t(&r["createdAt"]), amount: amount(&r["totalRefundedSet"]["shopMoney"]["amount"]).unwrap_or(0.0), lines: BTreeMap::new() }))
                    .collect()
            })
            .unwrap_or_default();
        let fulfillments = v["fulfillments"]
            .as_array()
            .map(|a| a.iter().take(MAX_LINES).filter_map(|f| Some((num_id(&f["id"])?, t(&f["createdAt"]), s(&f["status"]).map(|x| x.to_ascii_lowercase())))).collect())
            .unwrap_or_default();
        let name = s(&v["name"]);
        Some(Order {
            id,
            number: name.as_deref().and_then(|n| n.trim_start_matches('#').parse().ok()),
            name,
            email: s(&v["email"]),
            phone: s(&v["phone"]),
            customer_id: num_id(&v["customer"]["id"]),
            first_name: s(&v["customer"]["firstName"]),
            last_name: s(&v["customer"]["lastName"]),
            created_at: t(&v["createdAt"]),
            processed_at: t(&v["processedAt"]),
            updated_at: t(&v["updatedAt"]),
            cancelled_at: t(&v["cancelledAt"]),
            cancel_reason: s(&v["cancelReason"]).map(|x| x.to_ascii_lowercase()),
            currency: s(&v["currencyCode"]),
            total: money("totalPriceSet").unwrap_or(0.0),
            subtotal: money("subtotalPriceSet"),
            discounts: money("totalDiscountsSet"),
            shipping: money("totalShippingPriceSet"),
            tax: money("totalTaxSet"),
            discount_codes: v["discountCodes"].as_array().map(|a| a.iter().filter_map(s).take(20).collect()).unwrap_or_default(),
            financial_status: s(&v["displayFinancialStatus"]).map(|x| x.to_ascii_lowercase()),
            fulfillment_status: s(&v["displayFulfillmentStatus"]).map(|x| x.to_ascii_lowercase()),
            lines,
            refunds,
            fulfillments,
            status_url: None,
            test: v["test"].as_bool().unwrap_or(false),
        })
    }

    fn placed_at(&self) -> DateTime<Utc> {
        self.processed_at.or(self.created_at).unwrap_or_else(Utc::now)
    }
}

impl Refund {
    /// A refund in a webhook body (`refunds/create`, or an order's `refunds`).
    pub fn from_webhook(r: &Value) -> Option<Refund> {
        let id = num_id(&r["id"])?;
        let tx: f64 = r["transactions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|x| x["kind"].as_str() == Some("refund") && x["status"].as_str().is_none_or(|st| st == "success"))
                    .filter_map(|x| amount(&x["amount"]))
                    .sum()
            })
            .unwrap_or(0.0);
        let mut lines = BTreeMap::new();
        let mut line_sum = 0.0;
        for l in r["refund_line_items"].as_array().into_iter().flatten().take(MAX_LINES) {
            if let (Some(lid), Some(sub)) = (num_id(&l["line_item_id"]), amount(&l["subtotal"])) {
                *lines.entry(lid).or_insert(0.0) += sub;
                line_sum += sub;
            }
        }
        Some(Refund { id, at: t(&r["created_at"]), amount: if tx > 0.0 { tx } else { line_sum }, lines })
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Refunds known so far: what's already on the event plus these. Keyed by refund id, so nothing counts twice.
fn merged_refunds(old: &Value, new: &[Refund]) -> (Map<String, Value>, Map<String, Value>) {
    let mut refunds = old["refunds"].as_object().cloned().unwrap_or_default();
    let mut lines = old["line_refunds"].as_object().cloned().unwrap_or_default();
    for r in new {
        let key = r.id.to_string();
        if refunds.contains_key(&key) {
            continue;
        }
        refunds.insert(key.clone(), json!(round2(r.amount)));
        for (lid, a) in &r.lines {
            let e = lines.entry(lid.to_string()).or_insert(json!({}));
            if let Some(o) = e.as_object_mut() {
                o.insert(key.clone(), json!(round2(*a)));
            }
        }
    }
    (refunds, lines)
}

fn sum_obj(m: &Map<String, Value>) -> f64 {
    m.values().filter_map(|v| v.as_f64()).sum()
}

fn line_refunded(lines: &Map<String, Value>, line_id: i64) -> f64 {
    lines.get(&line_id.to_string()).and_then(|v| v.as_object()).map(sum_obj).unwrap_or(0.0)
}

pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub account_id: Uuid,
    pub metrics: &'a mut Metrics,
    /// History import: record everything quietly.
    pub quiet: bool,
}

fn items(o: &Order, links: &std::collections::HashMap<String, (Option<String>, Option<String>, Vec<String>)>) -> Vec<Value> {
    o.lines
        .iter()
        .map(|l| {
            let pid = l.product_id.map(|p| p.to_string());
            let link = pid.as_ref().and_then(|p| links.get(p));
            let mut it = Map::new();
            let mut put = |k: &str, v: Value| {
                if !v.is_null() {
                    it.insert(k.into(), v);
                }
            };
            put("name", json!(l.title));
            put("product_id", json!(pid));
            put("variant_id", json!(l.variant_id.map(|v| v.to_string())));
            put("variant", json!(l.variant_title.as_deref().filter(|v| *v != "Default Title")));
            put("sku", json!(l.sku));
            put("price", json!(l.price));
            put("quantity", json!(l.quantity));
            put("image_url", json!(l.image_url.clone().or_else(|| link.and_then(|x| x.1.clone()))));
            put("url", json!(link.and_then(|x| x.0.clone())));
            put("brand", json!(l.vendor));
            if let Some(cats) = link.map(|x| &x.2).filter(|c| !c.is_empty()) {
                put("categories", json!(cats));
            }
            Value::Object(it)
        })
        .collect()
}

/// Record everything an order says: Placed Order and Ordered Product (kept up to date), Fulfilled,
/// Partially Fulfilled, Cancelled and Refunded events, and redemptions of codes we made.
/// Every step is idempotent, so a redelivered or replayed webhook changes nothing.
pub async fn apply(cx: &mut Ctx<'_>, o: &Order) -> anyhow::Result<()> {
    if o.test {
        return Ok(());
    }
    let db = &cx.state.db;
    let Some(profile_id) = profile_for(db, cx.account_id, o.customer_id, o.email.as_deref(), o.phone.as_deref(), o.first_name.as_deref(), o.last_name.as_deref()).await? else {
        return Ok(());
    };
    let pids: Vec<String> = o.lines.iter().filter_map(|l| l.product_id.map(|p| p.to_string())).collect();
    let links = super::products::links(db, cx.account_id, &pids).await?;
    let items = items(o, &links);
    let cancelled = o.cancelled_at.is_some();
    let mut base = Map::new();
    base.insert("tracked_by".into(), json!("shopify"));
    base.insert("order_id".into(), json!(o.id.to_string()));
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            base.insert(k.into(), v);
        }
    };
    put("order_name", json!(o.name));
    put("order_number", json!(o.number));
    put("currency", json!(o.currency));
    put("discount_codes", json!(o.discount_codes));
    let placed = {
        let mut p = base.clone();
        p.insert("items".into(), json!(items));
        p.insert("item_names".into(), json!(o.lines.iter().filter_map(|l| l.title.clone()).collect::<Vec<_>>()));
        p.insert("item_count".into(), json!(o.lines.iter().map(|l| l.quantity).sum::<i64>()));
        p.insert("total".into(), json!(o.total));
        for (k, v) in [("subtotal", o.subtotal), ("discount_total", o.discounts), ("shipping_total", o.shipping), ("tax_total", o.tax)] {
            if let Some(v) = v {
                p.insert(k.into(), json!(v));
            }
        }
        if let Some(u) = &o.status_url {
            p.insert("order_status_url".into(), json!(u));
        }
        if let Some(f) = &o.financial_status {
            p.insert("financial_status".into(), json!(f));
        }
        if let Some(f) = &o.fulfillment_status {
            p.insert("fulfillment_status".into(), json!(f));
        }
        Value::Object(p)
    };
    let total = o.total;
    let refunds = o.refunds.clone();
    let merge_order = |old: &Value| -> (Value, Option<f64>) {
        let (r, lines) = merged_refunds(old, &refunds);
        let was_cancelled = old["cancelled"].as_bool() == Some(true);
        let refunded = round2(sum_obj(&r));
        let mut p = placed.clone();
        let is_cancelled = cancelled || was_cancelled;
        p["refunds"] = Value::Object(r);
        p["line_refunds"] = Value::Object(lines);
        p["refunded_total"] = json!(refunded);
        p["cancelled"] = json!(is_cancelled);
        let value = if is_cancelled { 0.0 } else { round2((total - refunded).max(0.0)) };
        (p, Some(value))
    };
    let (props, value) = merge_order(&json!({}));
    events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Placed Order", unique_id: format!("order:{}", o.id), properties: props, value, at: o.placed_at() }, cx.quiet, Some(&merge_order)).await?;

    // Ordered Product, one per line.
    let fully_refunded_unknown_lines = |r: &Map<String, Value>, l: &Map<String, Value>| l.is_empty() && sum_obj(r) >= total - 0.005 && total > 0.0;
    for (l, item) in o.lines.iter().zip(items.iter()) {
        let mut p = base.clone();
        if let Some(obj) = item.as_object() {
            for (k, v) in obj {
                p.insert(k.clone(), v.clone());
            }
        }
        p.insert("line_item_id".into(), json!(l.id.to_string()));
        let line_total = l.total();
        let lid = l.id;
        let props = Value::Object(p);
        let merge_line = |old: &Value| -> (Value, Option<f64>) {
            let (r, lines) = merged_refunds(old, &refunds);
            let is_cancelled = cancelled || old["cancelled"].as_bool() == Some(true);
            let refunded = line_refunded(&lines, lid);
            let mut p = props.clone();
            let value = if is_cancelled || fully_refunded_unknown_lines(&r, &lines) { 0.0 } else { round2((line_total - refunded).max(0.0)) };
            p["refunds"] = Value::Object(r);
            p["line_refunds"] = Value::Object(lines.into_iter().filter(|(k, _)| *k == lid.to_string()).collect());
            p["cancelled"] = json!(is_cancelled);
            (p, Some(value))
        };
        let (props0, value0) = merge_line(&json!({}));
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Ordered Product", unique_id: format!("order:{}:{}", o.id, l.id), properties: props0, value: value0, at: o.placed_at() }, cx.quiet, Some(&merge_line)).await?;
    }

    let order_props = || {
        let mut p = base.clone();
        p.insert("items".into(), json!(items));
        p.insert("item_names".into(), json!(o.lines.iter().filter_map(|l| l.title.clone()).collect::<Vec<_>>()));
        p.insert("total".into(), json!(o.total));
        p
    };
    // Shipments.
    let fulfilled_status = o.fulfillment_status.as_deref().map(str::to_ascii_lowercase);
    let shipped: Vec<_> = o.fulfillments.iter().filter(|f| !matches!(f.2.as_deref(), Some("cancelled" | "failure" | "error"))).collect();
    if fulfilled_status.as_deref() == Some("fulfilled") {
        let at = shipped.iter().filter_map(|f| f.1).max().or(o.updated_at).unwrap_or_else(Utc::now);
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Fulfilled Order", unique_id: format!("order:{}", o.id), properties: Value::Object(order_props()), value: Some(o.total), at }, cx.quiet, None).await?;
    } else if !shipped.is_empty() {
        for f in &shipped {
            let mut p = order_props();
            p.insert("fulfillment_id".into(), json!(f.0.to_string()));
            events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Fulfilled Partial Order", unique_id: format!("fulfillment:{}", f.0), properties: Value::Object(p), value: None, at: f.1.or(o.updated_at).unwrap_or_else(Utc::now) }, cx.quiet, None).await?;
        }
    }
    if let Some(at) = o.cancelled_at {
        let mut p = order_props();
        if let Some(r) = &o.cancel_reason {
            p.insert("reason".into(), json!(r));
        }
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Cancelled Order", unique_id: format!("order:{}", o.id), properties: Value::Object(p), value: Some(o.total), at }, cx.quiet, None).await?;
    }
    for r in &o.refunds {
        refunded_event(cx, profile_id, &base, o.id, r, o.updated_at).await?;
    }
    redeem(db, cx.account_id, o).await?;
    if !cx.quiet {
        sqlx::query("UPDATE shopify_shops SET last_order_at = GREATEST(COALESCE(last_order_at, 'epoch'), now()) WHERE account_id = $1").bind(cx.account_id).execute(db).await?;
    }
    Ok(())
}

async fn refunded_event(cx: &mut Ctx<'_>, profile_id: Uuid, base: &Map<String, Value>, order_id: i64, r: &Refund, fallback: Option<DateTime<Utc>>) -> anyhow::Result<()> {
    let mut p = base.clone();
    p.insert("refund_id".into(), json!(r.id.to_string()));
    p.insert("order_id".into(), json!(order_id.to_string()));
    p.insert("refund_amount".into(), json!(round2(r.amount)));
    if !r.lines.is_empty() {
        p.insert("refunded_line_item_ids".into(), json!(r.lines.keys().map(|k| k.to_string()).collect::<Vec<_>>()));
    }
    events::put(
        &cx.state.db,
        cx.metrics,
        cx.account_id,
        ServerEvent { profile_id, metric: "Refunded Order", unique_id: format!("refund:{}", r.id), properties: Value::Object(p), value: Some(round2(r.amount)), at: r.at.or(fallback).unwrap_or_else(Utc::now) },
        cx.quiet,
        None,
    )
    .await?;
    Ok(())
}

/// A `refunds/create` webhook: record the refund and take it off the order's value now (the
/// `orders/updated` that follows carries the same refund and changes nothing more). An order we
/// haven't seen yet is left for that order webhook.
pub async fn apply_refund(cx: &mut Ctx<'_>, body: &Value) -> anyhow::Result<()> {
    let Some(r) = Refund::from_webhook(body) else { return Ok(()) };
    let Some(order_id) = num_id(&body["order_id"]) else { return Ok(()) };
    let db = &cx.state.db;
    let placed_metric = cx.metrics.id(db, cx.account_id, "Placed Order").await?;
    let row: Option<(Uuid, Uuid, Value, Option<bool>)> = sqlx::query_as("SELECT id, profile_id, properties, from_browser FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3")
        .bind(cx.account_id)
        .bind(placed_metric)
        .bind(format!("order:{order_id}"))
        .fetch_optional(db)
        .await?;
    let Some((event_id, profile_id, props, from_browser)) = row else { return Ok(()) };
    if from_browser != Some(false) {
        // Only the browser has told us about this order so far: wait for the order itself.
        return Ok(());
    }
    let (refunds, lines) = merged_refunds(&props, std::slice::from_ref(&r));
    let total = props["total"].as_f64().unwrap_or(0.0);
    let refunded = round2(sum_obj(&refunds));
    let cancelled = props["cancelled"].as_bool() == Some(true);
    let mut np = props.clone();
    np["refunds"] = Value::Object(refunds.clone());
    np["line_refunds"] = Value::Object(lines.clone());
    np["refunded_total"] = json!(refunded);
    let value = if cancelled { 0.0 } else { round2((total - refunded).max(0.0)) };
    if np != props {
        sqlx::query("UPDATE events SET properties = $2, value = $3 WHERE id = $1").bind(event_id).bind(&np).bind(value).execute(db).await?;
        // Its lines.
        let line_metric = cx.metrics.id(db, cx.account_id, "Ordered Product").await?;
        let line_rows: Vec<(Uuid, Value)> = sqlx::query_as("SELECT id, properties FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id LIKE $3")
            .bind(cx.account_id)
            .bind(line_metric)
            .bind(format!("order:{order_id}:%"))
            .fetch_all(db)
            .await?;
        for (lid_event, lp) in line_rows {
            let Some(lid) = lp["line_item_id"].as_str().and_then(|x| x.parse::<i64>().ok()) else { continue };
            let line_total = (lp["price"].as_f64().unwrap_or(0.0) * lp["quantity"].as_f64().unwrap_or(1.0)).max(0.0);
            let base_value: f64 = sqlx::query_scalar::<_, Option<f64>>("SELECT value FROM events WHERE id = $1").bind(lid_event).fetch_one(db).await?.unwrap_or(line_total);
            let old_refunded = line_refunded(&lp["line_refunds"].as_object().cloned().unwrap_or_default(), lid);
            let new_refunded = line_refunded(&lines, lid);
            let v = if cancelled { 0.0 } else { round2((base_value + old_refunded - new_refunded).max(0.0)) };
            let mut nlp = lp.clone();
            nlp["refunds"] = Value::Object(refunds.clone());
            nlp["line_refunds"] = Value::Object(lines.iter().filter(|(k, _)| **k == lid.to_string()).map(|(k, v)| (k.clone(), v.clone())).collect());
            sqlx::query("UPDATE events SET properties = $2, value = $3 WHERE id = $1").bind(lid_event).bind(&nlp).bind(v).execute(db).await?;
        }
    }
    let mut base = Map::new();
    base.insert("tracked_by".into(), json!("shopify"));
    for k in ["order_name", "order_number", "currency"] {
        if let Some(v) = props.get(k) {
            base.insert(k.into(), v.clone());
        }
    }
    refunded_event(cx, profile_id, &base, order_id, &r, None).await
}

/// Mark codes we created as used when an order (trusted: it came from Shopify's servers) carries them.
async fn redeem(db: &sqlx::PgPool, account_id: Uuid, o: &Order) -> sqlx::Result<()> {
    if o.discount_codes.is_empty() || o.cancelled_at.is_some() {
        return Ok(());
    }
    let codes: Vec<String> = o.discount_codes.iter().map(|c| c.to_ascii_uppercase()).collect();
    sqlx::query("UPDATE shopify_discount_codes SET redeemed_at = COALESCE(redeemed_at, $3), order_id = COALESCE(order_id, $4) WHERE account_id = $1 AND upper(code) = ANY($2)")
        .bind(account_id)
        .bind(&codes)
        .bind(o.placed_at())
        .bind(o.id)
        .execute(db)
        .await?;
    Ok(())
}

/// `checkouts/create` and `checkouts/update`: Started Checkout once the buyer is known, with the
/// same id the pixel uses (`checkout:<token>`). Completed checkouts are orders, not abandoned carts.
pub async fn apply_checkout(cx: &mut Ctx<'_>, v: &Value) -> anyhow::Result<()> {
    let Some(token) = s(&v["token"]).filter(|t| t.len() <= 200) else { return Ok(()) };
    if !v["completed_at"].is_null() {
        return Ok(());
    }
    let c = &v["customer"];
    let email = s(&v["email"]).or_else(|| s(&c["email"]));
    let phone = s(&v["phone"]).or_else(|| s(&c["phone"])).or_else(|| s(&v["shipping_address"]["phone"]));
    let db = &cx.state.db;
    let Some(profile_id) = profile_for(db, cx.account_id, num_id(&c["id"]), email.as_deref(), phone.as_deref(), s(&c["first_name"]).as_deref(), s(&c["last_name"]).as_deref()).await? else {
        return Ok(());
    };
    let lines: Vec<Line> = v["line_items"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(MAX_LINES)
                .map(|l| Line {
                    id: 0,
                    product_id: num_id(&l["product_id"]),
                    variant_id: num_id(&l["variant_id"]),
                    title: s(&l["title"]),
                    variant_title: s(&l["variant_title"]),
                    sku: s(&l["sku"]),
                    vendor: s(&l["vendor"]),
                    quantity: l["quantity"].as_i64().unwrap_or(1),
                    price: amount(&l["price"]),
                    discount: 0.0,
                    image_url: None,
                })
                .collect()
        })
        .unwrap_or_default();
    let pids: Vec<String> = lines.iter().filter_map(|l| l.product_id.map(|p| p.to_string())).collect();
    let links = super::products::links(db, cx.account_id, &pids).await?;
    let order = Order { lines, ..Default::default() };
    let items = items(&order, &links);
    let total = amount(&v["total_price"]);
    let mut p = Map::new();
    p.insert("tracked_by".into(), json!("shopify"));
    p.insert("items".into(), json!(items));
    p.insert("item_names".into(), json!(order.lines.iter().filter_map(|l| l.title.clone()).collect::<Vec<_>>()));
    p.insert("item_count".into(), json!(order.lines.iter().map(|l| l.quantity).sum::<i64>()));
    if let Some(t) = total {
        p.insert("total".into(), json!(t));
    }
    if let Some(c) = s(&v["currency"]).or_else(|| s(&v["presentment_currency"])) {
        p.insert("currency".into(), json!(c));
    }
    if let Some(u) = s(&v["abandoned_checkout_url"]).filter(|u| u.starts_with("https://")) {
        p.insert("checkout_url".into(), json!(u));
    }
    p.insert("discount_codes".into(), json!(v["discount_codes"].as_array().map(|a| a.iter().filter_map(|d| s(&d["code"])).take(20).collect::<Vec<_>>()).unwrap_or_default()));
    let at = t(&v["created_at"]).unwrap_or_else(Utc::now);
    events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Started Checkout", unique_id: format!("checkout:{token}"), properties: Value::Object(p), value: total, at }, cx.quiet, None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refunds_merge_once() {
        let r1 = Refund { id: 1, amount: 10.0, lines: [(7, 10.0)].into_iter().collect(), ..Default::default() };
        let (r, l) = merged_refunds(&json!({}), std::slice::from_ref(&r1));
        assert_eq!(sum_obj(&r), 10.0);
        let old = json!({"refunds": r, "line_refunds": l});
        // The same refund again (a redelivery, or the order webhook that follows) adds nothing.
        let (r2, l2) = merged_refunds(&old, &[r1.clone(), Refund { id: 2, amount: 5.5, ..Default::default() }]);
        assert_eq!(sum_obj(&r2), 15.5);
        assert_eq!(line_refunded(&l2, 7), 10.0);
        // An older payload without the refunds doesn't add them back.
        let (r3, _) = merged_refunds(&json!({"refunds": r2}), &[]);
        assert_eq!(sum_obj(&r3), 15.5);
    }

    #[test]
    fn webhook_refund_amount() {
        let r = Refund::from_webhook(&json!({"id": 5, "created_at": "2026-11-01T10:00:00Z", "transactions": [
            {"kind": "refund", "status": "success", "amount": "12.00"}, {"kind": "refund", "status": "failure", "amount": "99.00"}, {"kind": "sale", "amount": "1.00"}],
            "refund_line_items": [{"line_item_id": 7, "subtotal": 10.0}]})).unwrap();
        assert_eq!(r.amount, 12.0);
        assert_eq!(r.lines.get(&7), Some(&10.0));
    }
}

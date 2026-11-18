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
//! **Money.** Every amount is the shop's currency (`shop_money` in webhooks, `shopMoney` in GraphQL),
//! kept exactly as Shopify gives it (decimal arithmetic, no rounding), and the event's `currency` is
//! the shop's. A checkout whose buyer paid in another currency is recorded without a value rather
//! than mixing the two.
//!
//! **Refunds and cancellations.** Revenue everywhere (overview, reports, attribution, cohorts, segment
//! totals and predictions) is the sum of `Placed Order` values. So instead of adding negative events
//! that every one of those would have to learn about, the `Placed Order` event's value is kept equal
//! to what the store actually kept: the total less what was refunded, and 0 when the order is
//! cancelled. What was refunded is always read from Shopify's GraphQL Admin API for the whole order
//! (every refund, counting only refund transactions that succeeded, in shop money), after any
//! refund webhook or order webhook that mentions refunds, and after the history import. Each read
//! replaces what was known, so a fuller later read corrects an earlier one; a read older than the
//! one applied (Shopify's `updatedAt`) changes nothing. An order snapshot older than the one already
//! applied changes nothing either. The order's value and its lines' values are rewritten together in
//! one transaction.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::client::{self, ApiError};
use super::customers::{dec, gid, num_id, parse_time, profile_for, to_f64};
use super::events::{self, Metrics, ServerEvent};
use super::shop::ShopRow;
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
    pub price: Option<Decimal>,
    pub discount: Decimal,
    pub image_url: Option<String>,
}

impl Line {
    fn total(&self) -> Decimal {
        (self.price.unwrap_or_default() * Decimal::from(self.quantity) - self.discount).max(Decimal::ZERO)
    }
}

/// One refund as Shopify's GraphQL Admin API reports it now: the sum of its refund transactions
/// that succeeded (shop money), and what it refunded per line item.
#[derive(Debug, Clone, Default)]
pub struct Refund {
    pub id: i64,
    pub at: Option<DateTime<Utc>>,
    pub amount: Decimal,
    pub lines: BTreeMap<i64, Decimal>,
}

#[derive(Debug, Clone, Default)]
pub struct Order {
    pub id: i64,
    pub name: Option<String>,
    pub number: Option<i64>,
    /// The order's own contact (what the buyer gave for this order).
    pub email: Option<String>,
    pub phone: Option<String>,
    /// The nested customer record's contact: used only when the order has none of its own.
    pub customer_email: Option<String>,
    pub customer_phone: Option<String>,
    pub customer_id: Option<i64>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub processed_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub cancel_reason: Option<String>,
    /// The shop's currency.
    pub currency: Option<String>,
    pub total: Decimal,
    pub subtotal: Option<Decimal>,
    pub discounts: Option<Decimal>,
    pub shipping: Option<Decimal>,
    pub tax: Option<Decimal>,
    pub discount_codes: Vec<String>,
    pub financial_status: Option<String>,
    pub fulfillment_status: Option<String>,
    pub lines: Vec<Line>,
    /// The payload mentions refunds: they're read from the Admin API (see the module docs).
    pub has_refunds: bool,
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

/// A REST money set's shop amount (`{"shop_money": {"amount": "1.00"}}`).
fn shop_money(v: &Value) -> Option<Decimal> {
    dec(&v["shop_money"]["amount"])
}

impl Order {
    /// An order webhook body (`orders/*`), which uses the REST resource's field names. Amounts come
    /// from the `*_set.shop_money` fields.
    pub fn from_webhook(v: &Value) -> Option<Order> {
        let id = num_id(&v["id"])?;
        let c = &v["customer"];
        let lines = v["line_items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(MAX_LINES)
                    .filter_map(|l| {
                        let allocated: Option<Decimal> = l["discount_allocations"].as_array().filter(|d| !d.is_empty()).map(|d| d.iter().filter_map(|x| shop_money(&x["amount_set"]).or_else(|| dec(&x["amount"]))).sum());
                        let discount = allocated.or_else(|| shop_money(&l["total_discount_set"])).or_else(|| dec(&l["total_discount"])).unwrap_or_default();
                        Some(Line {
                            id: num_id(&l["id"])?,
                            product_id: num_id(&l["product_id"]),
                            variant_id: num_id(&l["variant_id"]),
                            title: s(&l["title"]).or_else(|| s(&l["name"])),
                            variant_title: s(&l["variant_title"]),
                            sku: s(&l["sku"]),
                            vendor: s(&l["vendor"]),
                            quantity: l["quantity"].as_i64().unwrap_or(1),
                            price: shop_money(&l["price_set"]).or_else(|| dec(&l["price"])),
                            discount,
                            image_url: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fulfillments = v["fulfillments"]
            .as_array()
            .map(|a| a.iter().take(MAX_LINES).filter_map(|f| Some((num_id(&f["id"])?, t(&f["created_at"]), s(&f["status"])))).collect())
            .unwrap_or_default();
        let money = |set: &str, plain: &str| shop_money(&v[set]).or_else(|| dec(&v[plain]));
        Some(Order {
            id,
            name: s(&v["name"]),
            number: v["order_number"].as_i64(),
            email: s(&v["email"]).or_else(|| s(&v["contact_email"])),
            phone: s(&v["phone"]),
            customer_email: s(&c["email"]),
            customer_phone: s(&c["phone"]),
            customer_id: num_id(&c["id"]),
            first_name: s(&c["first_name"]),
            last_name: s(&c["last_name"]),
            created_at: t(&v["created_at"]),
            processed_at: t(&v["processed_at"]),
            updated_at: t(&v["updated_at"]),
            cancelled_at: t(&v["cancelled_at"]),
            cancel_reason: s(&v["cancel_reason"]),
            currency: s(&v["total_price_set"]["shop_money"]["currency_code"]).or_else(|| s(&v["currency"])),
            total: money("total_price_set", "total_price").unwrap_or_default(),
            subtotal: money("subtotal_price_set", "subtotal_price"),
            discounts: money("total_discounts_set", "total_discounts"),
            shipping: shop_money(&v["total_shipping_price_set"]),
            tax: money("total_tax_set", "total_tax"),
            discount_codes: v["discount_codes"].as_array().map(|a| a.iter().filter_map(|d| s(&d["code"])).take(20).collect()).unwrap_or_default(),
            financial_status: s(&v["financial_status"]),
            fulfillment_status: s(&v["fulfillment_status"]),
            lines,
            has_refunds: v["refunds"].as_array().is_some_and(|r| !r.is_empty()),
            fulfillments,
            status_url: s(&v["order_status_url"]),
            test: v["test"].as_bool().unwrap_or(false),
        })
    }

    /// An order from a bulk read of the GraphQL Admin API, with its line item lines.
    pub fn from_graphql(v: &Value, line_items: &[Value]) -> Option<Order> {
        let id = num_id(&v["id"])?;
        let money = |k: &str| dec(&v[k]["shopMoney"]["amount"]);
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
                    price: dec(&l["originalUnitPriceSet"]["shopMoney"]["amount"]),
                    discount: dec(&l["totalDiscountSet"]["shopMoney"]["amount"]).unwrap_or_default(),
                    image_url: s(&l["image"]["url"]),
                })
            })
            .collect();
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
            customer_email: None,
            customer_phone: None,
            customer_id: num_id(&v["customer"]["id"]),
            first_name: s(&v["customer"]["firstName"]),
            last_name: s(&v["customer"]["lastName"]),
            created_at: t(&v["createdAt"]),
            processed_at: t(&v["processedAt"]),
            updated_at: t(&v["updatedAt"]),
            cancelled_at: t(&v["cancelledAt"]),
            cancel_reason: s(&v["cancelReason"]).map(|x| x.to_ascii_lowercase()),
            currency: s(&v["totalPriceSet"]["shopMoney"]["currencyCode"]).or_else(|| s(&v["currencyCode"])),
            total: money("totalPriceSet").unwrap_or_default(),
            subtotal: money("subtotalPriceSet"),
            discounts: money("totalDiscountsSet"),
            shipping: money("totalShippingPriceSet"),
            tax: money("totalTaxSet"),
            discount_codes: v["discountCodes"].as_array().map(|a| a.iter().filter_map(s).take(20).collect()).unwrap_or_default(),
            financial_status: s(&v["displayFinancialStatus"]).map(|x| x.to_ascii_lowercase()),
            fulfillment_status: s(&v["displayFulfillmentStatus"]).map(|x| x.to_ascii_lowercase()),
            lines,
            has_refunds: v["refunds"].as_array().is_some_and(|r| !r.is_empty()),
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
    /// A refund from the Admin API read in [`REFUNDS_QUERY`]: only `REFUND` transactions with status
    /// `SUCCESS` count; a pending, failed or errored refund adds nothing (and none of its lines).
    pub fn from_graphql(r: &Value) -> Option<Refund> {
        let id = num_id(&r["id"])?;
        let amount: Decimal = r["transactions"]["nodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|x| x["kind"].as_str() == Some("REFUND") && x["status"].as_str() == Some("SUCCESS"))
                    .filter_map(|x| dec(&x["amountSet"]["shopMoney"]["amount"]))
                    .sum()
            })
            .unwrap_or_default();
        let mut lines = BTreeMap::new();
        if amount > Decimal::ZERO {
            for l in r["refundLineItems"]["nodes"].as_array().into_iter().flatten().take(MAX_LINES) {
                if let (Some(lid), Some(sub)) = (num_id(&l["lineItem"]["id"]), dec(&l["subtotalSet"]["shopMoney"]["amount"])) {
                    *lines.entry(lid).or_insert(Decimal::ZERO) += sub;
                }
            }
        }
        Some(Refund { id, at: t(&r["createdAt"]), amount, lines })
    }
}

/// Everything about an order's refunds, read for a few orders at a time.
pub const REFUNDS_QUERY: &str = "query($ids: [ID!]!) { nodes(ids: $ids) { ... on Order { id updatedAt
    refunds { id createdAt
        transactions(first: 20) { nodes { kind status amountSet { shopMoney { amount currencyCode } } } }
        refundLineItems(first: 100) { nodes { lineItem { id } subtotalSet { shopMoney { amount } } } } } } } }";

/// Orders read per refunds query.
pub const REFUNDS_PER_QUERY: usize = 5;

/// Refresh-queue kinds for orders: refunds read after a live webhook ("order"), refunds read for an
/// order the history import found (quiet), and an order read again in full for the history import
/// (a line of it arrived after the order itself was saved; quiet).
pub const REFUNDS_LIVE: &str = "order";
pub const REFUNDS_HISTORY: &str = "order_history";
pub const FULL_HISTORY: &str = "order_full";

/// An order read again in full, in the same shape the bulk import reads (lines included).
pub const ORDER_QUERY: &str = "query($ids: [ID!]!) { nodes(ids: $ids) { ... on Order { id name createdAt processedAt updatedAt cancelledAt cancelReason test email phone currencyCode
    displayFinancialStatus displayFulfillmentStatus discountCodes
    totalPriceSet { shopMoney { amount currencyCode } } subtotalPriceSet { shopMoney { amount } } totalDiscountsSet { shopMoney { amount } }
    totalShippingPriceSet { shopMoney { amount } } totalTaxSet { shopMoney { amount } }
    customer { id firstName lastName }
    refunds { id }
    fulfillments { id createdAt status }
    lineItems(first: 250) { nodes { id title variantTitle sku vendor quantity originalUnitPriceSet { shopMoney { amount } }
        totalDiscountSet { shopMoney { amount } } product { id } variant { id } image { url } } } } } }";

/// Read orders again in full and record them (history: quiet).
pub async fn refresh_full(cx: &mut Ctx<'_>, shop: &ShopRow, ids: &[i64]) -> Result<(), ApiError> {
    let gids: Vec<String> = ids.iter().map(|i| gid("Order", *i)).collect();
    let data = client::graphql(cx.state, shop, ORDER_QUERY, json!({"ids": gids})).await?;
    for n in data["nodes"].as_array().into_iter().flatten() {
        let lines: Vec<Value> = n["lineItems"]["nodes"].as_array().cloned().unwrap_or_default();
        if let Some(o) = Order::from_graphql(n, &lines) {
            apply(cx, &o).await.map_err(|_| ApiError::Network("database"))?;
        }
    }
    Ok(())
}

/// An email and phone, either the event's own or its nested customer record's.
#[derive(Clone, Copy)]
pub struct Contact<'a> {
    pub email: Option<&'a str>,
    pub phone: Option<&'a str>,
}

impl Contact<'_> {
    fn any(&self) -> bool {
        self.email.and_then(crate::models::normalize_email).is_some() || self.phone.and_then(crate::models::normalize_phone).is_some()
    }
}

/// Who an order or checkout is for. The event's own email and phone come first (so a new phone on
/// the order is never overridden by a stale email on the customer record). Only when the event has
/// neither is the nested customer's contact used, and then the event counts as having no contact of
/// its own: it's recorded, but nothing is sent because of it. With no contact anywhere, the linked
/// customer's profile, likewise marked.
#[allow(clippy::too_many_arguments)]
pub async fn buyer_for(db: &sqlx::PgPool, account_id: Uuid, shop: &str, customer_id: Option<i64>, own: Contact<'_>, nested: Contact<'_>, first: Option<&str>, last: Option<&str>, at: Option<DateTime<Utc>>) -> anyhow::Result<Option<super::customers::Buyer>> {
    if own.any() {
        return profile_for(db, account_id, shop, customer_id, own.email, own.phone, first, last, at).await;
    }
    let b = profile_for(db, account_id, shop, customer_id, nested.email, nested.phone, first, last, at).await?;
    Ok(b.map(|b| super::customers::Buyer { no_contact: true, ..b }))
}

pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub account_id: Uuid,
    pub shop: &'a str,
    /// The install the work is for: every write goes through a [`super::fence`] for it.
    pub generation: i32,
    pub metrics: &'a mut Metrics,
    /// History import: record everything quietly.
    pub quiet: bool,
}

type Links = std::collections::HashMap<String, (Option<String>, Option<String>, Vec<String>)>;

fn items(o: &Order, links: &Links) -> Vec<Value> {
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
            put("price", json!(l.price.map(to_f64)));
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

// ---------- Refund state kept on the Placed Order event (`shopify_state`) ----------

fn dmap(v: &Value) -> BTreeMap<String, Decimal> {
    v.as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), dec(v)?))).collect()).unwrap_or_default()
}

fn smap(m: &BTreeMap<String, Decimal>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), json!(v.normalize().to_string()))).collect())
}

fn sum(m: &BTreeMap<String, Decimal>) -> Decimal {
    m.values().copied().sum()
}

fn placed_value(total: Decimal, refunded: Decimal, cancelled: bool) -> f64 {
    if cancelled {
        0.0
    } else {
        to_f64((total - refunded).max(Decimal::ZERO))
    }
}

/// Rewrite the order's value and every line's value from the refund state on its Placed Order event,
/// in the caller's transaction (the order's row is locked first, then its lines).
async fn recompute(tx: &mut sqlx::PgConnection, account_id: Uuid, placed_metric: Uuid, line_metric: Uuid, order_id: i64) -> sqlx::Result<()> {
    let row: Option<(Uuid, Value)> = sqlx::query_as("SELECT id, properties FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND from_browser = false FOR UPDATE")
        .bind(account_id)
        .bind(placed_metric)
        .bind(format!("order:{order_id}"))
        .fetch_optional(&mut *tx)
        .await?;
    let Some((event_id, mut props)) = row else { return Ok(()) };
    let st = props["shopify_state"].clone();
    if st.is_null() {
        return Ok(());
    }
    let total = dec(&st["total"]).unwrap_or_default();
    let refunds = dmap(&st["refunds"]);
    let refunded = sum(&refunds);
    let cancelled = props["cancelled"].as_bool() == Some(true);
    props["refunded_total"] = json!(to_f64(refunded));
    props["refunds"] = Value::Object(refunds.iter().map(|(k, v)| (k.clone(), json!(to_f64(*v)))).collect());
    sqlx::query("UPDATE events SET properties = $2, value = $3 WHERE id = $1")
        .bind(event_id)
        .bind(&props)
        .bind(placed_value(total, refunded, cancelled))
        .execute(&mut *tx)
        .await?;
    let line_refunds: BTreeMap<String, BTreeMap<String, Decimal>> = st["line_refunds"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), dmap(v))).collect()).unwrap_or_default();
    // No per-line detail and everything refunded: every line counts 0.
    let all_refunded_unknown_lines = line_refunds.is_empty() && total > Decimal::ZERO && refunded >= total;
    let ids: Vec<String> = st["line_ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).take(MAX_LINES).map(|l| format!("order:{order_id}:{l}")).collect()).unwrap_or_default();
    let lines: Vec<(Uuid, Value)> = sqlx::query_as("SELECT id, properties FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = ANY($3) AND from_browser = false ORDER BY id FOR UPDATE")
        .bind(account_id)
        .bind(line_metric)
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
    for (lid_event, mut lp) in lines {
        let lid = lp["line_item_id"].as_str().unwrap_or("").to_string();
        let line_total = dec(&lp["shopify_state"]["line_total"]).unwrap_or_default();
        let lr = line_refunds.get(&lid).map(sum).unwrap_or_default();
        let value = if cancelled || all_refunded_unknown_lines { 0.0 } else { to_f64((line_total - lr).max(Decimal::ZERO)) };
        lp["cancelled"] = json!(cancelled);
        lp["refunded"] = json!(to_f64(lr));
        sqlx::query("UPDATE events SET properties = $2, value = $3 WHERE id = $1").bind(lid_event).bind(&lp).bind(value).execute(&mut *tx).await?;
    }
    Ok(())
}

async fn recompute_now(cx: &mut Ctx<'_>, order_id: i64) -> anyhow::Result<()> {
    let db = &cx.state.db;
    let placed = cx.metrics.id(db, cx.account_id, "Placed Order").await?;
    let line = cx.metrics.id(db, cx.account_id, "Ordered Product").await?;
    let mut tx = db.begin().await?;
    recompute(&mut tx, cx.account_id, placed, line, order_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Shopify's `updatedAt` of the order snapshot last applied, if any.
async fn applied_at(cx: &mut Ctx<'_>, order_id: i64) -> anyhow::Result<Option<DateTime<Utc>>> {
    let db = &cx.state.db;
    let placed = cx.metrics.id(db, cx.account_id, "Placed Order").await?;
    let st: Option<Option<Value>> = sqlx::query_scalar("SELECT properties->'shopify_state' FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND from_browser = false")
        .bind(cx.account_id)
        .bind(placed)
        .bind(format!("order:{order_id}"))
        .fetch_optional(db)
        .await?;
    Ok(st.flatten().and_then(|s| parse_time(s["updated_at"].as_str())))
}

/// Record everything an order says: Placed Order and Ordered Product (kept up to date), Fulfilled,
/// Partially Fulfilled and Cancelled events, and redemptions of codes we made. Refunds are read from
/// the Admin API afterwards (queued here). Every step is idempotent, so a redelivered or replayed
/// webhook changes nothing, and a snapshot older than the one applied is ignored.
pub async fn apply(cx: &mut Ctx<'_>, o: &Order) -> anyhow::Result<()> {
    if o.test {
        return Ok(());
    }
    // Written inside the fence: not into a newer install, and not after a privacy request erased it.
    let Some(fence) = super::fence::open(&cx.state.db, cx.account_id, cx.shop, cx.generation, o.customer_id, Some(o.id)).await? else { return Ok(()) };
    apply_inner(cx, o).await?;
    fence.done().await?;
    Ok(())
}

async fn apply_inner(cx: &mut Ctx<'_>, o: &Order) -> anyhow::Result<()> {
    if let (Some(seen), Some(this)) = (applied_at(cx, o.id).await?, o.updated_at) {
        if this < seen {
            return Ok(());
        }
    }
    let db = &cx.state.db;
    let own = Contact { email: o.email.as_deref(), phone: o.phone.as_deref() };
    let nested = Contact { email: o.customer_email.as_deref(), phone: o.customer_phone.as_deref() };
    let Some(buyer) = buyer_for(db, cx.account_id, cx.shop, o.customer_id, own, nested, o.first_name.as_deref(), o.last_name.as_deref(), o.updated_at).await? else {
        return Ok(());
    };
    let profile_id = buyer.profile_id;
    // No email or phone on the order: kept for the record, but nothing is sent because of it.
    let quiet = cx.quiet || buyer.no_contact;
    let pids: Vec<String> = o.lines.iter().filter_map(|l| l.product_id.map(|p| p.to_string())).collect();
    let links = super::products::links(db, cx.account_id, &pids).await?;
    let items = items(o, &links);
    let cancelled = o.cancelled_at.is_some();
    let mut base = Map::new();
    base.insert("tracked_by".into(), json!("shopify"));
    base.insert("order_id".into(), json!(o.id.to_string()));
    base.insert("shopify_shop".into(), json!(cx.shop));
    base.insert("shopify_generation".into(), json!(cx.generation));
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            base.insert(k.into(), v);
        }
    };
    put("shopify_customer_id", json!(o.customer_id.map(|c| c.to_string())));
    put("order_name", json!(o.name));
    put("order_number", json!(o.number));
    put("currency", json!(o.currency));
    put("discount_codes", json!(o.discount_codes));
    if buyer.no_contact {
        put("$no_contact", json!(true));
    }
    let placed = {
        let mut p = base.clone();
        p.insert("items".into(), json!(items));
        p.insert("item_names".into(), json!(o.lines.iter().filter_map(|l| l.title.clone()).collect::<Vec<_>>()));
        p.insert("item_count".into(), json!(o.lines.iter().map(|l| l.quantity).sum::<i64>()));
        p.insert("line_item_ids".into(), json!(o.lines.iter().map(|l| l.id.to_string()).collect::<Vec<_>>()));
        p.insert("total".into(), json!(to_f64(o.total)));
        for (k, v) in [("subtotal", o.subtotal), ("discount_total", o.discounts), ("shipping_total", o.shipping), ("tax_total", o.tax)] {
            if let Some(v) = v {
                p.insert(k.into(), json!(to_f64(v)));
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
    let line_ids: Vec<String> = o.lines.iter().map(|l| l.id.to_string()).collect();
    let updated = o.updated_at.map(|u| u.to_rfc3339());
    // The refund state is the Admin API's (see `set_refunds`); an order snapshot keeps it as it is.
    let merge_order = |old: &Value| -> (Value, Option<f64>) {
        let old_st = &old["shopify_state"];
        let is_cancelled = cancelled || old["cancelled"].as_bool() == Some(true);
        let mut p = placed.clone();
        let mut st = json!({"total": total.normalize().to_string(), "updated_at": updated, "line_ids": line_ids});
        for k in ["refunds", "refund_at", "line_refunds", "refunds_read_at"] {
            if !old_st[k].is_null() {
                st[k] = old_st[k].clone();
            }
        }
        let refunded = sum(&dmap(&st["refunds"]));
        p["shopify_state"] = st;
        p["cancelled"] = json!(is_cancelled);
        p["refunded_total"] = json!(to_f64(refunded));
        p["refunds"] = old.get("refunds").cloned().unwrap_or(json!({}));
        (p, Some(placed_value(total, refunded, is_cancelled)))
    };
    let (props, value) = merge_order(&json!({}));
    events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Placed Order", unique_id: format!("order:{}", o.id), properties: props, value, at: o.placed_at() }, quiet, Some(&merge_order)).await?;

    // Ordered Product, one per line. Values are settled with the order's in `recompute`.
    for (l, item) in o.lines.iter().zip(items.iter()) {
        let mut p = base.clone();
        if let Some(obj) = item.as_object() {
            for (k, v) in obj {
                p.insert(k.clone(), v.clone());
            }
        }
        p.insert("line_item_id".into(), json!(l.id.to_string()));
        p.insert("shopify_state".into(), json!({"line_total": l.total().normalize().to_string()}));
        let line_total = l.total();
        let props = Value::Object(p);
        let merge_line = |old: &Value| -> (Value, Option<f64>) {
            // Keep what `recompute` settled last time, so a replay writes nothing.
            let mut p = props.clone();
            for k in ["cancelled", "refunded"] {
                if !old[k].is_null() {
                    p[k] = old[k].clone();
                }
            }
            let was_cancelled = cancelled || old["cancelled"].as_bool() == Some(true);
            let refunded = dec(&old["refunded"]).unwrap_or_default();
            (p, Some(if was_cancelled { 0.0 } else { to_f64((line_total - refunded).max(Decimal::ZERO)) }))
        };
        let (props0, value0) = merge_line(&json!({}));
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Ordered Product", unique_id: format!("order:{}:{}", o.id, l.id), properties: props0, value: value0, at: o.placed_at() }, quiet, Some(&merge_line)).await?;
    }
    recompute_now(cx, o.id).await?;

    let db = &cx.state.db;
    let order_props = || {
        let mut p = base.clone();
        p.insert("items".into(), json!(items));
        p.insert("item_names".into(), json!(o.lines.iter().filter_map(|l| l.title.clone()).collect::<Vec<_>>()));
        p.insert("total".into(), json!(to_f64(o.total)));
        p
    };
    // Shipments.
    let fulfilled_status = o.fulfillment_status.as_deref().map(str::to_ascii_lowercase);
    let shipped: Vec<_> = o.fulfillments.iter().filter(|f| !matches!(f.2.as_deref(), Some("cancelled" | "failure" | "error"))).collect();
    if fulfilled_status.as_deref() == Some("fulfilled") {
        let at = shipped.iter().filter_map(|f| f.1).max().or(o.updated_at).unwrap_or_else(Utc::now);
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Fulfilled Order", unique_id: format!("order:{}", o.id), properties: Value::Object(order_props()), value: Some(to_f64(o.total)), at }, quiet, None).await?;
    } else if !shipped.is_empty() {
        for f in &shipped {
            let mut p = order_props();
            p.insert("fulfillment_id".into(), json!(f.0.to_string()));
            events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Fulfilled Partial Order", unique_id: format!("fulfillment:{}", f.0), properties: Value::Object(p), value: None, at: f.1.or(o.updated_at).unwrap_or_else(Utc::now) }, quiet, None).await?;
        }
    }
    if let Some(at) = o.cancelled_at {
        let mut p = order_props();
        if let Some(r) = &o.cancel_reason {
            p.insert("reason".into(), json!(r));
        }
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Cancelled Order", unique_id: format!("order:{}", o.id), properties: Value::Object(p), value: Some(to_f64(o.total)), at }, quiet, None).await?;
    }
    if o.has_refunds {
        // Found by the history import: its refunds are history too (no refund flows start).
        let kind = if cx.quiet { REFUNDS_HISTORY } else { REFUNDS_LIVE };
        super::products::queue_refresh(db, cx.account_id, kind, o.id).await?;
    }
    redeem(db, cx.account_id, cx.shop, o).await?;
    if !cx.quiet {
        sqlx::query("UPDATE shopify_shops SET last_order_at = GREATEST(COALESCE(last_order_at, 'epoch'), now()) WHERE account_id = $1").bind(cx.account_id).execute(db).await?;
    }
    Ok(())
}

/// A `refunds/create` webhook: read the order's refunds from the Admin API (its payload has the
/// transactions in the buyer's currency, so it isn't used for amounts).
pub async fn apply_refund(cx: &mut Ctx<'_>, body: &Value) -> anyhow::Result<()> {
    if let Some(order_id) = num_id(&body["order_id"]) {
        super::products::queue_refresh(&cx.state.db, cx.account_id, REFUNDS_LIVE, order_id).await?;
    }
    Ok(())
}

/// Read the refunds of these orders and apply them ([`set_refunds`]).
pub async fn refresh_refunds(cx: &mut Ctx<'_>, shop: &ShopRow, ids: &[i64]) -> Result<(), ApiError> {
    let gids: Vec<String> = ids.iter().map(|i| gid("Order", *i)).collect();
    let data = client::graphql(cx.state, shop, REFUNDS_QUERY, json!({"ids": gids})).await?;
    for n in data["nodes"].as_array().into_iter().flatten() {
        let Some(order_id) = num_id(&n["id"]) else { continue };
        let refunds: Vec<Refund> = n["refunds"].as_array().map(|a| a.iter().take(MAX_LINES).filter_map(Refund::from_graphql).collect()).unwrap_or_default();
        set_refunds(cx, order_id, t(&n["updatedAt"]), &refunds).await.map_err(|_| ApiError::Network("database"))?;
    }
    Ok(())
}

/// Replace the order's refunds with this full, current list (read at Shopify's `updated_at`), and
/// rewrite the order's and its lines' values in the same transaction. A read older than the one
/// applied changes nothing. Then each refund's Refunded Order event is recorded or corrected.
pub async fn set_refunds(cx: &mut Ctx<'_>, order_id: i64, updated_at: Option<DateTime<Utc>>, refunds: &[Refund]) -> anyhow::Result<()> {
    let Some(fence) = super::fence::open(&cx.state.db, cx.account_id, cx.shop, cx.generation, None, Some(order_id)).await? else { return Ok(()) };
    set_refunds_inner(cx, order_id, updated_at, refunds).await?;
    fence.done().await?;
    Ok(())
}

async fn set_refunds_inner(cx: &mut Ctx<'_>, order_id: i64, updated_at: Option<DateTime<Utc>>, refunds: &[Refund]) -> anyhow::Result<()> {
    let db = &cx.state.db;
    let placed = cx.metrics.id(db, cx.account_id, "Placed Order").await?;
    let line = cx.metrics.id(db, cx.account_id, "Ordered Product").await?;
    let mut tx = db.begin().await?;
    let row: Option<(Uuid, Uuid, Value)> = sqlx::query_as("SELECT id, profile_id, properties FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND from_browser = false FOR UPDATE")
        .bind(cx.account_id)
        .bind(placed)
        .bind(format!("order:{order_id}"))
        .fetch_optional(&mut *tx)
        .await?;
    // Not seen yet (or only the browser's copy): the order webhook will bring it, and read refunds again.
    let Some((event_id, profile_id, mut props)) = row.filter(|r| !r.2["shopify_state"].is_null()) else { return Ok(()) };
    let st = &mut props["shopify_state"];
    if let (Some(read), Some(this)) = (parse_time(st["refunds_read_at"].as_str()), updated_at) {
        if this < read {
            return Ok(());
        }
    }
    let mut amounts = BTreeMap::new();
    let mut at = Map::new();
    let mut lines: BTreeMap<String, BTreeMap<String, Decimal>> = BTreeMap::new();
    for r in refunds {
        let rid = r.id.to_string();
        amounts.insert(rid.clone(), r.amount);
        if let Some(a) = r.at {
            at.insert(rid.clone(), json!(a.to_rfc3339()));
        }
        for (lid, a) in &r.lines {
            lines.entry(lid.to_string()).or_default().insert(rid.clone(), *a);
        }
    }
    st["refunds"] = smap(&amounts);
    st["refund_at"] = Value::Object(at);
    st["line_refunds"] = Value::Object(lines.iter().map(|(k, v)| (k.clone(), smap(v))).collect());
    st["refunds_read_at"] = json!(updated_at.map(|u| u.to_rfc3339()));
    sqlx::query("UPDATE events SET properties = $2 WHERE id = $1").bind(event_id).bind(&props).execute(&mut *tx).await?;
    recompute(&mut tx, cx.account_id, placed, line, order_id).await?;
    tx.commit().await?;

    let refunded_metric = cx.metrics.id(db, cx.account_id, "Refunded Order").await?;
    let mut base = Map::new();
    for k in ["tracked_by", "order_name", "order_number", "currency", "shopify_shop", "shopify_generation", "shopify_customer_id", "$no_contact"] {
        if let Some(v) = props.get(k) {
            base.insert(k.into(), v.clone());
        }
    }
    base.insert("order_id".into(), json!(order_id.to_string()));
    let quiet = cx.quiet || props["$no_contact"].as_bool() == Some(true);
    for r in refunds {
        let unique = format!("refund:{}", r.id);
        if r.amount <= Decimal::ZERO {
            // Nothing refunded (yet): no event, and one recorded earlier now counts 0.
            sqlx::query("UPDATE events SET value = 0 WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND value IS DISTINCT FROM 0")
                .bind(cx.account_id)
                .bind(refunded_metric)
                .bind(&unique)
                .execute(db)
                .await?;
            continue;
        }
        let mut p = base.clone();
        p.insert("refund_id".into(), json!(r.id.to_string()));
        p.insert("refund_amount".into(), json!(to_f64(r.amount)));
        if !r.lines.is_empty() {
            p.insert("refunded_line_item_ids".into(), json!(r.lines.keys().map(|k| k.to_string()).collect::<Vec<_>>()));
        }
        let at = r.at.or(updated_at).unwrap_or_else(Utc::now);
        events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id, metric: "Refunded Order", unique_id: unique, properties: Value::Object(p), value: Some(to_f64(r.amount)), at }, quiet, None).await?;
    }
    Ok(())
}

/// Mark codes we created in this store as used when an order (trusted: it came from Shopify's
/// servers) carries them.
async fn redeem(db: &sqlx::PgPool, account_id: Uuid, shop: &str, o: &Order) -> sqlx::Result<()> {
    if o.discount_codes.is_empty() || o.cancelled_at.is_some() {
        return Ok(());
    }
    let codes: Vec<String> = o.discount_codes.iter().map(|c| c.to_ascii_uppercase()).collect();
    sqlx::query(
        "UPDATE shopify_discount_codes SET redeemed_at = COALESCE(redeemed_at, $3), order_id = COALESCE(order_id, $4)
         WHERE account_id = $1 AND upper(code) = ANY($2) AND shop_domain = $5",
    )
    .bind(account_id)
    .bind(&codes)
    .bind(o.placed_at())
    .bind(o.id)
    .bind(shop)
    .execute(db)
    .await?;
    Ok(())
}

/// `checkouts/create` and `checkouts/update`: Started Checkout with the same id the pixel uses
/// (`checkout:<token>`), for the checkout's own contact. Completed checkouts are orders, not
/// abandoned carts. The value is the shop-currency total; a checkout in another currency has none.
pub async fn apply_checkout(cx: &mut Ctx<'_>, v: &Value) -> anyhow::Result<()> {
    let Some(token) = s(&v["token"]).filter(|t| t.len() <= 200) else { return Ok(()) };
    if !v["completed_at"].is_null() {
        return Ok(());
    }
    let c = &v["customer"];
    let customer_id = num_id(&c["id"]);
    let Some(fence) = super::fence::open(&cx.state.db, cx.account_id, cx.shop, cx.generation, customer_id, None).await? else { return Ok(()) };
    checkout_inner(cx, v, &token, customer_id).await?;
    fence.done().await?;
    Ok(())
}

async fn checkout_inner(cx: &mut Ctx<'_>, v: &Value, token: &str, customer_id: Option<i64>) -> anyhow::Result<()> {
    let c = &v["customer"];
    let db = &cx.state.db;
    let (email, phone) = (s(&v["email"]), s(&v["phone"]).or_else(|| s(&v["shipping_address"]["phone"])));
    let (c_email, c_phone) = (s(&c["email"]), s(&c["phone"]));
    let own = Contact { email: email.as_deref(), phone: phone.as_deref() };
    let nested = Contact { email: c_email.as_deref(), phone: c_phone.as_deref() };
    let Some(buyer) = buyer_for(db, cx.account_id, cx.shop, customer_id, own, nested, s(&c["first_name"]).as_deref(), s(&c["last_name"]).as_deref(), t(&v["updated_at"])).await? else {
        return Ok(());
    };
    let shop_currency = s(&v["currency"]);
    let same_currency = s(&v["presentment_currency"]).is_none_or(|p| Some(&p) == shop_currency.as_ref());
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
                    price: dec(&l["price"]).filter(|_| same_currency),
                    discount: Decimal::ZERO,
                    image_url: None,
                })
                .collect()
        })
        .unwrap_or_default();
    let pids: Vec<String> = lines.iter().filter_map(|l| l.product_id.map(|p| p.to_string())).collect();
    let links = super::products::links(db, cx.account_id, &pids).await?;
    let order = Order { lines, ..Default::default() };
    let items = items(&order, &links);
    let total = dec(&v["total_price"]).filter(|_| same_currency).map(to_f64);
    let mut p = Map::new();
    p.insert("tracked_by".into(), json!("shopify"));
    p.insert("shopify_shop".into(), json!(cx.shop));
    p.insert("shopify_generation".into(), json!(cx.generation));
    if let Some(cid) = customer_id {
        p.insert("shopify_customer_id".into(), json!(cid.to_string()));
    }
    if buyer.no_contact {
        p.insert("$no_contact".into(), json!(true));
    }
    p.insert("items".into(), json!(items));
    p.insert("item_names".into(), json!(order.lines.iter().filter_map(|l| l.title.clone()).collect::<Vec<_>>()));
    p.insert("item_count".into(), json!(order.lines.iter().map(|l| l.quantity).sum::<i64>()));
    if let Some(t) = total {
        p.insert("total".into(), json!(t));
    }
    if let Some(c) = &shop_currency {
        p.insert("currency".into(), json!(c));
    }
    if let Some(u) = s(&v["abandoned_checkout_url"]).filter(|u| u.starts_with("https://")) {
        p.insert("checkout_url".into(), json!(u));
    }
    p.insert("discount_codes".into(), json!(v["discount_codes"].as_array().map(|a| a.iter().filter_map(|d| s(&d["code"])).take(20).collect::<Vec<_>>()).unwrap_or_default()));
    let at = t(&v["created_at"]).unwrap_or_else(Utc::now);
    let quiet = cx.quiet || buyer.no_contact;
    events::put(db, cx.metrics, cx.account_id, ServerEvent { profile_id: buyer.profile_id, metric: "Started Checkout", unique_id: format!("checkout:{token}"), properties: Value::Object(p), value: total, at }, quiet, None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_successful_refund_transactions_count() {
        let r = Refund::from_graphql(&json!({"id": "gid://shopify/Refund/5", "createdAt": "2026-11-01T10:00:00Z",
            "transactions": {"nodes": [
                {"kind": "REFUND", "status": "SUCCESS", "amountSet": {"shopMoney": {"amount": "12.005", "currencyCode": "KWD"}}},
                {"kind": "REFUND", "status": "FAILURE", "amountSet": {"shopMoney": {"amount": "99.00"}}},
                {"kind": "REFUND", "status": "PENDING", "amountSet": {"shopMoney": {"amount": "7.00"}}},
                {"kind": "SALE", "status": "SUCCESS", "amountSet": {"shopMoney": {"amount": "1.00"}}}]},
            "refundLineItems": {"nodes": [{"lineItem": {"id": "gid://shopify/LineItem/7"}, "subtotalSet": {"shopMoney": {"amount": "10.00"}}}]}})).unwrap();
        assert_eq!(r.amount.to_string(), "12.005");
        assert_eq!(r.lines.get(&7).map(|d| d.to_string()), Some("10.00".into()));
        let pending = Refund::from_graphql(&json!({"id": 6, "transactions": {"nodes": [{"kind": "REFUND", "status": "PENDING", "amountSet": {"shopMoney": {"amount": "7.00"}}}]},
            "refundLineItems": {"nodes": [{"lineItem": {"id": 7}, "subtotalSet": {"shopMoney": {"amount": "7.00"}}}]}})).unwrap();
        assert_eq!(pending.amount, Decimal::ZERO);
        assert!(pending.lines.is_empty(), "a refund that moved no money refunds no lines");
    }
}

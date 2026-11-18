//! What a signed-in shopper sees. Every read takes the [`Shopper`] from the session and filters on
//! its own profile id and account id; nothing here takes a person or an account from a request.
//!
//! **Orders** are built from the order events DumbMail recorded on the server (`from_browser` is
//! false: the API, MCP, Shopify and the store integrations), never from the website snippet, so a
//! shopper can't add orders to their own page. An order is a `Placed Order` event; its status and
//! shipments come from the `Fulfilled Order`, `Fulfilled Partial Order`, `Cancelled Order`,
//! `Refunded Order` and parcel-tracking (`Shipment In Transit`, `Out For Delivery`, `Delivered`,
//! `Shipment Exception`) events that name the same `order_id` (or `order_number`).

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::session::Shopper;
use super::settings::{self, ObjectKind, Settings};
use crate::error::AppResult;
use crate::features::catalog::{learn, recs, store};
use crate::features::objects::types::{Kind, ObjectType};
use crate::models::Account;

/// How far back orders go.
pub const ORDER_MONTHS: i32 = 24;
/// Most order events read for one person.
const MAX_ORDER_EVENTS: i64 = 1000;
/// Orders on the main page; "All orders" shows up to [`MAX_ORDERS`].
pub const ORDERS_FIRST: usize = 10;
pub const MAX_ORDERS: usize = 100;
/// Most records of one object type shown.
const MAX_OBJECTS: i64 = 25;
const PICKS: i64 = 8;
const MAX_STOCK: i64 = 25;

const ORDER_METRICS: &[&str] = &[
    "Placed Order", "Fulfilled Order", "Fulfilled Partial Order", "Cancelled Order", "Refunded Order", "Shipment In Transit", "Out For Delivery", "Delivered", "Shipment Exception",
];

/// Store integrations that record `Placed Order` only once it's paid (see their recipes).
const PAID_WHEN_RECORDED: &[&str] = &["woocommerce:order:", "bigcommerce:order:", "square:order:", "paddle:transaction:"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Placed,
    Paid,
    Fulfilled,
    Delivered,
    Refunded,
    Cancelled,
}

impl Status {
    pub fn key(self) -> &'static str {
        match self {
            Status::Placed => "placed",
            Status::Paid => "paid",
            Status::Fulfilled => "fulfilled",
            Status::Delivered => "delivered",
            Status::Refunded => "refunded",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub name: String,
    pub quantity: Option<f64>,
    pub price_text: Option<String>,
    pub image_url: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Shipment {
    pub carrier: Option<String>,
    pub tracking_number: Option<String>,
    pub tracking_url: Option<String>,
    pub status: Option<&'static str>,
    pub expected: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub id: String,
    pub number: String,
    pub placed_at: DateTime<Utc>,
    pub placed: String,
    pub status: Status,
    /// Some items shipped, not all.
    pub partial: bool,
    pub total_text: Option<String>,
    /// A partial refund (a full one is the `Refunded` status).
    pub refunded_text: Option<String>,
    pub items: Vec<Item>,
    pub item_count: usize,
    pub shipments: Vec<Shipment>,
    /// The `Placed Order` event's properties, where the items are read from.
    pub props: Value,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub type_name: String,
    pub kind: ObjectKind,
    pub title: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Pick {
    pub title: String,
    pub url: Option<String>,
    pub image_url: Option<String>,
    pub price_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StockRequest {
    pub id: Uuid,
    pub title: String,
    pub url: Option<String>,
    pub image_url: Option<String>,
    pub since: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailStatus {
    Subscribed,
    NotSubscribed,
    Unsubscribed,
    /// The address bounced or was reported as spam: nothing can change it here.
    Suppressed,
}

pub struct Prefs {
    pub email: String,
    pub email_status: EmailStatus,
    pub lists: Vec<crate::preferences::PrefList>,
    /// The last digits of their number, when they have one.
    pub phone_hint: Option<String>,
    pub sms_subscribed: bool,
}

/// Everything the hub page shows. A section the store turned off is `None`.
pub struct View {
    pub first_name: Option<String>,
    pub email: String,
    pub orders: Option<Vec<Order>>,
    pub more_orders: bool,
    pub records: Option<Vec<Record>>,
    pub picks: Option<Vec<Pick>>,
    pub stock: Option<Vec<StockRequest>>,
    pub prefs: Option<Prefs>,
}

fn tz(account: &Account) -> chrono_tz::Tz {
    account.timezone.parse().unwrap_or(chrono_tz::America::Los_Angeles)
}

pub fn date(account: &Account, t: DateTime<Utc>) -> String {
    t.with_timezone(&tz(account)).format("%b %-d, %Y").to_string()
}

fn text(v: &Value, k: &str) -> Option<String> {
    learn::text(v, k).map(|s| s.chars().take(200).collect())
}

fn num(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
}

/// Only plain web links (http or https) ever reach the page.
pub fn web_url(u: Option<String>) -> Option<String> {
    u.filter(|u| u.len() <= 2000).and_then(|u| url::Url::parse(&u).ok()).filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some()).map(|u| u.to_string())
}

/// The whole page's data for this shopper, for the sections the store turned on.
pub async fn view(db: &PgPool, shopper: &Shopper, s: &Settings, all_orders: bool) -> AppResult<View> {
    let (first_name,): (Option<String>,) = sqlx::query_as("SELECT first_name FROM profiles WHERE id = $1 AND account_id = $2").bind(shopper.profile_id).bind(shopper.account.id).fetch_one(db).await?;
    let mut v = View { first_name, email: shopper.email.clone(), orders: None, more_orders: false, records: None, picks: None, stock: None, prefs: None };
    if s.sections.orders {
        let mut orders = orders(db, shopper).await?;
        let cap = if all_orders { MAX_ORDERS } else { ORDERS_FIRST };
        v.more_orders = orders.len() > cap;
        orders.truncate(cap);
        fill_items(db, &shopper.account, &mut orders).await?;
        v.orders = Some(orders);
    }
    if s.sections.bookings && !s.object_types.is_empty() {
        v.records = Some(records(db, shopper, s).await?);
    }
    if s.sections.recommendations {
        v.picks = Some(picks(db, shopper).await?);
        v.stock = Some(stock_requests(db, shopper).await?);
    }
    if s.sections.preferences {
        v.prefs = Some(prefs(db, shopper).await?);
    }
    Ok(v)
}

struct Acc {
    order: Order,
    order_number: Option<String>,
    total: Option<f64>,
    currency: Option<String>,
    refunded: f64,
    cancelled: bool,
    fulfilled: bool,
    partial: bool,
    delivered: bool,
    paid: bool,
    shipments: Vec<(String, Shipment)>,
}

fn shipment_status(metric: &str) -> Option<&'static str> {
    match metric {
        "Fulfilled Order" | "Fulfilled Partial Order" => Some("Shipped"),
        "Shipment In Transit" => Some("In transit"),
        "Out For Delivery" => Some("Out for delivery"),
        "Delivered" => Some("Delivered"),
        "Shipment Exception" => Some("Delivery problem"),
        _ => None,
    }
}

fn add_shipment(acc: &mut Acc, metric: &str, p: &Value, account: &Account) {
    let number = text(p, "tracking_number");
    let url = web_url(text(p, "tracking_url").or_else(|| text(p, "tracking_link")));
    if number.is_none() && url.is_none() {
        return;
    }
    let key = number.clone().or_else(|| text(p, "shipment_id")).or_else(|| text(p, "fulfillment_id")).or_else(|| url.clone()).unwrap_or_default();
    let expected = text(p, "expected_delivery").and_then(|d| {
        DateTime::parse_from_rfc3339(&d).ok().map(|t| date(account, t.with_timezone(&Utc))).or_else(|| NaiveDate::parse_from_str(&d[..d.len().min(10)], "%Y-%m-%d").ok().map(|d| d.format("%b %-d, %Y").to_string()))
    });
    let carrier = text(p, "carrier").or_else(|| text(p, "tracking_company")).or_else(|| text(p, "courier"));
    // Events come newest first: the first status seen for a parcel is its latest.
    match acc.shipments.iter_mut().find(|(k, _)| *k == key) {
        Some((_, s)) => {
            s.carrier = s.carrier.take().or(carrier);
            s.tracking_number = s.tracking_number.take().or(number);
            s.tracking_url = s.tracking_url.take().or(url);
            s.expected = s.expected.take().or(expected);
            s.status = s.status.or(shipment_status(metric));
        }
        None => acc.shipments.push((key, Shipment { carrier, tracking_number: number, tracking_url: url, status: shipment_status(metric), expected })),
    }
}

/// This shopper's orders of the last [`ORDER_MONTHS`] months, newest first (items not filled in).
pub async fn orders(db: &PgPool, shopper: &Shopper) -> AppResult<Vec<Order>> {
    let account = &shopper.account;
    let rows: Vec<(Uuid, String, Value, Option<f64>, DateTime<Utc>, Option<String>)> = sqlx::query_as(
        "SELECT e.id, m.name, e.properties, e.value, e.occurred_at, e.unique_id FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.profile_id = $1 AND e.account_id = $2 AND m.account_id = $2 AND m.name = ANY($3) AND e.from_browser = false
           AND e.occurred_at > now() - make_interval(months => $4)
         ORDER BY e.occurred_at DESC LIMIT $5",
    )
    .bind(shopper.profile_id)
    .bind(account.id)
    .bind(ORDER_METRICS)
    .bind(ORDER_MONTHS)
    .bind(MAX_ORDER_EVENTS)
    .fetch_all(db)
    .await?;
    let mut by_id: HashMap<String, Acc> = HashMap::new();
    for (event_id, _, p, value, at, unique) in rows.iter().filter(|r| r.1 == "Placed Order") {
        let id = text(p, "order_id").unwrap_or_else(|| unique.clone().unwrap_or_else(|| event_id.to_string()));
        if by_id.contains_key(&id) {
            continue;
        }
        let order_number = text(p, "order_number");
        let number = text(p, "order_name").or_else(|| order_number.clone().map(|n| if n.starts_with('#') { n } else { format!("#{n}") })).unwrap_or_else(|| id.clone());
        let financial = text(p, "financial_status").map(|f| f.to_lowercase());
        let paid = matches!(financial.as_deref(), Some("paid" | "partially_refunded" | "refunded"))
            || p.get("paid").and_then(Value::as_bool) == Some(true)
            || unique.as_deref().is_some_and(|u| PAID_WHEN_RECORDED.iter().any(|pre| u.starts_with(pre)));
        let fstatus = text(p, "fulfillment_status").map(|f| f.to_lowercase());
        by_id.insert(
            id.clone(),
            Acc {
                order: Order {
                    id: id.clone(),
                    number,
                    placed_at: *at,
                    placed: date(account, *at),
                    status: Status::Placed,
                    partial: false,
                    total_text: None,
                    refunded_text: None,
                    items: Vec::new(),
                    item_count: 0,
                    shipments: Vec::new(),
                    props: p.clone(),
                },
                order_number,
                total: num(p, "total").or(*value),
                currency: text(p, "currency"),
                refunded: num(p, "refunded_total").unwrap_or(0.0),
                cancelled: p.get("cancelled").and_then(Value::as_bool) == Some(true),
                fulfilled: fstatus.as_deref() == Some("fulfilled"),
                partial: fstatus.as_deref() == Some("partial"),
                delivered: false,
                paid,
                shipments: Vec::new(),
            },
        );
    }
    let by_number: HashMap<String, String> = by_id.iter().filter_map(|(id, a)| a.order_number.clone().map(|n| (n.trim_start_matches('#').to_string(), id.clone()))).collect();
    let mut refunds: HashMap<String, f64> = HashMap::new();
    for (_, metric, p, value, _, _) in rows.iter().filter(|r| r.1 != "Placed Order") {
        let key = text(p, "order_id").filter(|k| by_id.contains_key(k)).or_else(|| text(p, "order_number").and_then(|n| by_number.get(n.trim_start_matches('#')).cloned()));
        let Some(acc) = key.as_ref().and_then(|k| by_id.get_mut(k)) else { continue };
        match metric.as_str() {
            "Cancelled Order" => acc.cancelled = true,
            "Refunded Order" => *refunds.entry(acc.order.id.clone()).or_default() += value.or_else(|| num(p, "amount")).unwrap_or(0.0),
            "Fulfilled Order" => acc.fulfilled = true,
            "Fulfilled Partial Order" => acc.partial = true,
            "Delivered" => acc.delivered = true,
            _ => {}
        }
        add_shipment(acc, metric, p, account);
    }
    let mut out: Vec<Order> = by_id
        .into_values()
        .map(|mut a| {
            let refunded = a.refunded.max(refunds.get(&a.order.id).copied().unwrap_or(0.0));
            let fully_refunded = a.total.is_some_and(|t| t > 0.0 && refunded >= t - 0.005);
            a.order.status = if a.cancelled {
                Status::Cancelled
            } else if fully_refunded {
                Status::Refunded
            } else if a.delivered {
                Status::Delivered
            } else if a.fulfilled || a.partial {
                Status::Fulfilled
            } else if a.paid {
                Status::Paid
            } else {
                Status::Placed
            };
            a.order.partial = a.partial && !a.fulfilled && !a.delivered;
            a.order.total_text = a.total.map(|t| store::money(t, a.currency.as_deref()));
            a.order.refunded_text = (refunded > 0.0 && !fully_refunded && !a.cancelled).then(|| store::money(refunded, a.currency.as_deref()));
            let items = learn::event_items("Placed Order", &a.order.props);
            a.order.item_count = items.len();
            a.order.shipments = a.shipments.into_iter().map(|s| s.1).collect();
            a.order.items = items
                .iter()
                .map(|it| Item { name: text(it, "name").or_else(|| text(it, "title")).or_else(|| text(it, "product_name")).unwrap_or_else(|| "Item".into()), ..Default::default() })
                .collect();
            a.order
        })
        .collect();
    out.sort_by(|a, b| b.placed_at.cmp(&a.placed_at).then(b.id.cmp(&a.id)));
    Ok(out)
}

/// Fill in the orders' items: what the event says, with the catalog's picture and link for items
/// it matches (only verified products).
pub async fn fill_items(db: &PgPool, account: &Account, orders: &mut [Order]) -> AppResult<()> {
    let props: Vec<Value> = orders.iter().map(|o| o.props.clone()).collect();
    let hosts = learn::site_hosts(db, account.id).await?;
    let mut raw: Vec<(usize, Value, Value)> = Vec::new();
    for (i, p) in props.iter().enumerate() {
        for it in learn::event_items("Placed Order", p) {
            raw.push((i, it, p.clone()));
        }
    }
    let items: Vec<Value> = raw.iter().map(|r| r.1.clone()).collect();
    let matches = learn::resolve_many(db, account.id, &items).await?;
    let ids: Vec<Uuid> = matches.iter().filter_map(|m| m.map(|x| x.0)).collect();
    let products = recs::products_by_ids_all(db, account.id, &ids).await?;
    for o in orders.iter_mut() {
        o.items.clear();
    }
    for ((i, it, p), m) in raw.iter().zip(matches) {
        let card = learn::raw_card(it, p, &hosts);
        let product = m.and_then(|(pid, _)| products.get(&pid)).filter(|p| p.verified);
        let qty = num(it, "quantity");
        orders[*i].items.push(Item {
            name: card["name"].as_str().map(String::from).or_else(|| product.map(|p| p.title.clone())).unwrap_or_else(|| "Item".into()),
            quantity: qty,
            price_text: card["price_text"].as_str().map(String::from).or_else(|| num(it, "total").map(|t| store::money(t, text(p, "currency").as_deref()))),
            image_url: web_url(card["image_url"].as_str().map(String::from)).or_else(|| web_url(product.and_then(|p| p.image_url.clone()))),
            url: web_url(card["url"].as_str().map(String::from)).or_else(|| web_url(product.filter(|p| p.published).and_then(|p| p.url.clone()))),
        });
    }
    Ok(())
}

/// One order of this shopper's, with its items, or None when it isn't theirs.
pub async fn order(db: &PgPool, shopper: &Shopper, id: &str) -> AppResult<Option<Order>> {
    let mut all = orders(db, shopper).await?;
    let Some(i) = all.iter().position(|o| o.id == id) else { return Ok(None) };
    let mut one = vec![all.swap_remove(i)];
    fill_items(db, &shopper.account, &mut one).await?;
    Ok(one.pop())
}

/// A field's key as a label: `check_in` → "Check in".
fn label_of(key: &str) -> String {
    let s = key.replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn field_text(account: &Account, t: &ObjectType, key: &str, v: &Value) -> Option<String> {
    let def = t.field(key)?;
    let s = match (def.kind, v) {
        (_, Value::Null) => return None,
        (Kind::Boolean, Value::Bool(b)) => if *b { "Yes" } else { "No" }.to_string(),
        (Kind::Date, Value::String(s)) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.format("%b %-d, %Y").to_string()).unwrap_or_else(|_| s.clone()),
        (Kind::Datetime, Value::String(s)) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&tz(account)).format("%b %-d, %Y, %-I:%M %p").to_string())
            .unwrap_or_else(|_| s.clone()),
        (Kind::Money, v) => v.as_f64().map(|n| store::money(n, def.currency.as_deref())).unwrap_or_else(|| v.to_string()),
        (Kind::List, Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from).or_else(|| (!x.is_null()).then(|| x.to_string()))).collect::<Vec<_>>().join(", "),
        (_, Value::String(s)) => s.clone(),
        (_, other) => other.to_string(),
    };
    Some(s.chars().take(300).collect())
}

/// The shopper's records of the object types the store chose, showing only the chosen fields that
/// can be shown as the type is now (never a sensitive field; for bookings, never access or address).
pub async fn records(db: &PgPool, shopper: &Shopper, s: &Settings) -> AppResult<Vec<Record>> {
    let mut out = Vec::new();
    for c in &s.object_types {
        let Some(t) = crate::features::objects::types::by_key(db, shopper.account.id, &c.type_key).await? else { continue };
        let rows: Vec<(Value,)> = sqlx::query_as(
            "SELECT o.fields FROM objects o WHERE o.account_id = $2 AND o.type_id = $3
               AND EXISTS (SELECT 1 FROM object_links l WHERE l.object_id = o.id AND l.profile_id = $1 AND l.account_id = $2)
             ORDER BY o.updated_at DESC LIMIT $4",
        )
        .bind(shopper.profile_id)
        .bind(shopper.account.id)
        .bind(t.id)
        .bind(MAX_OBJECTS)
        .fetch_all(db)
        .await?;
        for (fields,) in rows {
            out.push(record(&shopper.account, &t, c.kind, &c.fields, fields.as_object().cloned().unwrap_or_default()));
        }
    }
    Ok(out)
}

fn record(account: &Account, t: &ObjectType, kind: ObjectKind, chosen: &[String], fields: Map<String, Value>) -> Record {
    let shown = chosen
        .iter()
        .filter(|k| settings::showable(t, kind, k))
        .filter_map(|k| {
            let def = t.field(k)?;
            let v = field_text(account, t, k, fields.get(k.as_str())?)?;
            Some((def.label.clone().unwrap_or_else(|| label_of(k)), v))
        })
        .collect();
    let title = crate::features::objects::store::title_of(t, &fields).filter(|_| t.title_field.as_deref().is_none_or(|k| settings::showable(t, kind, k))).unwrap_or_else(|| t.name.clone());
    Record { type_name: t.name.clone(), kind, title, fields: shown }
}

/// Picks for this shopper (verified, in-stock products).
pub async fn picks(db: &PgPool, shopper: &Shopper) -> AppResult<Vec<Pick>> {
    Ok(recs::for_person(db, shopper.account.id, shopper.profile_id, PICKS)
        .await?
        .into_iter()
        .map(|p| Pick {
            price_text: p.price.map(|v| store::money(v, p.currency.as_deref())),
            url: web_url(p.url),
            image_url: web_url(p.image_url),
            title: p.title.chars().take(200).collect(),
        })
        .collect())
}

/// The shopper's waiting back-in-stock requests.
pub async fn stock_requests(db: &PgPool, shopper: &Shopper) -> AppResult<Vec<StockRequest>> {
    let rows: Vec<(Uuid, String, Option<String>, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT s.id, p.title, p.url, p.image_url, s.created_at FROM catalog_stock_subscriptions s JOIN catalog_products p ON p.id = s.product_id AND p.account_id = s.account_id
         WHERE s.profile_id = $1 AND s.account_id = $2 AND s.notified_at IS NULL AND s.confirmed_at IS NOT NULL
         ORDER BY s.created_at DESC LIMIT $3",
    )
    .bind(shopper.profile_id)
    .bind(shopper.account.id)
    .bind(MAX_STOCK)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, url, image, at)| StockRequest { id, title: title.chars().take(200).collect(), url: web_url(url), image_url: web_url(image), since: date(&shopper.account, at) })
        .collect())
}

/// Cancel one of the shopper's own waiting back-in-stock requests.
pub async fn cancel_stock_request(db: &PgPool, shopper: &Shopper, id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query("DELETE FROM catalog_stock_subscriptions WHERE id = $1 AND profile_id = $2 AND account_id = $3 AND notified_at IS NULL")
        .bind(id)
        .bind(shopper.profile_id)
        .bind(shopper.account.id)
        .execute(db)
        .await?
        .rows_affected()
        > 0)
}

pub async fn prefs(db: &PgPool, shopper: &Shopper) -> AppResult<Prefs> {
    let (email_consent, email_suppressed, phone, sms_consent): (String, Option<String>, Option<String>, String) =
        sqlx::query_as("SELECT email_consent, email_suppressed_reason, phone_number, sms_consent FROM profiles WHERE id = $1 AND account_id = $2")
            .bind(shopper.profile_id)
            .bind(shopper.account.id)
            .fetch_one(db)
            .await?;
    let email_status = if email_suppressed.is_some() {
        EmailStatus::Suppressed
    } else {
        match email_consent.as_str() {
            "subscribed" => EmailStatus::Subscribed,
            "unsubscribed" => EmailStatus::Unsubscribed,
            _ => EmailStatus::NotSubscribed,
        }
    };
    // Lists show while they're subscribed, as on the preference page.
    let lists = if email_status == EmailStatus::Subscribed { crate::preferences::lists_for(db, shopper.account.id, shopper.profile_id).await? } else { Vec::new() };
    let phone_hint = phone.map(|p| p.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect());
    Ok(Prefs { email: shopper.email.clone(), email_status, lists, phone_hint, sms_subscribed: sms_consent == "subscribed" })
}

/// A made-up shopper for the dashboard's preview: nobody's real data.
pub fn sample(account: &Account, s: &Settings, types: &[ObjectType]) -> View {
    let now = Utc::now();
    let money = |v: f64| Some(store::money(v, Some("USD")));
    let orders = vec![
        Order {
            id: "1002".into(),
            number: "#1002".into(),
            placed_at: now - chrono::Duration::days(3),
            placed: date(account, now - chrono::Duration::days(3)),
            status: Status::Fulfilled,
            partial: false,
            total_text: money(86.0),
            refunded_text: None,
            items: vec![Item { name: "Linen shirt".into(), quantity: Some(1.0), price_text: money(68.0), image_url: None, url: None }, Item { name: "Canvas tote".into(), quantity: Some(1.0), price_text: money(18.0), image_url: None, url: None }],
            item_count: 2,
            shipments: vec![Shipment { carrier: Some("UPS".into()), tracking_number: Some("1Z999AA10123456784".into()), tracking_url: None, status: Some("In transit"), expected: Some(date(account, now + chrono::Duration::days(2))) }],
            props: json!({}),
        },
        Order {
            id: "1001".into(),
            number: "#1001".into(),
            placed_at: now - chrono::Duration::days(40),
            placed: date(account, now - chrono::Duration::days(40)),
            status: Status::Delivered,
            partial: false,
            total_text: money(42.5),
            refunded_text: None,
            items: vec![Item { name: "Wool socks".into(), quantity: Some(2.0), price_text: money(21.25), image_url: None, url: None }],
            item_count: 1,
            shipments: Vec::new(),
            props: json!({}),
        },
    ];
    let records = s
        .object_types
        .iter()
        .filter_map(|c| {
            let t = types.iter().find(|t| t.key == c.type_key)?;
            let fields = c
                .fields
                .iter()
                .filter(|k| settings::showable(t, c.kind, k))
                .filter_map(|k| {
                    let def = t.field(k)?;
                    let v = match def.kind {
                        Kind::Date => (now + chrono::Duration::days(14)).format("%b %-d, %Y").to_string(),
                        Kind::Datetime => date(account, now + chrono::Duration::days(14)),
                        Kind::Number => "2".into(),
                        Kind::Boolean => "Yes".into(),
                        Kind::Money => store::money(120.0, def.currency.as_deref()),
                        Kind::Enum => def.options.first().cloned().unwrap_or_else(|| "Sample".into()),
                        _ => "Sample".into(),
                    };
                    Some((def.label.clone().unwrap_or_else(|| label_of(k)), v))
                })
                .collect();
            Some(Record { type_name: t.name.clone(), kind: c.kind, title: format!("Sample {}", t.name.to_lowercase()), fields })
        })
        .collect();
    View {
        first_name: Some("Sam".into()),
        email: "sam@example.com".into(),
        orders: s.sections.orders.then_some(orders),
        more_orders: false,
        records: (s.sections.bookings && !s.object_types.is_empty()).then_some(records),
        picks: s.sections.recommendations.then(|| vec![Pick { title: "Merino beanie".into(), url: None, image_url: None, price_text: money(32.0) }, Pick { title: "Field jacket".into(), url: None, image_url: None, price_text: money(148.0) }]),
        stock: s.sections.recommendations.then(Vec::new),
        prefs: s.sections.preferences.then(|| Prefs {
            email: "sam@example.com".into(),
            email_status: EmailStatus::Subscribed,
            lists: vec![crate::preferences::PrefList { id: Uuid::nil(), name: "Newsletter".into(), description: String::new(), member: true }],
            phone_hint: None,
            sms_subscribed: false,
        }),
    }
}

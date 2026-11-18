//! Shopify customers as profiles, and their marketing consent.
//!
//! Consent comes only from Shopify's own consent fields, and it belongs to the exact address or
//! number Shopify reports it for:
//! - email `SUBSCRIBED` subscribes that address (from never subscribed or pending; never over an
//!   unsubscribe or a suppression, which always win);
//! - `UNSUBSCRIBED` unsubscribes it;
//! - `INVALID` and `REDACTED` never leave it subscribed (a subscribed or pending address becomes
//!   unsubscribed);
//! - `NOT_SUBSCRIBED` and `PENDING` change nothing; new profiles start as never subscribed and
//!   Shopify's state is kept in the `shopify_email_marketing` property.
//!
//! A report is applied only when its consent time (`marketingUpdatedAt`, `updatedAt` for SMS) is
//! newer than the last one applied for that address or number (`shopify_consent`), so a late
//! webhook or an old bulk snapshot never undoes a newer choice. A customer snapshot older than the
//! one already saved (Shopify's `updatedAt`) changes nothing at all.
//!
//! SMS works the same from the SMS consent of that phone number. Orders and checkouts never change
//! consent. When a customer's address or number changes in Shopify, the new one is a different
//! profile (or gets its own consent), and the old one keeps its own: nothing carries over.
//!
//! Every profile a customer touches is recorded (`shopify_customer_profiles`), so privacy requests
//! act on Shopify's ids rather than on whoever shares an address.
//!
//! Unsubscribes made in DumbMail are written back to Shopify ([`write_back`]).

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

use super::client::{self, ApiError};
use super::shop::ShopRow;
use crate::models::{normalize_email, normalize_phone};
use crate::state::AppState;

/// The numeric id from a gid (`gid://shopify/Customer/123` → 123) or a plain number.
pub fn num_id(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.rsplit('/').next()?.split('?').next()?.parse().ok(),
        _ => None,
    }
}

pub fn gid(kind: &str, id: i64) -> String {
    format!("gid://shopify/{kind}/{id}")
}

/// An exact amount: a decimal string or number as Shopify sends it, or a MoneyV2 / money set
/// (`amount`). No rounding.
pub fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s.trim()).ok(),
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        Value::Object(o) => o.get("amount").and_then(dec),
        _ => None,
    }
}

/// An amount as a float (prices in the catalog).
pub fn amount(v: &Value) -> Option<f64> {
    dec(v).and_then(|d| d.to_f64()).filter(|f| f.is_finite())
}

pub fn to_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// A customer as the GraphQL Admin API returns it (bulk and `nodes` share this shape).
#[derive(Debug, Clone, Default)]
pub struct Customer {
    pub id: i64,
    pub updated_at: Option<DateTime<Utc>>,
    pub email: Option<String>,
    pub email_state: Option<String>,
    pub email_opt_in_level: Option<String>,
    pub email_updated_at: Option<String>,
    pub phone: Option<String>,
    pub sms_state: Option<String>,
    pub sms_opt_in_level: Option<String>,
    pub sms_updated_at: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub tags: Vec<String>,
    pub orders: Option<i64>,
    pub spent: Option<f64>,
    pub currency: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country: Option<String>,
    pub zip: Option<String>,
    pub created_at: Option<String>,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|x| !x.is_empty()).map(|x| x.chars().take(500).collect())
}

pub const FIELDS: &str = "id updatedAt firstName lastName tags numberOfOrders amountSpent { amount currencyCode } createdAt
    defaultEmailAddress { emailAddress marketingState marketingOptInLevel marketingUpdatedAt }
    defaultPhoneNumber { phoneNumber smsMarketingConsent { state optInLevel updatedAt } }
    defaultAddress { city province countryCodeV2 zip }";

/// Profile properties this app sets. A customer redaction removes them.
pub const PROPERTY_KEYS: &[&str] = &[
    "shopify_customer_id", "shopify_tags", "shopify_order_count", "shopify_total_spent", "shopify_currency", "city", "region", "country", "zip",
    "shopify_email_marketing", "shopify_email_opt_in_level", "shopify_email_consent_at", "shopify_sms_marketing", "shopify_sms_consent_at",
];

impl Customer {
    pub fn from_graphql(v: &Value) -> Option<Customer> {
        let id = num_id(&v["id"])?;
        let e = &v["defaultEmailAddress"];
        let p = &v["defaultPhoneNumber"];
        let a = &v["defaultAddress"];
        Some(Customer {
            id,
            updated_at: parse_time(v["updatedAt"].as_str()),
            email: s(&e["emailAddress"]),
            email_state: s(&e["marketingState"]).map(|x| x.to_ascii_uppercase()),
            email_opt_in_level: s(&e["marketingOptInLevel"]),
            email_updated_at: s(&e["marketingUpdatedAt"]),
            phone: s(&p["phoneNumber"]),
            sms_state: s(&p["smsMarketingConsent"]["state"]).map(|x| x.to_ascii_uppercase()),
            sms_opt_in_level: s(&p["smsMarketingConsent"]["optInLevel"]),
            sms_updated_at: s(&p["smsMarketingConsent"]["updatedAt"]),
            first_name: s(&v["firstName"]),
            last_name: s(&v["lastName"]),
            tags: v["tags"].as_array().map(|t| t.iter().filter_map(s).take(100).collect()).unwrap_or_default(),
            orders: v["numberOfOrders"].as_str().and_then(|n| n.parse().ok()).or_else(|| v["numberOfOrders"].as_i64()),
            spent: amount(&v["amountSpent"]),
            currency: s(&v["amountSpent"]["currencyCode"]),
            city: s(&a["city"]),
            region: s(&a["province"]),
            country: s(&a["countryCodeV2"]),
            zip: s(&a["zip"]),
            created_at: s(&v["createdAt"]),
        })
    }
}

#[derive(sqlx::FromRow, Debug, Clone)]
struct P {
    id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
}

async fn by_email(db: &PgPool, account_id: Uuid, email: &str) -> sqlx::Result<Option<P>> {
    sqlx::query_as("SELECT id, email, phone_number FROM profiles WHERE account_id = $1 AND email = $2").bind(account_id).bind(email).fetch_optional(db).await
}

async fn by_phone(db: &PgPool, account_id: Uuid, phone: &str) -> sqlx::Result<Option<P>> {
    sqlx::query_as("SELECT id, email, phone_number FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(phone).fetch_optional(db).await
}

async fn by_id(db: &PgPool, account_id: Uuid, id: Uuid) -> sqlx::Result<Option<P>> {
    sqlx::query_as("SELECT id, email, phone_number FROM profiles WHERE account_id = $1 AND id = $2").bind(account_id).bind(id).fetch_optional(db).await
}

/// Remember that this store's customer (0 for a guest order or checkout) touched this profile.
pub async fn note_profile(db: &PgPool, account_id: Uuid, shop: &str, customer_id: Option<i64>, profile_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO shopify_customer_profiles (account_id, shop_domain, customer_id, profile_id, generation) VALUES ($1, $2, $3, $4, (SELECT generation FROM shopify_shops WHERE account_id = $1))
         ON CONFLICT (account_id, shop_domain, customer_id, profile_id) DO UPDATE SET generation = EXCLUDED.generation")
        .bind(account_id)
        .bind(shop)
        .bind(customer_id.unwrap_or(0))
        .bind(profile_id)
        .execute(db)
        .await?;
    Ok(())
}

/// A new profile with one identifier and no consent (never subscribed), or the one that already has
/// it. The bool says whether this app made it (recorded for privacy requests).
async fn create(db: &PgPool, account_id: Uuid, shop: &str, email: Option<&str>, phone: Option<&str>, first: Option<&str>, last: Option<&str>) -> sqlx::Result<Option<P>> {
    for _ in 0..2 {
        let found = match (email, phone) {
            (Some(e), _) => by_email(db, account_id, e).await?,
            (None, Some(p)) => by_phone(db, account_id, p).await?,
            _ => return Ok(None),
        };
        if found.is_some() {
            return Ok(found);
        }
        // Made here from the store's data, nobody has opted in yet: held back from marketing in the
        // same statement (`marketing_hold`; Shopify's own subscribed state, applied next, clears it),
        // and the address is marked as the store's, so only flows marked as service messages (order
        // and shipping updates) reach it while it's held.
        let tag = source_tag(shop);
        let phone = if email.is_some() { None } else { phone };
        let r: Result<P, sqlx::Error> = sqlx::query_as(
            "INSERT INTO profiles (id, account_id, email, phone_number, first_name, last_name, source, marketing_hold, email_source, phone_source)
             VALUES ($1, $2, $3, $4, $5, $6, $7, true, CASE WHEN $3::text IS NULL THEN NULL ELSE $7 END, CASE WHEN $4::text IS NULL THEN NULL ELSE $7 END)
             RETURNING id, email, phone_number",
        )
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(email)
        .bind(phone)
        .bind(first)
        .bind(last)
        .bind(&tag)
        .fetch_one(db)
        .await;
        match r {
            Ok(p) => {
                sqlx::query("INSERT INTO shopify_created_profiles (account_id, shop_domain, profile_id, generation) VALUES ($1, $2, $3, (SELECT generation FROM shopify_shops WHERE account_id = $1)) ON CONFLICT DO NOTHING")
                    .bind(account_id)
                    .bind(shop)
                    .bind(p.id)
                    .execute(db)
                    .await?;
                return Ok(Some(p));
            }
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// The marker for addresses and profiles this app brought in (`email_source`, `phone_source`,
/// `source`): the same `integration:` form main's marketing-hold rule recognises.
pub fn source_tag(shop: &str) -> String {
    format!("integration:shopify:{shop}")
}

/// Give a profile that has no email this address. Its email consent starts over (never subscribed):
/// consent never comes from somewhere else. False if another profile has the address or this one got one.
async fn attach_email(db: &PgPool, id: Uuid, email: &str) -> sqlx::Result<bool> {
    let r = sqlx::query("UPDATE profiles SET email = $2, email_consent = 'never_subscribed', updated_at = now() WHERE id = $1 AND email IS NULL").bind(id).bind(email).execute(db).await;
    match r {
        Ok(r) => Ok(r.rows_affected() == 1),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => Ok(false),
        Err(e) => Err(e),
    }
}

async fn attach_phone(db: &PgPool, id: Uuid, phone: &str) -> sqlx::Result<bool> {
    let r = sqlx::query("UPDATE profiles SET phone_number = $2, sms_consent = 'never_subscribed', updated_at = now() WHERE id = $1 AND phone_number IS NULL").bind(id).bind(phone).execute(db).await;
    match r {
        Ok(r) => Ok(r.rows_affected() == 1),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => Ok(false),
        Err(e) => Err(e),
    }
}

/// Claim the right to apply Shopify's consent report for this address or number: only when it's
/// newer than the last one applied (a report without a time only when nothing was applied yet).
/// One conditional statement, so two workers can't both apply out of order.
async fn newer_consent(db: &mut sqlx::PgConnection, account_id: Uuid, channel: &str, contact: &str, state: Option<&str>, at: Option<DateTime<Utc>>) -> sqlx::Result<bool> {
    let won: Option<String> = sqlx::query_scalar(
        "INSERT INTO shopify_consent (account_id, channel, contact, state, consent_at) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (account_id, channel, contact) DO UPDATE SET state = EXCLUDED.state, consent_at = EXCLUDED.consent_at, applied_at = now()
         WHERE EXCLUDED.consent_at IS NOT NULL AND (shopify_consent.consent_at IS NULL OR EXCLUDED.consent_at > shopify_consent.consent_at)
         RETURNING contact",
    )
    .bind(account_id)
    .bind(channel)
    .bind(contact)
    .bind(state)
    .bind(at)
    .fetch_optional(db)
    .await?;
    Ok(won.is_some())
}

/// Apply Shopify's email state to the profile that has exactly this address. Conditional on the
/// current row, so a DumbMail unsubscribe or a suppression written a moment ago still wins.
async fn apply_email_state(db: &mut sqlx::PgConnection, id: Uuid, email: &str, state: Option<&str>) -> sqlx::Result<()> {
    let sql = match state {
        Some("SUBSCRIBED") => {
            "UPDATE profiles SET email_consent = 'subscribed', updated_at = now()
             WHERE id = $1 AND email = $2 AND email_consent IN ('never_subscribed', 'pending') AND email_suppressed_reason IS NULL"
        }
        Some("UNSUBSCRIBED") => "UPDATE profiles SET email_consent = 'unsubscribed', updated_at = now() WHERE id = $1 AND email = $2 AND email_consent <> 'unsubscribed'",
        // Shopify can't email this address (invalid) or erased it (redacted): never leave it subscribed.
        Some("INVALID" | "REDACTED") => "UPDATE profiles SET email_consent = 'unsubscribed', updated_at = now() WHERE id = $1 AND email = $2 AND email_consent IN ('subscribed', 'pending')",
        _ => return Ok(()),
    };
    sqlx::query(sql).bind(id).bind(email).execute(db).await?;
    Ok(())
}

async fn apply_sms_state(db: &mut sqlx::PgConnection, id: Uuid, phone: &str, state: Option<&str>) -> sqlx::Result<()> {
    let sql = match state {
        Some("SUBSCRIBED") => "UPDATE profiles SET sms_consent = 'subscribed', updated_at = now() WHERE id = $1 AND phone_number = $2 AND sms_consent IN ('never_subscribed', 'pending')",
        Some("UNSUBSCRIBED") => "UPDATE profiles SET sms_consent = 'unsubscribed', updated_at = now() WHERE id = $1 AND phone_number = $2 AND sms_consent <> 'unsubscribed'",
        Some("INVALID" | "REDACTED") => "UPDATE profiles SET sms_consent = 'unsubscribed', updated_at = now() WHERE id = $1 AND phone_number = $2 AND sms_consent IN ('subscribed', 'pending')",
        _ => return Ok(()),
    };
    sqlx::query(sql).bind(id).bind(phone).execute(db).await?;
    Ok(())
}

/// Save a Shopify customer: find or make the right profile, apply consent for its exact address and
/// number, fill in details, and remember the link. Returns the profile it's linked to. A customer
/// erased by a privacy request, or a snapshot older than the one already saved, changes nothing.
pub async fn apply(db: &PgPool, account_id: Uuid, shop: &str, generation: i32, c: &Customer) -> anyhow::Result<Option<Uuid>> {
    // Written inside the fence: not into a newer install, and not after a privacy request erased it.
    let Some(fence) = super::fence::open(db, account_id, shop, generation, Some(c.id), None).await? else { return Ok(None) };
    let r = apply_inner(db, account_id, shop, c).await?;
    fence.done().await?;
    Ok(r)
}

async fn apply_inner(db: &PgPool, account_id: Uuid, shop: &str, c: &Customer) -> anyhow::Result<Option<Uuid>> {
    let email = c.email.as_deref().and_then(normalize_email);
    let phone = c.phone.as_deref().and_then(normalize_phone);
    let saved: Option<(Option<Uuid>, Option<DateTime<Utc>>)> = sqlx::query_as("SELECT profile_id, shopify_updated_at FROM shopify_customers WHERE account_id = $1 AND customer_id = $2")
        .bind(account_id)
        .bind(c.id)
        .fetch_optional(db)
        .await?;
    if let (Some((linked, Some(saved_at))), Some(at)) = (&saved, c.updated_at) {
        if at < *saved_at {
            return Ok(*linked);
        }
    }
    let linked = match saved.and_then(|s| s.0) {
        Some(id) => by_id(db, account_id, id).await?,
        None => None,
    };
    let (first, last) = (c.first_name.as_deref(), c.last_name.as_deref());

    // The profile for the email address: the one that has it, else the linked profile if it has no
    // email yet, else a new one. The linked profile with a different address is left as it is.
    let main: Option<P> = match &email {
        Some(e) => match by_email(db, account_id, e).await? {
            Some(p) => Some(p),
            None => match &linked {
                Some(l) if l.email.is_none() && attach_email(db, l.id, e).await? => Some(P { email: Some(e.clone()), ..l.clone() }),
                _ => create(db, account_id, shop, Some(e), None, first, last).await?,
            },
        },
        None => match &phone {
            Some(ph) => match by_phone(db, account_id, ph).await? {
                Some(p) => Some(p),
                None => match &linked {
                    Some(l) if l.phone_number.is_none() && attach_phone(db, l.id, ph).await? => Some(P { phone_number: Some(ph.clone()), ..l.clone() }),
                    _ => create(db, account_id, shop, None, Some(ph), first, last).await?,
                },
            },
            None => linked.clone(),
        },
    };
    let Some(main) = main else { return Ok(None) };
    note_profile(db, account_id, shop, Some(c.id), main.id).await?;
    if let Some(e) = &email {
        // The consent time and the profile's consent commit together: a crash between them can't
        // leave the time recorded and the change (an unsubscribe, say) unapplied.
        let mut tx = db.begin().await?;
        if newer_consent(&mut tx, account_id, "email", e, c.email_state.as_deref(), parse_time(c.email_updated_at.as_deref())).await? {
            apply_email_state(&mut tx, main.id, e, c.email_state.as_deref()).await?;
        }
        tx.commit().await?;
    }
    // SMS consent goes to whoever has that number: the profile that has it, else this profile if it
    // has none, else a profile of its own (a changed number is never left unlinked).
    if let Some(ph) = &phone {
        let sms_owner = match by_phone(db, account_id, ph).await? {
            Some(p) => Some(p.id),
            None if main.phone_number.is_none() && attach_phone(db, main.id, ph).await? => Some(main.id),
            None => create(db, account_id, shop, None, Some(ph), first, last).await?.map(|p| p.id),
        };
        if let Some(owner) = sms_owner {
            note_profile(db, account_id, shop, Some(c.id), owner).await?;
            let mut tx = db.begin().await?;
            if newer_consent(&mut tx, account_id, "sms", ph, c.sms_state.as_deref(), parse_time(c.sms_updated_at.as_deref())).await? {
                apply_sms_state(&mut tx, owner, ph, c.sms_state.as_deref()).await?;
            }
            tx.commit().await?;
        }
    }
    let mut props = Map::new();
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            props.insert(k.to_string(), v);
        }
    };
    put("shopify_customer_id", json!(c.id.to_string()));
    put("shopify_tags", json!(c.tags));
    put("shopify_order_count", json!(c.orders));
    put("shopify_total_spent", json!(c.spent));
    put("shopify_currency", json!(c.currency));
    put("city", json!(c.city));
    put("region", json!(c.region));
    put("country", json!(c.country));
    put("zip", json!(c.zip));
    if email.is_some() {
        put("shopify_email_marketing", json!(c.email_state.as_deref().map(str::to_ascii_lowercase)));
        put("shopify_email_opt_in_level", json!(c.email_opt_in_level.as_deref().map(str::to_ascii_lowercase)));
        put("shopify_email_consent_at", json!(c.email_updated_at));
    }
    if phone.is_some() {
        put("shopify_sms_marketing", json!(c.sms_state.as_deref().map(str::to_ascii_lowercase)));
        put("shopify_sms_consent_at", json!(c.sms_updated_at));
    }
    sqlx::query(
        "UPDATE profiles SET properties = properties || $2, first_name = COALESCE($3, first_name), last_name = COALESCE($4, last_name), updated_at = now()
         WHERE id = $1",
    )
    .bind(main.id)
    .bind(Value::Object(props))
    .bind(first)
    .bind(last)
    .execute(db)
    .await?;
    // The link, unless a newer snapshot got there first (checked in the same statement).
    sqlx::query(
        "INSERT INTO shopify_customers (account_id, customer_id, profile_id, email, phone, email_state, sms_state, shop_domain, shopify_updated_at, updated_at, generation)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), (SELECT generation FROM shopify_shops WHERE account_id = $1))
         ON CONFLICT (account_id, customer_id) DO UPDATE SET generation = EXCLUDED.generation,
            -- An order moved the link after this snapshot was taken: the link (and its contact) stay.
            profile_id = CASE WHEN (shopify_customers.linked_at IS NULL OR (EXCLUDED.shopify_updated_at IS NOT NULL AND EXCLUDED.shopify_updated_at > shopify_customers.linked_at)) THEN EXCLUDED.profile_id ELSE shopify_customers.profile_id END,
            email = CASE WHEN (shopify_customers.linked_at IS NULL OR (EXCLUDED.shopify_updated_at IS NOT NULL AND EXCLUDED.shopify_updated_at > shopify_customers.linked_at)) THEN EXCLUDED.email ELSE shopify_customers.email END,
            phone = CASE WHEN (shopify_customers.linked_at IS NULL OR (EXCLUDED.shopify_updated_at IS NOT NULL AND EXCLUDED.shopify_updated_at > shopify_customers.linked_at)) THEN EXCLUDED.phone ELSE shopify_customers.phone END,
            email_state = CASE WHEN (shopify_customers.linked_at IS NULL OR (EXCLUDED.shopify_updated_at IS NOT NULL AND EXCLUDED.shopify_updated_at > shopify_customers.linked_at)) THEN EXCLUDED.email_state ELSE shopify_customers.email_state END,
            sms_state = CASE WHEN (shopify_customers.linked_at IS NULL OR (EXCLUDED.shopify_updated_at IS NOT NULL AND EXCLUDED.shopify_updated_at > shopify_customers.linked_at)) THEN EXCLUDED.sms_state ELSE shopify_customers.sms_state END,
            shop_domain = EXCLUDED.shop_domain,
            shopify_updated_at = COALESCE(EXCLUDED.shopify_updated_at, shopify_customers.shopify_updated_at), deleted_at = NULL,
            email_wb_attempts = CASE WHEN shopify_customers.email IS DISTINCT FROM EXCLUDED.email THEN 0 ELSE shopify_customers.email_wb_attempts END,
            sms_wb_attempts = CASE WHEN shopify_customers.phone IS DISTINCT FROM EXCLUDED.phone THEN 0 ELSE shopify_customers.sms_wb_attempts END,
            updated_at = now()
         WHERE EXCLUDED.shopify_updated_at IS NULL OR shopify_customers.shopify_updated_at IS NULL OR EXCLUDED.shopify_updated_at >= shopify_customers.shopify_updated_at",
    )
    .bind(account_id)
    .bind(c.id)
    .bind(main.id)
    .bind(&email)
    .bind(&phone)
    .bind(&c.email_state)
    .bind(&c.sms_state)
    .bind(shop)
    .bind(c.updated_at)
    .execute(db)
    .await?;
    Ok(Some(main.id))
}

/// Who an order or checkout is for.
pub struct Buyer {
    pub profile_id: Uuid,
    /// The event carried no email or phone: it's recorded for the linked customer's profile, but
    /// nothing should be sent because of it (flows don't start from it).
    pub no_contact: bool,
}

/// The profile an order or checkout belongs to: always the one whose email (else phone) is the
/// event's own contact, found or made. If the Shopify customer was linked to a different profile,
/// the link moves to this one, so nothing about this order goes to an address the buyer no longer
/// uses. With no contact at all, the linked customer's profile, marked `no_contact`. Never changes
/// consent or an existing profile's identity. A customer seen for the first time is queued to be read
/// in full (that's where their consent comes from).
#[allow(clippy::too_many_arguments)]
pub async fn profile_for(db: &PgPool, account_id: Uuid, shop: &str, customer_id: Option<i64>, email: Option<&str>, phone: Option<&str>, first: Option<&str>, last: Option<&str>, at: Option<DateTime<Utc>>) -> anyhow::Result<Option<Buyer>> {
    let email = email.and_then(normalize_email);
    let phone = phone.and_then(normalize_phone);
    let found = match (&email, &phone) {
        (Some(e), _) => create(db, account_id, shop, Some(e), None, first, last).await?,
        (None, Some(ph)) => create(db, account_id, shop, None, Some(ph), first, last).await?,
        (None, None) => {
            let Some(cid) = customer_id else { return Ok(None) };
            let linked: Option<Option<Uuid>> = sqlx::query_scalar("SELECT profile_id FROM shopify_customers WHERE account_id = $1 AND customer_id = $2")
                .bind(account_id)
                .bind(cid)
                .fetch_optional(db)
                .await?;
            return Ok(match linked.flatten() {
                Some(id) if by_id(db, account_id, id).await?.is_some() => Some(Buyer { profile_id: id, no_contact: true }),
                _ => None,
            });
        }
    };
    let Some(p) = found else { return Ok(None) };
    note_profile(db, account_id, shop, customer_id, p.id).await?;
    if let Some(cid) = customer_id {
        // Moving the link also moves its contact to this event's, and records when (`linked_at`,
        // the event's own time): a customer snapshot no newer than the event (a bulk import running
        // alongside) can't move it back. The snapshot time (`shopify_updated_at`) isn't touched, so
        // the customer's own later updates (and their consent) still apply.
        let new: Option<i64> = sqlx::query_scalar(
            "INSERT INTO shopify_customers (account_id, customer_id, profile_id, shop_domain, email, phone, linked_at, generation) VALUES ($1, $2, $3, $4, $5, $6, $7, (SELECT generation FROM shopify_shops WHERE account_id = $1))
             ON CONFLICT (account_id, customer_id) DO UPDATE SET profile_id = EXCLUDED.profile_id, generation = EXCLUDED.generation,
                email = COALESCE(EXCLUDED.email, shopify_customers.email), phone = COALESCE(EXCLUDED.phone, shopify_customers.phone),
                linked_at = GREATEST(shopify_customers.linked_at, EXCLUDED.linked_at), updated_at = now()
                WHERE shopify_customers.profile_id IS DISTINCT FROM EXCLUDED.profile_id
             RETURNING (xmax = 0)::int::bigint",
        )
        .bind(account_id)
        .bind(cid)
        .bind(p.id)
        .bind(shop)
        .bind(&email)
        .bind(&phone)
        .bind(at)
        .fetch_optional(db)
        .await?;
        if new == Some(1) {
            super::products::queue_refresh(db, account_id, "customer", cid).await?;
        }
    }
    Ok(Some(Buyer { profile_id: p.id, no_contact: false }))
}

/// Read customers again by id (after a webhook said they changed), up to 50 at a time.
pub async fn refresh(state: &AppState, shop: &ShopRow, ids: &[i64]) -> Result<(), ApiError> {
    let gids: Vec<String> = ids.iter().map(|i| gid("Customer", *i)).collect();
    let q = format!("query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ... on Customer {{ {FIELDS} }} }} }}");
    let data = client::graphql(state, shop, &q, json!({"ids": gids})).await?;
    // Ids Shopify doesn't return (null) were deleted: nothing to read.
    for n in data["nodes"].as_array().into_iter().flatten() {
        if let Some(c) = Customer::from_graphql(n) {
            apply(&state.db, shop.account_id, &shop.shop_domain, shop.generation, &c).await.map_err(|_| ApiError::Network("database"))?;
        }
    }
    Ok(())
}

/// Shopify deleted the customer: stop writing back to it. The link stays (marked deleted) so a later
/// privacy request still finds what came from this customer; the profile and its history stay.
pub async fn unlink(db: &PgPool, account_id: Uuid, customer_id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE shopify_customers SET deleted_at = now() WHERE account_id = $1 AND customer_id = $2").bind(account_id).bind(customer_id).execute(db).await?;
    Ok(())
}

/// Write-back candidates for one store, matched by the address and the number themselves (a
/// customer's SMS number often belongs to a different profile than their email): Shopify still has
/// them subscribed or pending and the profile with that address or number is unsubscribed here.
const DUE: &str = "
    (c.email IS NOT NULL AND c.email_state IN ('SUBSCRIBED', 'PENDING') AND (c.email_wb_after IS NULL OR c.email_wb_after <= now())
        AND EXISTS (SELECT 1 FROM profiles p WHERE p.account_id = c.account_id AND p.email = c.email AND p.email_consent = 'unsubscribed'))
    OR (c.phone IS NOT NULL AND c.sms_state IN ('SUBSCRIBED', 'PENDING') AND (c.sms_wb_after IS NULL OR c.sms_wb_after <= now())
        AND EXISTS (SELECT 1 FROM profiles p WHERE p.account_id = c.account_id AND p.phone_number = c.phone AND p.sms_consent = 'unsubscribed'))";

/// Unsubscribes made in DumbMail, sent to Shopify. A write-back is done only when Shopify confirms it
/// (no `userErrors`); otherwise it stays queued and is tried again later (1 minute, doubling, up to a
/// day), with the problem shown in status. Up to `max` per call, until `deadline`. Returns how many
/// were confirmed.
pub async fn write_back(state: &AppState, shop: &ShopRow, max: i64, deadline: std::time::Instant) -> Result<usize, ApiError> {
    let db_err = |_| ApiError::Network("database");
    let rows: Vec<(i64, bool, bool)> = sqlx::query_as(&format!(
        "SELECT c.customer_id,
                (c.email IS NOT NULL AND c.email_state IN ('SUBSCRIBED', 'PENDING') AND (c.email_wb_after IS NULL OR c.email_wb_after <= now())
                    AND EXISTS (SELECT 1 FROM profiles p WHERE p.account_id = c.account_id AND p.email = c.email AND p.email_consent = 'unsubscribed')),
                (c.phone IS NOT NULL AND c.sms_state IN ('SUBSCRIBED', 'PENDING') AND (c.sms_wb_after IS NULL OR c.sms_wb_after <= now())
                    AND EXISTS (SELECT 1 FROM profiles p WHERE p.account_id = c.account_id AND p.phone_number = c.phone AND p.sms_consent = 'unsubscribed'))
         FROM shopify_customers c
         WHERE c.account_id = $1 AND c.shop_domain = $2 AND c.deleted_at IS NULL AND ({DUE})
         ORDER BY c.customer_id LIMIT $3"
    ))
    .bind(shop.account_id)
    .bind(&shop.shop_domain)
    .bind(max)
    .fetch_all(&state.db)
    .await
    .map_err(db_err)?;
    let mut n = 0;
    for (cid, email_out, sms_out) in rows {
        for (on, channel) in [(email_out, "email"), (sms_out, "sms")] {
            if !on || std::time::Instant::now() >= deadline {
                continue;
            }
            let now = Utc::now();
            let (mutation, key, field) = if channel == "email" {
                ("mutation($input: CustomerEmailMarketingConsentUpdateInput!) { customerEmailMarketingConsentUpdate(input: $input) { customer { id } userErrors { field message code } } }", "customerEmailMarketingConsentUpdate", "emailMarketingConsent")
            } else {
                ("mutation($input: CustomerSmsMarketingConsentUpdateInput!) { customerSmsMarketingConsentUpdate(input: $input) { customer { id } userErrors { field message code } } }", "customerSmsMarketingConsentUpdate", "smsMarketingConsent")
            };
            let vars = json!({"input": {"customerId": gid("Customer", cid), field: {"marketingState": "UNSUBSCRIBED", "marketingOptInLevel": "SINGLE_OPT_IN", "consentUpdatedAt": now.to_rfc3339()}}});
            let problem = match client::graphql(state, shop, mutation, vars).await {
                Ok(d) => client::user_error(&d[key]).or_else(|| d[key]["customer"]["id"].is_null().then(|| "no customer returned".to_string())),
                Err(e) if e.retry_after().is_some() && matches!(e, ApiError::Throttled(_) | ApiError::Busy) => return Err(e),
                Err(e) => Some(e.to_string()),
            };
            let (state_col, a, after, err) = if channel == "email" { ("email_state", "email_wb_attempts", "email_wb_after", "email_wb_error") } else { ("sms_state", "sms_wb_attempts", "sms_wb_after", "sms_wb_error") };
            match problem {
                None => {
                    sqlx::query(&format!("UPDATE shopify_customers SET {state_col} = 'UNSUBSCRIBED', {a} = 0, {after} = NULL, {err} = NULL, updated_at = now() WHERE account_id = $1 AND customer_id = $2"))
                        .bind(shop.account_id)
                        .bind(cid)
                        .execute(&state.db)
                        .await
                        .map_err(db_err)?;
                    // What we sent is now the newest consent Shopify has for that contact.
                    let contact: Option<String> = sqlx::query_scalar(&format!("SELECT {} FROM shopify_customers WHERE account_id = $1 AND customer_id = $2", if channel == "email" { "email" } else { "phone" }))
                        .bind(shop.account_id)
                        .bind(cid)
                        .fetch_optional(&state.db)
                        .await
                        .map_err(db_err)?
                        .flatten();
                    if let Some(contact) = contact {
                        let mut c = state.db.acquire().await.map_err(db_err)?;
                        newer_consent(&mut c, shop.account_id, channel, &contact, Some("UNSUBSCRIBED"), Some(now)).await.map_err(db_err)?;
                    }
                    n += 1;
                }
                Some(code) => {
                    tracing::warn!(account = %shop.account_id, channel, problem = %code, "Shopify didn't accept an unsubscribe write-back; trying again later");
                    sqlx::query(&format!(
                        "UPDATE shopify_customers SET {a} = {a} + 1, {after} = now() + LEAST(interval '1 day', interval '1 minute' * power(2, {a})), {err} = $3, updated_at = now()
                         WHERE account_id = $1 AND customer_id = $2"
                    ))
                    .bind(shop.account_id)
                    .bind(cid)
                    .bind(code.chars().take(100).collect::<String>())
                    .execute(&state.db)
                    .await
                    .map_err(db_err)?;
                }
            }
        }
    }
    Ok(n)
}

/// Accounts with an unsubscribe to write back now. (Cheap; the scheduler asks.)
pub async fn writeback_due(db: &PgPool) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(&format!(
        "SELECT DISTINCT c.account_id FROM shopify_customers c
         JOIN shopify_shops s ON s.account_id = c.account_id AND s.status = 'connected' AND s.shop_domain = c.shop_domain
         WHERE c.deleted_at IS NULL AND ({DUE})
         LIMIT 200"
    ))
    .fetch_all(db)
    .await
}

/// Write-backs waiting, and those Shopify refused at least once (for status).
pub async fn writeback_counts(db: &PgPool, account_id: Uuid, shop: &str) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE TRUE), count(*) FILTER (WHERE c.email_wb_attempts > 0 OR c.sms_wb_attempts > 0)
         FROM shopify_customers c WHERE c.account_id = $1 AND c.shop_domain = $2 AND c.deleted_at IS NULL AND (
           (c.email IS NOT NULL AND c.email_state IN ('SUBSCRIBED', 'PENDING') AND EXISTS (SELECT 1 FROM profiles p WHERE p.account_id = c.account_id AND p.email = c.email AND p.email_consent = 'unsubscribed'))
           OR (c.phone IS NOT NULL AND c.sms_state IN ('SUBSCRIBED', 'PENDING') AND EXISTS (SELECT 1 FROM profiles p WHERE p.account_id = c.account_id AND p.phone_number = c.phone AND p.sms_consent = 'unsubscribed')))"
    )
    .bind(account_id)
    .bind(shop)
    .fetch_one(db)
    .await
}

pub fn parse_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|x| DateTime::parse_from_rfc3339(x).ok()).map(|d| d.with_timezone(&Utc))
}

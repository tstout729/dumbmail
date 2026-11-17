//! Shopify customers as profiles, and their marketing consent.
//!
//! Consent comes only from Shopify's own consent fields, and it belongs to the exact address or
//! number Shopify reports it for:
//! - email `SUBSCRIBED` subscribes that address (from never subscribed or pending; never over an
//!   unsubscribe or a suppression, which always win);
//! - `UNSUBSCRIBED` unsubscribes it;
//! - anything else (`NOT_SUBSCRIBED`, `PENDING`, `INVALID`, `REDACTED`) changes nothing; new profiles
//!   start as never subscribed and Shopify's state is kept in the `shopify_email_marketing` property.
//! SMS works the same from the SMS consent of that phone number. Orders and checkouts never change
//! consent. When a customer's address changes in Shopify, the new address is a different profile (or
//! gets its own consent), and the old one keeps its own: nothing carries over.
//!
//! Unsubscribes made in DumbMail are written back to Shopify ([`write_back`]).

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
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

fn money(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        Value::Object(o) => o.get("amount").and_then(money).or_else(|| o.get("shopMoney").and_then(money)),
        _ => None,
    }
}

pub fn amount(v: &Value) -> Option<f64> {
    money(v).filter(|f| f.is_finite())
}

/// A customer as the GraphQL Admin API returns it (bulk and `nodes` share this shape).
#[derive(Debug, Clone, Default)]
pub struct Customer {
    pub id: i64,
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

pub const FIELDS: &str = "id firstName lastName tags numberOfOrders amountSpent { amount currencyCode } createdAt
    defaultEmailAddress { emailAddress marketingState marketingOptInLevel marketingUpdatedAt }
    defaultPhoneNumber { phoneNumber smsMarketingConsent { state optInLevel updatedAt } }
    defaultAddress { city province countryCodeV2 zip }";

impl Customer {
    pub fn from_graphql(v: &Value) -> Option<Customer> {
        let id = num_id(&v["id"])?;
        let e = &v["defaultEmailAddress"];
        let p = &v["defaultPhoneNumber"];
        let a = &v["defaultAddress"];
        Some(Customer {
            id,
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

/// A new profile with one identifier and no consent (never subscribed), or the one that already has it.
async fn create(db: &PgPool, account_id: Uuid, email: Option<&str>, phone: Option<&str>, first: Option<&str>, last: Option<&str>) -> sqlx::Result<Option<P>> {
    for _ in 0..2 {
        let found = match (email, phone) {
            (Some(e), _) => by_email(db, account_id, e).await?,
            (None, Some(p)) => by_phone(db, account_id, p).await?,
            _ => return Ok(None),
        };
        if found.is_some() {
            return Ok(found);
        }
        let r: Result<P, sqlx::Error> = sqlx::query_as(
            "INSERT INTO profiles (id, account_id, email, phone_number, first_name, last_name) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, email, phone_number",
        )
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(email)
        .bind(if email.is_some() { None } else { phone })
        .bind(first)
        .bind(last)
        .fetch_one(db)
        .await;
        match r {
            Ok(p) => return Ok(Some(p)),
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
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

/// Apply Shopify's email state to the profile that has exactly this address. Conditional on the
/// current row, so a DumbMail unsubscribe or a suppression written a moment ago still wins.
async fn apply_email_state(db: &PgPool, id: Uuid, email: &str, state: Option<&str>) -> sqlx::Result<()> {
    match state {
        Some("SUBSCRIBED") => {
            sqlx::query(
                "UPDATE profiles SET email_consent = 'subscribed', updated_at = now()
                 WHERE id = $1 AND email = $2 AND email_consent IN ('never_subscribed', 'pending') AND email_suppressed_reason IS NULL",
            )
            .bind(id)
            .bind(email)
            .execute(db)
            .await?;
        }
        Some("UNSUBSCRIBED") => {
            sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed', updated_at = now() WHERE id = $1 AND email = $2 AND email_consent <> 'unsubscribed'")
                .bind(id)
                .bind(email)
                .execute(db)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn apply_sms_state(db: &PgPool, id: Uuid, phone: &str, state: Option<&str>) -> sqlx::Result<()> {
    match state {
        Some("SUBSCRIBED") => {
            sqlx::query("UPDATE profiles SET sms_consent = 'subscribed', updated_at = now() WHERE id = $1 AND phone_number = $2 AND sms_consent IN ('never_subscribed', 'pending')")
                .bind(id)
                .bind(phone)
                .execute(db)
                .await?;
        }
        Some("UNSUBSCRIBED") => {
            sqlx::query("UPDATE profiles SET sms_consent = 'unsubscribed', updated_at = now() WHERE id = $1 AND phone_number = $2 AND sms_consent <> 'unsubscribed'")
                .bind(id)
                .bind(phone)
                .execute(db)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

/// Save a Shopify customer: find or make the right profile, apply consent for its exact address and
/// number, fill in details, and remember the link. Returns the profile it's linked to.
pub async fn apply(db: &PgPool, account_id: Uuid, c: &Customer) -> anyhow::Result<Option<Uuid>> {
    let email = c.email.as_deref().and_then(normalize_email);
    let phone = c.phone.as_deref().and_then(normalize_phone);
    let linked: Option<Uuid> = sqlx::query_scalar("SELECT profile_id FROM shopify_customers WHERE account_id = $1 AND customer_id = $2")
        .bind(account_id)
        .bind(c.id)
        .fetch_optional(db)
        .await?
        .flatten();
    let linked = match linked {
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
                _ => create(db, account_id, Some(e), None, first, last).await?,
            },
        },
        None => match &phone {
            Some(ph) => match by_phone(db, account_id, ph).await? {
                Some(p) => Some(p),
                None => match &linked {
                    Some(l) if l.phone_number.is_none() && attach_phone(db, l.id, ph).await? => Some(P { phone_number: Some(ph.clone()), ..l.clone() }),
                    _ => create(db, account_id, None, Some(ph), first, last).await?,
                },
            },
            None => linked.clone(),
        },
    };
    let Some(main) = main else { return Ok(None) };
    if let Some(e) = &email {
        apply_email_state(db, main.id, e, c.email_state.as_deref()).await?;
    }
    // SMS consent goes to whoever has that number: this profile if it's theirs or it has none.
    if let Some(ph) = &phone {
        let sms_owner = match by_phone(db, account_id, ph).await? {
            Some(p) => Some(p.id),
            None if main.phone_number.is_none() && attach_phone(db, main.id, ph).await? => Some(main.id),
            None => None,
        };
        if let Some(owner) = sms_owner {
            apply_sms_state(db, owner, ph, c.sms_state.as_deref()).await?;
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
    sqlx::query(
        "INSERT INTO shopify_customers (account_id, customer_id, profile_id, email, phone, email_state, sms_state, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())
         ON CONFLICT (account_id, customer_id) DO UPDATE SET profile_id = EXCLUDED.profile_id, email = EXCLUDED.email, phone = EXCLUDED.phone,
            email_state = EXCLUDED.email_state, sms_state = EXCLUDED.sms_state, updated_at = now()",
    )
    .bind(account_id)
    .bind(c.id)
    .bind(main.id)
    .bind(&email)
    .bind(&phone)
    .bind(&c.email_state)
    .bind(&c.sms_state)
    .execute(db)
    .await?;
    Ok(Some(main.id))
}

/// The profile an order or checkout belongs to: the linked customer's, else the one with the email,
/// else the one with the phone, else a new one with only that identifier. Never changes consent or
/// an existing profile's identity. A customer seen for the first time is queued to be read in full
/// (that's where their consent comes from).
pub async fn profile_for(db: &PgPool, account_id: Uuid, customer_id: Option<i64>, email: Option<&str>, phone: Option<&str>, first: Option<&str>, last: Option<&str>) -> anyhow::Result<Option<Uuid>> {
    let email = email.and_then(normalize_email);
    let phone = phone.and_then(normalize_phone);
    if let Some(cid) = customer_id {
        let linked: Option<Option<Uuid>> = sqlx::query_scalar("SELECT profile_id FROM shopify_customers WHERE account_id = $1 AND customer_id = $2")
            .bind(account_id)
            .bind(cid)
            .fetch_optional(db)
            .await?;
        if let Some(Some(id)) = linked {
            if by_id(db, account_id, id).await?.is_some() {
                return Ok(Some(id));
            }
        }
    }
    let found = match (&email, &phone) {
        (Some(e), _) => match by_email(db, account_id, e).await? {
            Some(p) => Some(p),
            None => create(db, account_id, Some(e), None, first, last).await?,
        },
        (None, Some(ph)) => create(db, account_id, None, Some(ph), first, last).await?,
        _ => None,
    };
    if let (Some(cid), Some(p)) = (customer_id, &found) {
        let new: Option<i64> = sqlx::query_scalar(
            "INSERT INTO shopify_customers (account_id, customer_id, profile_id) VALUES ($1, $2, $3) ON CONFLICT (account_id, customer_id) DO NOTHING RETURNING customer_id",
        )
        .bind(account_id)
        .bind(cid)
        .bind(p.id)
        .fetch_optional(db)
        .await?;
        if new.is_some() {
            super::products::queue_refresh(db, account_id, "customer", cid).await?;
        }
    }
    Ok(found.map(|p| p.id))
}

/// Read customers again by id (after a webhook said they changed), up to 50 at a time.
pub async fn refresh(state: &AppState, shop: &ShopRow, ids: &[i64]) -> Result<(), ApiError> {
    let gids: Vec<String> = ids.iter().map(|i| gid("Customer", *i)).collect();
    let q = format!("query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ... on Customer {{ {FIELDS} }} }} }}");
    let data = client::graphql(state, shop, &q, json!({"ids": gids})).await?;
    // Ids Shopify doesn't return (null) were deleted: nothing to read.
    for n in data["nodes"].as_array().into_iter().flatten() {
        if let Some(c) = Customer::from_graphql(n) {
            apply(&state.db, shop.account_id, &c).await.map_err(|_| ApiError::Network("database"))?;
        }
    }
    Ok(())
}

/// Shopify deleted the customer: forget the link (the profile and its history stay).
pub async fn unlink(db: &PgPool, account_id: Uuid, customer_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM shopify_customers WHERE account_id = $1 AND customer_id = $2").bind(account_id).bind(customer_id).execute(db).await?;
    Ok(())
}

/// Unsubscribes made in DumbMail, sent to Shopify: linked customers Shopify still has as subscribed
/// (or pending) whose profile, for that same address or number, is unsubscribed here. Up to `max`
/// per call. Returns how many were written.
pub async fn write_back(state: &AppState, shop: &ShopRow, max: i64) -> Result<usize, ApiError> {
    let rows: Vec<(i64, bool, bool)> = sqlx::query_as(
        "SELECT c.customer_id,
                (c.email_state IN ('SUBSCRIBED', 'PENDING') AND p.email = c.email AND p.email_consent = 'unsubscribed') AS email_out,
                (c.sms_state IN ('SUBSCRIBED', 'PENDING') AND p.phone_number = c.phone AND p.sms_consent = 'unsubscribed') AS sms_out
         FROM shopify_customers c JOIN profiles p ON p.id = c.profile_id AND p.account_id = c.account_id
         WHERE c.account_id = $1 AND (
            (c.email_state IN ('SUBSCRIBED', 'PENDING') AND p.email = c.email AND p.email_consent = 'unsubscribed')
            OR (c.sms_state IN ('SUBSCRIBED', 'PENDING') AND p.phone_number = c.phone AND p.sms_consent = 'unsubscribed'))
         LIMIT $2",
    )
    .bind(shop.account_id)
    .bind(max)
    .fetch_all(&state.db)
    .await
    .map_err(|_| ApiError::Network("database"))?;
    let now = Utc::now().to_rfc3339();
    let mut n = 0;
    for (cid, email_out, sms_out) in rows {
        if email_out {
            let d = client::graphql(
                state,
                shop,
                "mutation($input: CustomerEmailMarketingConsentUpdateInput!) { customerEmailMarketingConsentUpdate(input: $input) { customer { id } userErrors { field message code } } }",
                json!({"input": {"customerId": gid("Customer", cid), "emailMarketingConsent": {"marketingState": "UNSUBSCRIBED", "marketingOptInLevel": "SINGLE_OPT_IN", "consentUpdatedAt": now}}}),
            )
            .await?;
            // A customer Shopify no longer has (or without an email) can't be updated; stop trying either way.
            let _ = client::user_error(&d["customerEmailMarketingConsentUpdate"]);
            sqlx::query("UPDATE shopify_customers SET email_state = 'UNSUBSCRIBED', updated_at = now() WHERE account_id = $1 AND customer_id = $2")
                .bind(shop.account_id)
                .bind(cid)
                .execute(&state.db)
                .await
                .map_err(|_| ApiError::Network("database"))?;
            n += 1;
        }
        if sms_out {
            let d = client::graphql(
                state,
                shop,
                "mutation($input: CustomerSmsMarketingConsentUpdateInput!) { customerSmsMarketingConsentUpdate(input: $input) { customer { id } userErrors { field message code } } }",
                json!({"input": {"customerId": gid("Customer", cid), "smsMarketingConsent": {"marketingState": "UNSUBSCRIBED", "marketingOptInLevel": "SINGLE_OPT_IN", "consentUpdatedAt": now}}}),
            )
            .await?;
            let _ = client::user_error(&d["customerSmsMarketingConsentUpdate"]);
            sqlx::query("UPDATE shopify_customers SET sms_state = 'UNSUBSCRIBED', updated_at = now() WHERE account_id = $1 AND customer_id = $2")
                .bind(shop.account_id)
                .bind(cid)
                .execute(&state.db)
                .await
                .map_err(|_| ApiError::Network("database"))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Is there anyone to write back for this account? (Cheap; the scheduler asks.)
pub async fn writeback_due(db: &PgPool) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT DISTINCT c.account_id FROM shopify_customers c JOIN profiles p ON p.id = c.profile_id
         JOIN shopify_shops s ON s.account_id = c.account_id AND s.status = 'connected'
         WHERE (c.email_state IN ('SUBSCRIBED', 'PENDING') AND p.email = c.email AND p.email_consent = 'unsubscribed')
            OR (c.sms_state IN ('SUBSCRIBED', 'PENDING') AND p.phone_number = c.phone AND p.sms_consent = 'unsubscribed')
         LIMIT 200",
    )
    .fetch_all(db)
    .await
}

pub fn parse_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|x| DateTime::parse_from_rfc3339(x).ok()).map(|d| d.with_timezone(&Utc))
}

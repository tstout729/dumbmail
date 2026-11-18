//! Shopify's mandatory privacy webhooks. They act on Shopify's own ids (customer, orders, store),
//! never on whoever happens to share an email address or phone number.
//!
//! What came from which store and customer is recorded as it arrives: the customer link
//! (`shopify_customers`), every profile a customer touched (`shopify_customer_profiles`), profiles
//! this app created (`shopify_created_profiles`), and on every event the store
//! (`shopify_shop`), Shopify customer id and order id. Past installs are kept
//! (`shopify_installs`), so a request still finds the account after the connection is gone.
//!
//! - `customers/data_request`: gather that customer's profiles (linked by Shopify customer id), their
//!   events and messages, and the store's events for the requested orders into an export the store
//!   owner downloads from Settings → Shopify, and email the account's Owner that it's there (the
//!   email says only that, not the data).
//! - `customers/redact`: delete that customer's Shopify data: the link, the order and checkout events
//!   for that customer and for `orders_to_redact`, the Shopify properties on their profiles, the
//!   consent Shopify reported for their address and number, and any export made for them. A profile
//!   this app created is deleted too when nothing else is on it (no other events, no list
//!   membership); otherwise it stays without the Shopify data.
//! - `shop/redact` (Shopify sends it 48 hours after the app is uninstalled): the same for every
//!   customer of that store, plus the store's catalog products, queued work, stored deliveries,
//!   imports, discount code records and exports, and the connection itself.
//!
//! An erasure marker is written first, so a queued or resumed import, or a late webhook, skips what
//! was erased instead of bringing it back. Each request is retried with backoff on failure without
//! holding up the others; one still failing after 7 days is logged as an error and shown in status
//! (Shopify expects an answer within 30 days).

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::customers::{num_id, PROPERTY_KEYS};
use crate::models::{Profile, PROFILE_COLS};
use crate::state::AppState;

/// Most events and messages put in one export (newest first).
const MAX_ROWS: i64 = 10_000;
/// Exports are kept this long.
pub const KEEP_DAYS: i32 = 90;
/// Rows deleted per statement when a store is erased.
const BATCH: i64 = 5_000;
/// A request failing for this long is raised as an operator alert.
pub const ALERT_DAYS: i32 = 7;

/// The metrics this app records (and so may erase).
pub const METRICS: &[&str] = &["Placed Order", "Ordered Product", "Fulfilled Order", "Fulfilled Partial Order", "Cancelled Order", "Refunded Order", "Started Checkout"];

/// Process the account's waiting privacy requests, one at a time until `deadline` (each claimed so
/// two runners never do the same one). A failure is retried later and the next one goes ahead.
pub async fn run_until(state: &AppState, account_id: Uuid, deadline: std::time::Instant) -> anyhow::Result<()> {
    while std::time::Instant::now() < deadline {
        let claimed: Option<(i64, String, Option<Value>, Option<String>, i32, bool)> = sqlx::query_as(
            "UPDATE shopify_webhooks SET claimed_until = now() + interval '5 minutes'
             WHERE id = (SELECT id FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic = ANY($2)
                           AND (next_at IS NULL OR next_at <= now()) AND (claimed_until IS NULL OR claimed_until < now())
                         ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING id, topic, payload, shop_domain, attempts, received_at < now() - make_interval(days => $3)",
        )
        .bind(account_id)
        .bind(super::webhooks::PRIVACY_TOPICS)
        .bind(ALERT_DAYS)
        .fetch_optional(&state.db)
        .await?;
        let Some((id, topic, payload, shop, attempts, overdue)) = claimed else { return Ok(()) };
        let payload = payload.unwrap_or(Value::Null);
        let shop = shop.or_else(|| payload["shop_domain"].as_str().and_then(super::shop::valid_shop)).unwrap_or_default();
        let r = match topic.as_str() {
            "customers/data_request" => data_request(state, account_id, &shop, &payload).await,
            "customers/redact" => redact_customer(&state.db, account_id, &shop, &payload).await,
            "shop/redact" => redact_shop(&state.db, account_id, &shop).await,
            _ => Ok(()),
        };
        match r {
            Ok(()) => {
                sqlx::query("UPDATE shopify_webhooks SET done_at = now(), payload = NULL, error = NULL, claimed_until = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
            }
            Err(e) => {
                if overdue {
                    tracing::error!(account = %account_id, topic, attempts, error = %format!("{e:#}"), "a Shopify privacy request has been failing for {ALERT_DAYS} days; Shopify expects it done within 30");
                } else {
                    tracing::warn!(account = %account_id, topic, error = %format!("{e:#}"), "a Shopify privacy request failed; trying again later");
                }
                sqlx::query(
                    "UPDATE shopify_webhooks SET attempts = attempts + 1, error = 'processing failed', claimed_until = NULL,
                        next_at = now() + LEAST(interval '6 hours', interval '1 minute' * power(2, LEAST(attempts, 12)))
                     WHERE id = $1",
                )
                .bind(id)
                .execute(&state.db)
                .await?;
            }
        }
    }
    Ok(())
}

/// Job: process the account's waiting privacy requests.
pub async fn run(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    run_until(state, account_id, std::time::Instant::now() + std::time::Duration::from_secs(60)).await
}

/// Accounts with a privacy request due now and no job queued for it (the scheduler asks).
pub async fn queue_due(db: &PgPool) -> sqlx::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT w.account_id FROM shopify_webhooks w
         WHERE w.done_at IS NULL AND w.topic = ANY($1) AND (w.next_at IS NULL OR w.next_at <= now()) AND (w.claimed_until IS NULL OR w.claimed_until < now())
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'shopify_privacy' AND j.payload->>'account_id' = w.account_id::text AND j.run_at <> 'infinity')
         LIMIT 200",
    )
    .bind(super::webhooks::PRIVACY_TOPICS)
    .fetch_all(db)
    .await?;
    for a in due {
        crate::jobs::enqueue_background(db, "shopify_privacy", json!({"account_id": a}), None).await?;
    }
    Ok(())
}

/// Privacy requests not yet done, and those failing for [`ALERT_DAYS`] (for status).
pub async fn pending(db: &PgPool, account_id: Uuid) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE received_at < now() - make_interval(days => $3))
         FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic = ANY($2)",
    )
    .bind(account_id)
    .bind(super::webhooks::PRIVACY_TOPICS)
    .bind(ALERT_DAYS)
    .fetch_one(db)
    .await
}

async fn metric_ids(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar("SELECT id FROM metrics WHERE account_id = $1 AND name = ANY($2)").bind(account_id).bind(METRICS).fetch_all(db).await
}

/// Profiles linked to this store's customer by Shopify's id.
async fn customer_profiles(db: &PgPool, account_id: Uuid, shop: &str, customer_id: i64) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT DISTINCT p.id FROM profiles p WHERE p.account_id = $1 AND p.id IN (
            SELECT profile_id FROM shopify_customer_profiles WHERE account_id = $1 AND shop_domain = $2 AND customer_id = $3
            UNION SELECT profile_id FROM shopify_customers WHERE account_id = $1 AND customer_id = $3 AND shop_domain = $2 AND profile_id IS NOT NULL)
         LIMIT 50",
    )
    .bind(account_id)
    .bind(shop)
    .bind(customer_id)
    .fetch_all(db)
    .await
}

fn order_ids(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(num_id).take(5_000).map(|o| o.to_string()).collect()).unwrap_or_default()
}

async fn data_request(state: &AppState, account_id: Uuid, shop: &str, p: &Value) -> anyhow::Result<()> {
    let db = &state.db;
    let cid = num_id(&p["customer"]["id"]);
    let orders = order_ids(&p["orders_requested"]);
    let ids = match cid {
        Some(c) => customer_profiles(db, account_id, shop, c).await?,
        None => Vec::new(),
    };
    let metrics = metric_ids(db, account_id).await?;
    let profiles: Vec<Profile> = sqlx::query_as::<_, Profile>(&format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.account_id = $1 AND p.id = ANY($2)"))
        .bind(account_id)
        .bind(&ids)
        .fetch_all(db)
        .await?;
    // Only this store's events for this customer (by Shopify customer id) and for the orders asked
    // about. Two customers who used the same address share a profile: neither's export includes
    // the other's orders.
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('metric', m.name, 'occurred_at', e.occurred_at, 'value', e.value, 'properties', e.properties, 'profile_id', e.profile_id)
         FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.account_id = $1 AND e.metric_id = ANY($2) AND e.properties->>'shopify_shop' = $3
           AND ((e.properties->>'shopify_customer_id') = $4 OR (e.properties->>'order_id') = ANY($5))
         ORDER BY e.occurred_at DESC LIMIT $6",
    )
    .bind(account_id)
    .bind(&metrics)
    .bind(shop)
    .bind(cid.map(|c| c.to_string()))
    .bind(&orders)
    .bind(MAX_ROWS)
    .fetch_all(db)
    .await?;
    let messages: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('channel', channel, 'to', to_address, 'subject', subject, 'status', status, 'created_at', created_at, 'sent_at', sent_at,
                'opened_at', opened_at, 'clicked_at', clicked_at, 'unsubscribed_at', unsubscribed_at, 'profile_id', profile_id)
         FROM messages WHERE account_id = $1 AND profile_id = ANY($2) ORDER BY created_at DESC LIMIT $3",
    )
    .bind(account_id)
    .bind(&ids)
    .bind(MAX_ROWS)
    .fetch_all(db)
    .await?;
    let export = json!({
        "requested_by": "Shopify customer data request",
        "shop": shop,
        "shopify_request_id": p["data_request"]["id"],
        "shopify_customer": {"id": p["customer"]["id"], "email": p["customer"]["email"], "phone": p["customer"]["phone"]},
        "orders_requested": p["orders_requested"],
        "profiles": profiles,
        "events": events,
        "messages": messages,
        "note": format!("What DumbMail holds for this Shopify customer in this account: the profiles linked to their Shopify customer id and the messages sent to them, and up to {MAX_ROWS} of this store's events for this customer and for the orders asked about. Profiles that only share an email address or phone number with them, and other customers' orders, are not included."),
    });
    let request_id = p["data_request"]["id"].as_i64();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO shopify_data_requests (id, account_id, shopify_request_id, shopify_customer_id, shop_domain, export)
         SELECT $1, $2, $3, $4, $5, $6 WHERE NOT EXISTS (SELECT 1 FROM shopify_data_requests WHERE account_id = $2 AND shopify_request_id = $3)
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(request_id)
    .bind(cid)
    .bind(shop)
    .bind(&export)
    .fetch_optional(db)
    .await?;
    if inserted.is_some() {
        notify_owner(
            state,
            account_id,
            "A customer asked your Shopify store for their data",
            &format!(
                "A customer asked your Shopify store for the data held about them. DumbMail has prepared what it holds for them.\n\nDownload it from Settings → Shopify → Data requests ({}/settings/shopify) and send it to the customer. Shopify expects requests to be answered within 30 days.",
                state.cfg.public_url()
            ),
        )
        .await;
    }
    Ok(())
}

async fn mark(conn: &mut sqlx::PgConnection, account_id: Uuid, shop: &str, kind: &str, id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO shopify_erasures (account_id, shop_domain, kind, shopify_id) VALUES ($1, $2, $3, $4)
         ON CONFLICT (account_id, shop_domain, kind, shopify_id) DO NOTHING",
    )
    .bind(account_id)
    .bind(shop)
    .bind(kind)
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Remove the Shopify data from these profiles, and delete the ones this app created that have
/// nothing else on them: no events left, no list membership, no other store's link, and no link to
/// another customer of this store (other than `except`, the one being erased) still kept. Links of
/// installs up to `upto` (a shop redaction for old installs) don't count as kept.
async fn clean_profiles(db: &PgPool, account_id: Uuid, shop: &str, ids: &[Uuid], except: Option<i64>, upto: Option<i32>) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = PROPERTY_KEYS.iter().map(|k| k.to_string()).collect();
    sqlx::query("UPDATE profiles SET properties = properties - $3::text[], updated_at = now() WHERE account_id = $1 AND id = ANY($2)")
        .bind(account_id)
        .bind(ids)
        .bind(&keys)
        .execute(db)
        .await?;
    // Deleted through the one erasure (features::people::privacy), so reviews, push devices, object
    // links and stored exports go with them and an unsubscribe is kept as a one-way code.
    let mut tx = db.begin().await?;
    let doomed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT p.id FROM profiles p WHERE p.account_id = $1 AND p.id = ANY($2)
           AND EXISTS (SELECT 1 FROM shopify_created_profiles c WHERE c.account_id = $1 AND c.profile_id = p.id AND c.shop_domain = $3)
           AND NOT EXISTS (SELECT 1 FROM events e WHERE e.account_id = $1 AND e.profile_id = p.id)
           AND NOT EXISTS (SELECT 1 FROM list_members l WHERE l.profile_id = p.id)
           AND NOT EXISTS (SELECT 1 FROM shopify_customer_profiles o WHERE o.account_id = $1 AND o.profile_id = p.id AND o.shop_domain <> $3)
           AND NOT EXISTS (SELECT 1 FROM shopify_customer_profiles o WHERE o.account_id = $1 AND o.profile_id = p.id AND o.shop_domain = $3
                             AND o.customer_id <> 0 AND o.customer_id IS DISTINCT FROM $4 AND COALESCE(o.generation, 0) > COALESCE($5, -1))
         ORDER BY p.id FOR UPDATE",
    )
    .bind(account_id)
    .bind(ids)
    .bind(shop)
    .bind(except)
    .bind(upto)
    .fetch_all(&mut *tx)
    .await?;
    crate::features::people::privacy::erase_in(&mut tx, account_id, &doomed).await?;
    tx.commit().await?;
    Ok(())
}

async fn redact_customer(db: &PgPool, account_id: Uuid, shop: &str, p: &Value) -> anyhow::Result<()> {
    let cid = num_id(&p["customer"]["id"]);
    let orders = order_ids(&p["orders_to_redact"]);
    let order_nums: Vec<i64> = orders.iter().filter_map(|o| o.parse().ok()).collect();
    // First the markers, under the erasure locks: a write already inside a fence finishes first (and
    // is erased below); any later one sees the markers and writes nothing.
    let mut tx = super::fence::erasing(db, account_id, shop, false, cid, &order_nums).await?;
    if let Some(c) = cid {
        mark(&mut tx, account_id, shop, "customer", c).await?;
    }
    for o in &order_nums {
        mark(&mut tx, account_id, shop, "order", *o).await?;
    }
    tx.commit().await?;
    let mut ids = match cid {
        Some(c) => customer_profiles(db, account_id, shop, c).await?,
        None => Vec::new(),
    };
    let metrics = metric_ids(db, account_id).await?;
    let cid_s = cid.map(|c| c.to_string());
    // The profiles the erased orders were recorded on (a guest order's contact) are cleaned too.
    let order_profiles: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT profile_id FROM events WHERE account_id = $1 AND metric_id = ANY($2) AND properties->>'shopify_shop' = $3 AND (properties->>'order_id') = ANY($4) LIMIT 1000",
    )
    .bind(account_id)
    .bind(&metrics)
    .bind(shop)
    .bind(&orders)
    .fetch_all(db)
    .await?;
    for p in order_profiles {
        if !ids.contains(&p) {
            ids.push(p);
        }
    }
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM events WHERE account_id = $1 AND metric_id = ANY($2) AND properties->>'shopify_shop' = $3
           AND ((properties->>'shopify_customer_id') = $4 OR (properties->>'order_id') = ANY($5))",
    )
    .bind(account_id)
    .bind(&metrics)
    .bind(shop)
    .bind(&cid_s)
    .bind(&orders)
    .execute(&mut *tx)
    .await?;
    if let Some(c) = cid {
        // Shopify's consent for every address and number this customer has had here (the link's,
        // and those of the profiles linked to them), unless another customer of the store still
        // has that address or number.
        sqlx::query(
            "WITH mine AS (
                SELECT 'email' AS channel, email AS contact FROM shopify_customers WHERE account_id = $1 AND shop_domain = $2 AND customer_id = $3 AND email IS NOT NULL
                UNION SELECT 'sms', phone FROM shopify_customers WHERE account_id = $1 AND shop_domain = $2 AND customer_id = $3 AND phone IS NOT NULL
                UNION SELECT 'email', p.email FROM shopify_customer_profiles l JOIN profiles p ON p.id = l.profile_id
                      WHERE l.account_id = $1 AND l.shop_domain = $2 AND l.customer_id = $3 AND p.email IS NOT NULL
                UNION SELECT 'sms', p.phone_number FROM shopify_customer_profiles l JOIN profiles p ON p.id = l.profile_id
                      WHERE l.account_id = $1 AND l.shop_domain = $2 AND l.customer_id = $3 AND p.phone_number IS NOT NULL)
             DELETE FROM shopify_consent k USING mine m
             WHERE k.account_id = $1 AND k.channel = m.channel AND k.contact = m.contact
               AND NOT EXISTS (SELECT 1 FROM shopify_customers o WHERE o.account_id = $1 AND o.customer_id <> $3
                                 AND ((m.channel = 'email' AND o.email = m.contact) OR (m.channel = 'sms' AND o.phone = m.contact)))",
        )
        .bind(account_id)
        .bind(shop)
        .bind(c)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM shopify_customers WHERE account_id = $1 AND shop_domain = $2 AND customer_id = $3").bind(account_id).bind(shop).bind(c).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM shopify_refresh WHERE account_id = $1 AND kind = 'customer' AND shopify_id = $2").bind(account_id).bind(c).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM shopify_data_requests WHERE account_id = $1 AND shop_domain = $2 AND shopify_customer_id = $3").bind(account_id).bind(shop).bind(c).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    clean_profiles(db, account_id, shop, &ids, cid, None).await?;
    if let Some(c) = cid {
        sqlx::query("DELETE FROM shopify_customer_profiles WHERE account_id = $1 AND shop_domain = $2 AND customer_id = $3").bind(account_id).bind(shop).bind(c).execute(db).await?;
    }
    Ok(())
}

/// Delete in batches until nothing is left (each statement small; safe to run again). `$1` account,
/// `$2` shop, `$3` batch size, `$4` the newest install erased, `$5` the metrics (when given).
async fn delete_all(db: &PgPool, sql: &str, account_id: Uuid, shop: &str, upto: i32, extra: Option<&[Uuid]>) -> sqlx::Result<()> {
    loop {
        let mut q = sqlx::query(sql).bind(account_id).bind(shop).bind(BATCH).bind(upto);
        if let Some(e) = extra {
            q = q.bind(e);
        }
        if q.execute(db).await?.rows_affected() == 0 {
            return Ok(());
        }
    }
}

/// `shop/redact`: erase what this store's installs gave the account. If the same store has since
/// connected again (and the account is open), only the earlier installs' data goes: what the
/// current install wrote (or wrote again) stays.
async fn redact_shop(db: &PgPool, account_id: Uuid, shop: &str) -> anyhow::Result<()> {
    let current: Option<(i32, bool)> = sqlx::query_as(
        "SELECT s.generation, s.status = 'connected' AND a.closed_at IS NULL FROM shopify_shops s JOIN accounts a ON a.id = s.account_id
         WHERE s.account_id = $1 AND s.shop_domain = $2",
    )
    .bind(account_id)
    .bind(shop)
    .fetch_optional(db)
    .await?;
    let newest: Option<i32> = sqlx::query_scalar("SELECT max(generation) FROM shopify_installs WHERE account_id = $1 AND shop_domain = $2").bind(account_id).bind(shop).fetch_one(db).await?;
    let live = current.is_some_and(|c| c.1);
    // The newest install this redaction covers.
    let upto = match current {
        Some((g, true)) => g - 1,
        Some((g, false)) => g.max(newest.unwrap_or(0)),
        None => newest.unwrap_or(0),
    };
    // The marker (under the store's erasure lock): imports and webhooks of installs up to `upto`
    // write nothing more; a later install of the same store isn't affected.
    let mut tx = super::fence::erasing(db, account_id, shop, true, None, &[]).await?;
    mark(&mut tx, account_id, shop, "shop", i64::from(upto)).await?;
    tx.commit().await?;
    let metrics = metric_ids(db, account_id).await?;
    delete_all(
        db,
        "DELETE FROM events WHERE id IN (SELECT id FROM events WHERE account_id = $1 AND metric_id = ANY($5) AND properties->>'shopify_shop' = $2
            AND (CASE WHEN properties->>'shopify_generation' ~ '^[0-9]{1,9}$' THEN (properties->>'shopify_generation')::int ELSE 0 END) <= $4 LIMIT $3)",
        account_id,
        shop,
        upto,
        Some(&metrics),
    )
    .await?;
    // The store's customers' profiles, in batches.
    loop {
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT profile_id FROM shopify_customer_profiles WHERE account_id = $1 AND shop_domain = $2 AND COALESCE(generation, 0) <= $4 LIMIT $3")
            .bind(account_id)
            .bind(shop)
            .bind(BATCH)
            .bind(upto)
            .fetch_all(db)
            .await?;
        if ids.is_empty() {
            break;
        }
        clean_profiles(db, account_id, shop, &ids, None, Some(upto)).await?;
        sqlx::query("DELETE FROM shopify_customer_profiles WHERE account_id = $1 AND shop_domain = $2 AND profile_id = ANY($3) AND COALESCE(generation, 0) <= $4")
            .bind(account_id)
            .bind(shop)
            .bind(&ids)
            .bind(upto)
            .execute(db)
            .await?;
    }
    // Profiles made for guest orders and no longer linked to anything.
    let created: Vec<Uuid> = sqlx::query_scalar("SELECT profile_id FROM shopify_created_profiles WHERE account_id = $1 AND shop_domain = $2 AND COALESCE(generation, 0) <= $3 LIMIT 100000")
        .bind(account_id)
        .bind(shop)
        .bind(upto)
        .fetch_all(db)
        .await?;
    for chunk in created.chunks(BATCH as usize) {
        clean_profiles(db, account_id, shop, chunk, None, Some(upto)).await?;
    }
    sqlx::query(
        "DELETE FROM shopify_consent c WHERE c.account_id = $1 AND EXISTS (SELECT 1 FROM shopify_customers s WHERE s.account_id = $1 AND s.shop_domain = $2
            AND COALESCE(s.generation, 0) <= $3 AND ((c.channel = 'email' AND c.contact = s.email) OR (c.channel = 'sms' AND c.contact = s.phone)))",
    )
    .bind(account_id)
    .bind(shop)
    .bind(upto)
    .execute(db)
    .await?;
    // The store's products leave the catalog.
    delete_all(
        db,
        "DELETE FROM catalog_products WHERE id IN (SELECT c.id FROM catalog_products c JOIN shopify_products s ON s.account_id = c.account_id AND s.product_id::text = c.external_id
           WHERE c.account_id = $1 AND c.source = 'shopify' AND s.shop_domain = $2 AND COALESCE(s.generation, 0) <= $4 LIMIT $3)",
        account_id,
        shop,
        upto,
        None,
    )
    .await?;
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM shopify_inventory_items WHERE account_id = $1 AND product_id IN (SELECT product_id FROM shopify_products WHERE account_id = $1 AND shop_domain = $2 AND COALESCE(generation, 0) <= $3)")
        .bind(account_id)
        .bind(shop)
        .bind(upto)
        .execute(&mut *tx)
        .await?;
    for table in ["shopify_products", "shopify_customers", "shopify_created_profiles", "shopify_imports"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE account_id = $1 AND shop_domain = $2 AND COALESCE(generation, 0) <= $3"))
            .bind(account_id)
            .bind(shop)
            .bind(upto)
            .execute(&mut *tx)
            .await?;
    }
    // Stored deliveries for those installs (this request itself is marked done by the caller).
    sqlx::query("DELETE FROM shopify_webhooks WHERE account_id = $1 AND shop_domain = $2 AND topic <> 'shop/redact' AND COALESCE(generation, 0) <= $3")
        .bind(account_id)
        .bind(shop)
        .bind(upto)
        .execute(&mut *tx)
        .await?;
    if !live {
        for table in ["shopify_discount_codes", "shopify_data_requests"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE account_id = $1 AND shop_domain = $2")).bind(account_id).bind(shop).execute(&mut *tx).await?;
        }
    }
    if current.is_some_and(|c| !c.1) {
        for table in ["shopify_refresh", "shopify_inventory_items", "shopify_oauth_states"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE account_id = $1")).bind(account_id).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM shopify_shops WHERE account_id = $1 AND shop_domain = $2 AND status <> 'connected'").bind(account_id).bind(shop).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE shopify_installs SET redacted_at = now() WHERE account_id = $1 AND shop_domain = $2 AND generation <= $3")
        .bind(account_id)
        .bind(shop)
        .bind(upto)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Tell the account's Owner, from SUPPORT_EMAIL. Only that a request exists, never the data.
async fn notify_owner(state: &AppState, account_id: Uuid, subject: &str, text: &str) {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, "no SUPPORT_EMAIL set; Shopify data request notice not emailed (it's on the Shopify settings page)");
        return;
    };
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND m.role = 'owner' ORDER BY m.created_at LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(to) = owner else {
        tracing::warn!(account = %account_id, "no Owner to tell about a Shopify data request (it's on the Shopify settings page)");
        return;
    };
    let html = format!("<div style=\"font-family:system-ui,sans-serif;font-size:15px;line-height:1.5;white-space:pre-wrap\">{}</div>", text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"));
    let email = crate::providers::email::OutgoingEmail {
        message_id: Uuid::now_v7(),
        to,
        from_name: "DumbMail".into(),
        from_email: from.to_string(),
        reply_to: Some(from.to_string()),
        subject: subject.to_string(),
        html,
        text: text.to_string(),
        unsubscribe_url: None,
    };
    if let Err(e) = state.email.send(&email).await {
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email the Shopify data request notice");
    }
}

/// Data requests for status: Shopify's ids, never the customer's email.
pub async fn list(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'shopify_customer_id', shopify_customer_id::text, 'shopify_request_id', shopify_request_id::text, 'shop', shop_domain,
                'created_at', created_at, 'downloaded_at', downloaded_at)
         FROM shopify_data_requests WHERE account_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(db)
    .await
}

/// The export (and record that it was downloaded).
pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> sqlx::Result<Option<Value>> {
    sqlx::query_scalar("UPDATE shopify_data_requests SET downloaded_at = COALESCE(downloaded_at, now()) WHERE account_id = $1 AND id = $2 RETURNING export")
        .bind(account_id)
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn prune(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM shopify_data_requests WHERE created_at < now() - make_interval(days => $1)").bind(KEEP_DAYS).execute(db).await?;
    Ok(())
}

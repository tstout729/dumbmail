//! Webhooks from Shopify: verified, deduplicated, stored and acknowledged at once; processed by the
//! account's worker.
//!
//! A delivery is trusted only when `X-Shopify-Hmac-Sha256` is the HMAC-SHA256 of the raw body with
//! the app secret (compared in constant time). It's stored once per `X-Shopify-Webhook-Id`; a
//! redelivery is acknowledged and ignored. Product, customer and inventory topics keep only the
//! item's id (the worker reads the item from the Admin API); order, refund and checkout bodies are
//! kept until processed, then cleared.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

use super::customers::num_id;
use super::shop::{self, valid_shop};
use crate::state::AppState;

/// Largest webhook body accepted.
pub const MAX_BODY: usize = 2 * 1024 * 1024;

pub const PRIVACY_TOPICS: &[&str] = &["customers/data_request", "customers/redact", "shop/redact"];

pub fn body_hmac_ok(secret: &str, body: &[u8], header: &str) -> bool {
    let Ok(given) = base64::engine::general_purpose::STANDARD.decode(header.trim()) else { return false };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

fn header<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

/// What we keep of a body until it's processed.
fn keep(topic: &str, body: &Value) -> Option<Value> {
    match topic {
        "products/create" | "products/update" | "products/delete" | "customers/create" | "customers/update" | "customers/delete" => Some(json!({"id": num_id(&body["id"])?})),
        "customers_email_marketing_consent/update" | "customers_marketing_consent/update" => Some(json!({"id": num_id(&body["customer_id"]).or_else(|| num_id(&body["id"]))?})),
        "inventory_levels/update" => Some(json!({"inventory_item_id": num_id(&body["inventory_item_id"])?})),
        "bulk_operations/finish" => Some(json!({"id": body["admin_graphql_api_id"]})),
        "app/uninstalled" => Some(json!({})),
        t if t.starts_with("orders/") || t == "refunds/create" || t.starts_with("checkouts/") => Some(body.clone()),
        t if PRIVACY_TOPICS.contains(&t) => Some(body.clone()),
        _ => None,
    }
}

/// POST /webhooks/shopify. Bad signature: 401 (Shopify's requirement). Anything else verified: 200,
/// even if we have no use for it, so Shopify doesn't retry. A privacy request whose signed body
/// names a different store than the header: 400, nothing stored.
pub async fn receive(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let Some(secret) = state.cfg.shopify.secret() else { return StatusCode::NOT_FOUND };
    let Some(sig) = header(&headers, "x-shopify-hmac-sha256") else { return StatusCode::UNAUTHORIZED };
    if body.len() > MAX_BODY || !body_hmac_ok(secret, &body, sig) {
        return StatusCode::UNAUTHORIZED;
    }
    let (Some(topic), Some(domain), Some(webhook_id)) = (header(&headers, "x-shopify-topic"), header(&headers, "x-shopify-shop-domain").and_then(valid_shop), header(&headers, "x-shopify-webhook-id")) else {
        return StatusCode::OK;
    };
    if webhook_id.len() > 200 || topic.len() > 100 {
        return StatusCode::OK;
    }
    let topic = topic.to_ascii_lowercase();
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else { return StatusCode::OK };
    if PRIVACY_TOPICS.contains(&topic.as_str()) {
        // The store a privacy request is about comes from the signed body, and must be the header's.
        let signed = parsed["shop_domain"].as_str().and_then(valid_shop);
        if signed.as_deref() != Some(domain.as_str()) {
            tracing::error!(topic, "a Shopify privacy request named a different store in its body than in its header; refused");
            return StatusCode::BAD_REQUEST;
        }
    }
    let triggered = header(&headers, "x-shopify-triggered-at").and_then(|t| super::customers::parse_time(Some(t)));
    match store(&state, &topic, &domain, webhook_id, &parsed, triggered).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::error!(topic, error = %format!("{e:#}"), "couldn't store a Shopify webhook");
            // Shopify retries a failed delivery; nothing is lost.
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn store(state: &AppState, topic: &str, domain: &str, webhook_id: &str, body: &Value, triggered: Option<chrono::DateTime<chrono::Utc>>) -> anyhow::Result<()> {
    let Some(kept) = keep(topic, body) else { return Ok(()) };
    // Privacy requests concern every account that has ever held this store's data (kept in
    // `shopify_installs`, which outlives the connection); everything else goes only to the account
    // the store is connected to now, for its current install.
    let targets: Vec<(Uuid, Option<i32>)> = if PRIVACY_TOPICS.contains(&topic) {
        sqlx::query_as("SELECT DISTINCT account_id, NULL::int FROM shopify_installs WHERE shop_domain = $1").bind(domain).fetch_all(&state.db).await?
    } else {
        // A closed account gets nothing but privacy requests and the uninstall (which forgets the
        // token and releases the store): anything else is acknowledged and dropped.
        sqlx::query_as(
            "SELECT s.account_id, s.generation FROM shopify_shops s JOIN accounts a ON a.id = s.account_id
             WHERE s.shop_domain = $1 AND s.status = 'connected' AND (a.closed_at IS NULL OR $2 = 'app/uninstalled')",
        )
        .bind(domain)
        .bind(topic)
        .fetch_all(&state.db)
        .await?
    };
    for (account_id, generation) in targets {
        let mut tx = state.db.begin().await?;
        let uninstall = topic == "app/uninstalled";
        // Every topic, uninstalls included, is stored once per delivery id: a redelivery does nothing.
        let new: Option<i64> = sqlx::query_scalar(
            "INSERT INTO shopify_webhooks (account_id, webhook_id, topic, payload, generation, shop_domain, done_at)
             VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7 THEN now() END)
             ON CONFLICT (account_id, webhook_id) DO NOTHING RETURNING id",
        )
        .bind(account_id)
        .bind(webhook_id)
        .bind(topic)
        .bind(if uninstall { None } else { Some(&kept) })
        .bind(generation)
        .bind(domain)
        .bind(uninstall)
        .fetch_optional(&mut *tx)
        .await?;
        if new.is_none() {
            tx.commit().await?;
            continue;
        }
        sqlx::query("UPDATE shopify_shops SET last_webhook_at = now(), last_webhook_topic = $2 WHERE account_id = $1 AND shop_domain = $3").bind(account_id).bind(topic).bind(domain).execute(&mut *tx).await?;
        if uninstall {
            // Stop using the token now; Shopify has already removed the webhooks. Only for the install
            // this was sent for: one Shopify triggered before the current install began is stale.
            let stale: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_shops WHERE account_id = $1 AND $2::timestamptz IS NOT NULL AND connected_at > $2)")
                .bind(account_id)
                .bind(triggered)
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            if stale {
                tracing::info!(account = %account_id, "ignored an app/uninstalled webhook sent before the current install");
            } else if super::oauth::forget(&state.db, account_id, "uninstalled", generation).await? {
                shop::note_error(&state.db, account_id, Some("The DumbMail app was uninstalled from the store in Shopify. Connect it again to resume.")).await?;
            }
            continue;
        }
        if PRIVACY_TOPICS.contains(&topic) {
            // A store that's gone has no worker; privacy work also runs as its own job.
            crate::jobs::enqueue_background(&mut *tx, "shopify_privacy", json!({"account_id": account_id}), None).await?;
        } else {
            shop::kick(&mut tx, account_id).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Process waiting deliveries for the account's current install, oldest first, until `deadline`.
/// Returns true when more are waiting. Each delivery is processed on its own; one that keeps failing
/// is set aside after 5 tries so it can't hold the rest up. Before each one the run checks it still
/// holds the lease for this install, and marking it done checks again in the same statement.
pub async fn drain(state: &AppState, cx: &mut super::orders::Ctx<'_>, lease: &shop::Lease, deadline: std::time::Instant) -> anyhow::Result<bool> {
    // Deliveries for an earlier install of the app are dropped with their data.
    sqlx::query("DELETE FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic <> ALL($2) AND generation IS DISTINCT FROM $3")
        .bind(cx.account_id)
        .bind(PRIVACY_TOPICS)
        .bind(lease.generation)
        .execute(&state.db)
        .await?;
    loop {
        if std::time::Instant::now() >= deadline {
            return Ok(true);
        }
        let batch: Vec<(i64, String, Value)> = sqlx::query_as(
            "SELECT id, topic, payload FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic <> ALL($2) AND generation = $3 ORDER BY id LIMIT 50",
        )
        .bind(cx.account_id)
        .bind(PRIVACY_TOPICS)
        .bind(lease.generation)
        .fetch_all(&state.db)
        .await?;
        if batch.is_empty() {
            return Ok(false);
        }
        for (id, topic, payload) in batch {
            if std::time::Instant::now() >= deadline {
                return Ok(true);
            }
            if !shop::still_mine(&state.db, lease).await? {
                return Ok(false);
            }
            let r = process(state, cx, &topic, &payload).await;
            match r {
                Ok(()) => {
                    sqlx::query(&format!("UPDATE shopify_webhooks SET done_at = now(), payload = NULL, error = NULL WHERE id = $4 AND {}", shop::LEASE_GUARD))
                        .bind(lease.account_id)
                        .bind(lease.token)
                        .bind(lease.generation)
                        .bind(id)
                        .execute(&state.db)
                        .await?;
                }
                Err(e) => {
                    tracing::warn!(account = %cx.account_id, topic, error = %format!("{e:#}"), "couldn't process a Shopify webhook");
                    sqlx::query(
                        "UPDATE shopify_webhooks SET attempts = attempts + 1, error = 'processing failed',
                            done_at = CASE WHEN attempts + 1 >= 5 THEN now() ELSE NULL END,
                            payload = CASE WHEN attempts + 1 >= 5 THEN NULL ELSE payload END
                         WHERE id = $1",
                    )
                    .bind(id)
                    .execute(&state.db)
                    .await?;
                    // Try the rest later rather than spinning on this one.
                    return Ok(true);
                }
            }
        }
    }
}

async fn process(state: &AppState, cx: &mut super::orders::Ctx<'_>, topic: &str, p: &Value) -> anyhow::Result<()> {
    let db = &state.db;
    let account_id = cx.account_id;
    let id = || num_id(&p["id"]);
    match topic {
        t if t.starts_with("orders/") => {
            if let Some(o) = super::orders::Order::from_webhook(p) {
                super::orders::apply(cx, &o).await?;
            }
        }
        "refunds/create" => super::orders::apply_refund(cx, p).await?,
        "checkouts/create" | "checkouts/update" => super::orders::apply_checkout(cx, p).await?,
        "customers/create" | "customers/update" | "customers_email_marketing_consent/update" | "customers_marketing_consent/update" => {
            if let Some(id) = id() {
                super::products::queue_refresh(db, account_id, "customer", id).await?;
            }
        }
        "customers/delete" => {
            if let Some(id) = id() {
                super::customers::unlink(db, account_id, id).await?;
            }
        }
        "products/create" | "products/update" => {
            if let Some(id) = id() {
                super::products::queue_refresh(db, account_id, "product", id).await?;
            }
        }
        "products/delete" => {
            if let Some(id) = id() {
                sqlx::query("DELETE FROM shopify_refresh WHERE account_id = $1 AND kind = 'product' AND shopify_id = $2").bind(account_id).bind(id).execute(db).await?;
                if let Some(s) = shop::load(db, account_id).await? {
                    super::products::mark_deleted(db, &s, id).await?;
                }
            }
        }
        "inventory_levels/update" => {
            if let Some(item) = num_id(&p["inventory_item_id"]) {
                super::products::inventory_changed(db, account_id, item).await?;
            }
        }
        // The import checks its own operation; this just means "look now".
        _ => {}
    }
    Ok(())
}

/// Old delivery records (kept a week to recognise redeliveries), and bodies never processed: after
/// 30 days an unprocessed body (an order, a refund, a privacy request) is deleted and counted on the
/// connection, so nothing a store sent is kept indefinitely.
pub async fn prune(db: &sqlx::PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM shopify_webhooks WHERE done_at < now() - interval '7 days'").execute(db).await?;
    let dropped: Vec<(Uuid, i64)> = sqlx::query_as(
        "WITH d AS (UPDATE shopify_webhooks SET payload = NULL, done_at = now(), error = 'expired unprocessed'
                    WHERE done_at IS NULL AND received_at < now() - interval '30 days' RETURNING account_id)
         SELECT account_id, count(*) FROM d GROUP BY account_id",
    )
    .fetch_all(db)
    .await?;
    for (account_id, n) in dropped {
        tracing::error!(account = %account_id, count = n, "deleted Shopify webhook bodies left unprocessed for 30 days");
        sqlx::query("UPDATE shopify_shops SET dropped_payloads = dropped_payloads + $2 WHERE account_id = $1").bind(account_id).bind(n as i32).execute(db).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_signatures() {
        let body = br#"{"id":1}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert!(body_hmac_ok("secret", body, &sig));
        assert!(!body_hmac_ok("other", body, &sig));
        assert!(!body_hmac_ok("secret", br#"{"id":2}"#, &sig));
        assert!(!body_hmac_ok("secret", body, "not base64!"));
    }

    #[test]
    fn keeps_only_what_processing_needs() {
        let big = json!({"id": 9, "title": "x", "body_html": "lots", "email": "a@b.co"});
        assert_eq!(keep("products/update", &big), Some(json!({"id": 9})));
        assert_eq!(keep("customers/update", &big), Some(json!({"id": 9})));
        assert_eq!(keep("customers_email_marketing_consent/update", &json!({"customer_id": 4, "email_address": "a@b.co"})), Some(json!({"id": 4})));
        assert_eq!(keep("shop/update", &big), None);
    }
}

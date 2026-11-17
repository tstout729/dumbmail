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
/// even if we have no use for it, so Shopify doesn't retry.
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
    match store(&state, &topic, &domain, webhook_id, &parsed).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::error!(topic, error = %format!("{e:#}"), "couldn't store a Shopify webhook");
            // Shopify retries a failed delivery; nothing is lost.
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn store(state: &AppState, topic: &str, domain: &str, webhook_id: &str, body: &Value) -> anyhow::Result<()> {
    let Some(kept) = keep(topic, body) else { return Ok(()) };
    // Privacy requests concern every account that has held this store's data; everything else goes
    // only to the account the store is connected to now.
    let accounts: Vec<Uuid> = if PRIVACY_TOPICS.contains(&topic) {
        let only_gone = topic == "shop/redact";
        sqlx::query_scalar("SELECT account_id FROM shopify_shops WHERE shop_domain = $1 AND (NOT $2 OR status <> 'connected')")
            .bind(domain)
            .bind(only_gone)
            .fetch_all(&state.db)
            .await?
    } else {
        sqlx::query_scalar("SELECT account_id FROM shopify_shops WHERE shop_domain = $1 AND status = 'connected'").bind(domain).fetch_all(&state.db).await?
    };
    for account_id in accounts {
        if topic == "app/uninstalled" {
            // Stop using the token now; Shopify has already removed the webhooks.
            super::oauth::forget(&state.db, account_id, "uninstalled").await?;
            shop::note_error(&state.db, account_id, Some("The DumbMail app was uninstalled from the store in Shopify. Connect it again to resume.")).await?;
            continue;
        }
        let mut tx = state.db.begin().await?;
        let new: Option<i64> = sqlx::query_scalar(
            "INSERT INTO shopify_webhooks (account_id, webhook_id, topic, payload) VALUES ($1, $2, $3, $4) ON CONFLICT (account_id, webhook_id) DO NOTHING RETURNING id",
        )
        .bind(account_id)
        .bind(webhook_id)
        .bind(topic)
        .bind(&kept)
        .fetch_optional(&mut *tx)
        .await?;
        if new.is_some() {
            sqlx::query("UPDATE shopify_shops SET last_webhook_at = now(), last_webhook_topic = $2 WHERE account_id = $1").bind(account_id).bind(topic).execute(&mut *tx).await?;
            if PRIVACY_TOPICS.contains(&topic) {
                // A store that's gone has no worker; privacy work runs as its own job.
                crate::jobs::enqueue(&mut *tx, "shopify_privacy", json!({"account_id": account_id}), None).await?;
            } else {
                shop::kick(&mut tx, account_id).await?;
            }
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Process waiting deliveries for the account, oldest first, until `deadline`. Returns true when
/// more are waiting. Each delivery is processed on its own; one that keeps failing is set aside
/// after 5 tries so it can't hold the rest up.
pub async fn drain(state: &AppState, cx: &mut super::orders::Ctx<'_>, deadline: std::time::Instant) -> anyhow::Result<bool> {
    loop {
        if std::time::Instant::now() >= deadline {
            return Ok(true);
        }
        let batch: Vec<(i64, String, Value)> = sqlx::query_as(
            "SELECT id, topic, payload FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic <> ALL($2) ORDER BY id LIMIT 50",
        )
        .bind(cx.account_id)
        .bind(PRIVACY_TOPICS)
        .fetch_all(&state.db)
        .await?;
        if batch.is_empty() {
            return Ok(false);
        }
        for (id, topic, payload) in batch {
            if std::time::Instant::now() >= deadline {
                return Ok(true);
            }
            let r = process(state, cx, &topic, &payload).await;
            match r {
                Ok(()) => {
                    sqlx::query("UPDATE shopify_webhooks SET done_at = now(), payload = NULL, error = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
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
                super::products::unpublish(db, account_id, id).await?;
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

/// Old delivery records (kept a week to recognise redeliveries).
pub async fn prune(db: &sqlx::PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM shopify_webhooks WHERE done_at < now() - interval '7 days'").execute(db).await?;
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

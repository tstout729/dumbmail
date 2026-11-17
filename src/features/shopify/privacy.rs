//! Shopify's mandatory privacy webhooks.
//!
//! - `customers/data_request`: gather everything DumbMail holds on that customer (profile, events,
//!   messages) into an export the store owner downloads from Settings → Shopify, and email the owner
//!   that it's there (the email says only that, not the data).
//! - `customers/redact`: delete that customer's profiles (by the linked Shopify customer, email and
//!   phone), with their events and messages, and any export made for them.
//! - `shop/redact` (Shopify sends it 48 hours after the app is uninstalled): delete what the Shopify
//!   connection itself holds for the store: its tokens, customer links and consent mirror, webhook
//!   deliveries, imports, discount codes and exports. Profiles, events and products already in the
//!   account stay: they're the account's own audience, which its owner can delete.

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::customers::num_id;
use crate::models::{normalize_email, normalize_phone, Profile, PROFILE_COLS};
use crate::state::AppState;

/// Most events and messages put in one export (newest first).
const MAX_ROWS: i64 = 10_000;
/// Exports are kept this long.
pub const KEEP_DAYS: i32 = 90;

/// Job: process the account's waiting privacy requests.
pub async fn run(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    let rows: Vec<(i64, String, Value)> = sqlx::query_as(
        "SELECT id, topic, payload FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic = ANY($2) ORDER BY id LIMIT 20",
    )
    .bind(account_id)
    .bind(super::webhooks::PRIVACY_TOPICS)
    .fetch_all(&state.db)
    .await?;
    for (id, topic, payload) in rows {
        match topic.as_str() {
            "customers/data_request" => data_request(state, account_id, &payload).await?,
            "customers/redact" => redact_customer(&state.db, account_id, &payload).await?,
            "shop/redact" => {
                redact_shop(&state.db, account_id).await?;
                return Ok(());
            }
            _ => {}
        }
        sqlx::query("UPDATE shopify_webhooks SET done_at = now(), payload = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
    }
    Ok(())
}

/// Profiles in this account that are the customer: linked by Shopify id, or with their email or phone.
async fn profiles_for(db: &PgPool, account_id: Uuid, customer: &Value) -> sqlx::Result<Vec<Uuid>> {
    let cid = num_id(&customer["id"]);
    let email = customer["email"].as_str().and_then(normalize_email);
    let phone = customer["phone"].as_str().and_then(normalize_phone);
    sqlx::query_scalar(
        "SELECT p.id FROM profiles p WHERE p.account_id = $1 AND (
            p.id IN (SELECT profile_id FROM shopify_customers WHERE account_id = $1 AND customer_id = $2)
            OR (p.email = $3) OR (p.phone_number = $4))
         LIMIT 20",
    )
    .bind(account_id)
    .bind(cid)
    .bind(email)
    .bind(phone)
    .fetch_all(db)
    .await
}

async fn data_request(state: &AppState, account_id: Uuid, p: &Value) -> anyhow::Result<()> {
    let db = &state.db;
    let ids = profiles_for(db, account_id, &p["customer"]).await?;
    let profiles: Vec<Profile> = sqlx::query_as::<_, Profile>(&format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.account_id = $1 AND p.id = ANY($2)"))
        .bind(account_id)
        .bind(&ids)
        .fetch_all(db)
        .await?;
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('metric', m.name, 'occurred_at', e.occurred_at, 'value', e.value, 'properties', e.properties, 'profile_id', e.profile_id)
         FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.account_id = $1 AND e.profile_id = ANY($2) ORDER BY e.occurred_at DESC LIMIT $3",
    )
    .bind(account_id)
    .bind(&ids)
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
        "shopify_request_id": p["data_request"]["id"],
        "shopify_customer": {"id": p["customer"]["id"], "email": p["customer"]["email"], "phone": p["customer"]["phone"]},
        "orders_requested": p["orders_requested"],
        "profiles": profiles,
        "events": events,
        "messages": messages,
        "note": format!("Everything DumbMail holds for this customer in this account: profiles matched by Shopify customer, email or phone, with up to {MAX_ROWS} of their newest events and messages each."),
    });
    let email = p["customer"]["email"].as_str().map(|e| e.chars().take(320).collect::<String>());
    let request_id = p["data_request"]["id"].as_i64();
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_data_requests WHERE account_id = $1 AND shopify_request_id = $2)")
        .bind(account_id)
        .bind(request_id)
        .fetch_one(db)
        .await?;
    if !exists {
        sqlx::query("INSERT INTO shopify_data_requests (id, account_id, shopify_request_id, customer_email, export) VALUES ($1, $2, $3, $4, $5)")
            .bind(Uuid::now_v7())
            .bind(account_id)
            .bind(request_id)
            .bind(&email)
            .bind(&export)
            .execute(db)
            .await?;
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

async fn redact_customer(db: &PgPool, account_id: Uuid, p: &Value) -> anyhow::Result<()> {
    let ids = profiles_for(db, account_id, &p["customer"]).await?;
    let mut tx = db.begin().await?;
    if let Some(cid) = num_id(&p["customer"]["id"]) {
        sqlx::query("DELETE FROM shopify_customers WHERE account_id = $1 AND customer_id = $2").bind(account_id).bind(cid).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM profiles WHERE account_id = $1 AND id = ANY($2)").bind(account_id).bind(&ids).execute(&mut *tx).await?;
    if let Some(e) = p["customer"]["email"].as_str() {
        sqlx::query("DELETE FROM shopify_data_requests WHERE account_id = $1 AND lower(customer_email) = lower($2)").bind(account_id).bind(e).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn redact_shop(db: &PgPool, account_id: Uuid) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    // Only a store that's still gone: reconnected since, it's live data again.
    let gone: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_shops WHERE account_id = $1 AND status <> 'connected')").bind(account_id).fetch_one(&mut *tx).await?;
    if gone {
        for table in ["shopify_customers", "shopify_inventory_items", "shopify_refresh", "shopify_imports", "shopify_discount_codes", "shopify_data_requests", "shopify_oauth_states", "shopify_webhooks"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE account_id = $1")).bind(account_id).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM shopify_shops WHERE account_id = $1").bind(account_id).execute(&mut *tx).await?;
    } else {
        sqlx::query("UPDATE shopify_webhooks SET done_at = now(), payload = NULL WHERE account_id = $1 AND topic = 'shop/redact'").bind(account_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Tell the account's first member, from SUPPORT_EMAIL. Only that a request exists, never the data.
async fn notify_owner(state: &AppState, account_id: Uuid, subject: &str, text: &str) {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, "no SUPPORT_EMAIL set; Shopify data request notice not emailed (it's on the Shopify settings page)");
        return;
    };
    let owner: Option<String> = sqlx::query_scalar("SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(to) = owner else { return };
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

pub async fn list(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'customer_email', customer_email, 'created_at', created_at, 'downloaded_at', downloaded_at)
         FROM shopify_data_requests WHERE account_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(db)
    .await
}

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

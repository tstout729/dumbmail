//! The connected store: domain checks, the stored connection, its access token, and the account's
//! one background worker (a lease on the shop row plus one of a few global lanes).

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// A store domain as Shopify defines it: `name.myshopify.com`, lowercase, nothing else. Anything
/// else (a custom domain, a path, a port, `evil.com?.myshopify.com`) is refused, so the OAuth code
/// and the access token only ever go to Shopify.
pub fn valid_shop(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    let s = s.strip_prefix("https://").unwrap_or(&s);
    let s = s.strip_suffix('/').unwrap_or(s);
    let name = s.strip_suffix(".myshopify.com")?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphanumeric() || name.len() > 60 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some(format!("{name}.myshopify.com"))
}

/// What a person typed for their store: `name`, `name.myshopify.com` or the admin link.
pub fn shop_from_input(raw: &str) -> Option<String> {
    let t = raw.trim().to_ascii_lowercase();
    let t = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")).unwrap_or(&t).to_string();
    // admin.shopify.com/store/<name>
    if let Some(rest) = t.strip_prefix("admin.shopify.com/store/") {
        let name = rest.split(['/', '?', '#']).next().unwrap_or("");
        return valid_shop(&format!("{name}.myshopify.com"));
    }
    let host = t.split(['/', '?', '#']).next().unwrap_or("");
    if host.ends_with(".myshopify.com") {
        return valid_shop(host);
    }
    if !host.contains('.') {
        return valid_shop(&format!("{host}.myshopify.com"));
    }
    None
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShopRow {
    pub account_id: Uuid,
    pub shop_domain: String,
    pub status: String,
    pub scopes: String,
    pub access_token_enc: Option<String>,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub refresh_token_enc: Option<String>,
    pub token_hint: Option<String>,
    pub shop_name: Option<String>,
    pub currency: Option<String>,
    pub primary_url: Option<String>,
    pub shop_timezone: Option<String>,
    pub webhook_ids: Vec<String>,
    pub web_pixel_id: Option<String>,
    pub storefront_error: Option<String>,
    pub connected_at: DateTime<Utc>,
    pub disconnected_at: Option<DateTime<Utc>>,
    pub last_webhook_at: Option<DateTime<Utc>>,
    pub last_webhook_topic: Option<String>,
    pub last_order_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
}

const SHOP_COLS: &str = "account_id, shop_domain, status, scopes, access_token_enc, token_expires_at, refresh_token_enc, token_hint, shop_name, currency,
    primary_url, shop_timezone, webhook_ids, web_pixel_id, storefront_error, connected_at, disconnected_at, last_webhook_at, last_webhook_topic,
    last_order_at, last_error, last_error_at";

pub async fn load(db: &PgPool, account_id: Uuid) -> AppResult<Option<ShopRow>> {
    Ok(sqlx::query_as::<_, ShopRow>(&format!("SELECT {SHOP_COLS} FROM shopify_shops WHERE account_id = $1"))
        .bind(account_id)
        .fetch_optional(db)
        .await?)
}

/// The account's store when it's connected.
pub async fn connected(db: &PgPool, account_id: Uuid) -> AppResult<ShopRow> {
    match load(db, account_id).await? {
        Some(s) if s.status == "connected" => Ok(s),
        _ => Err(AppError::Conflict("no Shopify store is connected; connect one in Settings → Shopify".into())),
    }
}

/// Save a short, plain-words problem for the dashboard. Never pass vendor bodies or tokens here.
pub async fn note_error(db: &PgPool, account_id: Uuid, msg: Option<&str>) -> sqlx::Result<()> {
    sqlx::query("UPDATE shopify_shops SET last_error = $2, last_error_at = CASE WHEN $2::text IS NULL THEN last_error_at ELSE now() END, updated_at = now() WHERE account_id = $1")
        .bind(account_id)
        .bind(msg)
        .execute(db)
        .await?;
    Ok(())
}

/// A token's last four characters, for "connected with …abcd".
pub fn hint(token: &str) -> String {
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

// ---------- The account's background worker ----------

/// How long one worker run may hold the account's lease (each run stops well before this).
pub const LEASE_SECS: i64 = 120;
/// How long a run works before handing the worker back to the queue.
pub const SLICE_SECS: u64 = 15;

/// Ask for the account's worker to run soon. Safe to call often and inside a transaction: at most one
/// job is queued per account until it starts.
pub async fn kick(conn: &mut sqlx::PgConnection, account_id: Uuid) -> sqlx::Result<()> {
    let marked: Option<Uuid> = sqlx::query_scalar(
        "UPDATE shopify_shops SET work_queued_at = now() WHERE account_id = $1 AND work_queued_at IS NULL AND status <> 'disconnected' RETURNING account_id",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    if marked.is_some() {
        crate::jobs::enqueue(&mut *conn, "shopify_work", json!({"account_id": account_id}), None).await?;
    }
    Ok(())
}

/// [`kick`] outside a transaction.
pub async fn kick_now(db: &PgPool, account_id: Uuid) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    kick(&mut tx, account_id).await?;
    tx.commit().await
}

pub struct Lease {
    pub account_id: Uuid,
    pub token: Uuid,
    pub slot: i32,
}

/// Take the account's lease and one global lane. `Ok(None)`: someone else holds the lease (they will
/// see the queued flag when they finish) or every lane is busy (the job is queued again shortly).
pub async fn acquire(state: &AppState, account_id: Uuid) -> anyhow::Result<Option<Lease>> {
    let token = Uuid::now_v7();
    let got: Option<Uuid> = sqlx::query_scalar(
        "UPDATE shopify_shops SET lease_until = now() + make_interval(secs => $2), lease_token = $3, work_queued_at = NULL
         WHERE account_id = $1 AND (lease_until IS NULL OR lease_until < now()) RETURNING account_id",
    )
    .bind(account_id)
    .bind(LEASE_SECS as f64)
    .bind(token)
    .fetch_optional(&state.db)
    .await?;
    if got.is_none() {
        return Ok(None);
    }
    let slot: Option<i32> = sqlx::query_scalar(
        "UPDATE shopify_lanes SET account_id = $1, lease_until = now() + make_interval(secs => $2)
         WHERE slot = (SELECT slot FROM shopify_lanes WHERE lease_until IS NULL OR lease_until < now() ORDER BY slot LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING slot",
    )
    .bind(account_id)
    .bind(LEASE_SECS as f64)
    .fetch_optional(&state.db)
    .await?;
    let Some(slot) = slot else {
        // All lanes busy: give the lease back and try again in a few seconds.
        sqlx::query("UPDATE shopify_shops SET lease_until = NULL, lease_token = NULL, work_queued_at = now() WHERE account_id = $1 AND lease_token = $2")
            .bind(account_id)
            .bind(token)
            .execute(&state.db)
            .await?;
        crate::jobs::enqueue(&state.db, "shopify_work", json!({"account_id": account_id}), Some(Utc::now() + Duration::seconds(5))).await?;
        return Ok(None);
    };
    Ok(Some(Lease { account_id, token, slot }))
}

/// Give the lease and lane back. With `more` (work left, or a wait such as a bulk operation still
/// running), or when someone asked for the worker while it ran, it's queued again (after `delay`).
pub async fn release(state: &AppState, lease: &Lease, more: bool, delay: Option<Duration>) -> anyhow::Result<()> {
    sqlx::query("UPDATE shopify_lanes SET account_id = NULL, lease_until = NULL WHERE slot = $1 AND account_id = $2")
        .bind(lease.slot)
        .bind(lease.account_id)
        .execute(&state.db)
        .await?;
    let asked: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "UPDATE shopify_shops s SET lease_until = NULL, lease_token = NULL FROM (SELECT work_queued_at FROM shopify_shops WHERE account_id = $1) old
         WHERE s.account_id = $1 AND s.lease_token = $2 RETURNING old.work_queued_at",
    )
    .bind(lease.account_id)
    .bind(lease.token)
    .fetch_optional(&state.db)
    .await?;
    let asked = matches!(asked, Some(Some(_)));
    if more || asked {
        let mut tx = state.db.begin().await?;
        sqlx::query("UPDATE shopify_shops SET work_queued_at = now() WHERE account_id = $1").bind(lease.account_id).execute(&mut *tx).await?;
        let at = if asked { None } else { delay.map(|d| Utc::now() + d) };
        crate::jobs::enqueue(&mut *tx, "shopify_work", json!({"account_id": lease.account_id}), at).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Keep the lease while a run is still inside its slice.
pub async fn renew(state: &AppState, lease: &Lease) -> anyhow::Result<bool> {
    let n = sqlx::query("UPDATE shopify_shops SET lease_until = now() + make_interval(secs => $3) WHERE account_id = $1 AND lease_token = $2")
        .bind(lease.account_id)
        .bind(lease.token)
        .bind(LEASE_SECS as f64)
        .execute(&state.db)
        .await?
        .rows_affected();
    sqlx::query("UPDATE shopify_lanes SET lease_until = now() + make_interval(secs => $3) WHERE slot = $1 AND account_id = $2")
        .bind(lease.slot)
        .bind(lease.account_id)
        .bind(LEASE_SECS as f64)
        .execute(&state.db)
        .await?;
    Ok(n == 1)
}

/// Safety net for the scheduler: workers that should be running but aren't (a crash between
/// claiming and finishing, or a lost job).
pub async fn requeue_stuck(db: &PgPool) -> sqlx::Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.account_id FROM shopify_shops s
         WHERE s.status <> 'disconnected' AND (s.lease_until IS NULL OR s.lease_until < now())
           AND ((s.work_queued_at IS NOT NULL AND s.work_queued_at < now() - interval '3 minutes')
             OR (s.work_queued_at IS NULL AND (
                  EXISTS (SELECT 1 FROM shopify_webhooks w WHERE w.account_id = s.account_id AND w.done_at IS NULL AND w.received_at < now() - interval '1 minute')
               OR EXISTS (SELECT 1 FROM shopify_refresh r WHERE r.account_id = s.account_id AND r.queued_at < now() - interval '1 minute')
               OR EXISTS (SELECT 1 FROM shopify_imports i WHERE i.account_id = s.account_id AND i.status = 'running' AND i.updated_at < now() - interval '2 minutes'))))
         LIMIT 200",
    )
    .fetch_all(db)
    .await?;
    for id in ids {
        let mut tx = db.begin().await?;
        sqlx::query("UPDATE shopify_shops SET work_queued_at = now() WHERE account_id = $1").bind(id).execute(&mut *tx).await?;
        crate::jobs::enqueue(&mut *tx, "shopify_work", json!({"account_id": id}), None).await?;
        tx.commit().await?;
    }
    Ok(())
}

pub fn status_json(s: &ShopRow) -> Value {
    json!({
        "shop": s.shop_domain,
        "status": s.status,
        "name": s.shop_name,
        "currency": s.currency,
        "store_url": s.primary_url,
        "scopes": s.scopes.split(',').filter(|x| !x.is_empty()).collect::<Vec<_>>(),
        "token_hint": s.token_hint,
        "connected_at": s.connected_at,
        "disconnected_at": s.disconnected_at,
        "last_webhook_at": s.last_webhook_at,
        "last_webhook_topic": s.last_webhook_topic,
        "last_order_at": s.last_order_at,
        "last_error": s.last_error,
        "last_error_at": s.last_error_at,
        "web_pixel_installed": s.web_pixel_id.is_some(),
        "storefront_error": s.storefront_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shop_domains() {
        assert_eq!(valid_shop("Example-Shop.myshopify.com").as_deref(), Some("example-shop.myshopify.com"));
        assert_eq!(valid_shop("https://example.myshopify.com/").as_deref(), Some("example.myshopify.com"));
        for bad in ["example.myshopify.com.evil.com", "evil.com", "-x.myshopify.com", "a_b.myshopify.com", "x.myshopify.com:8443", "x.myshopify.com/admin", ".myshopify.com", "a.b.myshopify.com", "x.myshopify.com?.evil"] {
            assert_eq!(valid_shop(bad), None, "{bad}");
        }
        assert_eq!(shop_from_input("my-store").as_deref(), Some("my-store.myshopify.com"));
        assert_eq!(shop_from_input("https://admin.shopify.com/store/my-store/settings").as_deref(), Some("my-store.myshopify.com"));
        assert_eq!(shop_from_input("my-store.myshopify.com/admin").as_deref(), Some("my-store.myshopify.com"));
        assert_eq!(shop_from_input("www.mystore.com"), None);
    }

    #[test]
    fn hints() {
        assert_eq!(hint("shpat_123456abcd"), "…abcd");
    }
}

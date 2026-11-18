//! The fence every write from Shopify data goes through.
//!
//! A [`Fence`] is a short transaction, held while one item (a webhook, a customer, an order, a page
//! of products) is written, that
//! - holds a `FOR KEY SHARE` lock on the store's connection row, for the install (generation) the
//!   work was queued for: a disconnect or reinstall (which lock that row `FOR UPDATE`) waits for it,
//!   and work for an earlier install gets no fence at all, so an Admin API answer that was in flight
//!   across a reinstall can't write into the new one;
//! - holds shared erasure locks for the store and the customer and order being written, and checks
//!   their erasure markers under them. A privacy request takes the same locks exclusively before
//!   writing its markers ([`erasing`]), so either it waits for this write and then erases it, or
//!   this write waits, sees the marker and writes nothing.
//!
//! The item's own writes run on other connections while the fence is open; non-key updates of the
//! store row (last order time, lease) don't conflict with the share lock.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub struct Fence {
    tx: Transaction<'static, Postgres>,
}

impl Fence {
    /// The write is done: let a waiting disconnect, reinstall or privacy request go ahead.
    pub async fn done(self) -> sqlx::Result<()> {
        self.tx.commit().await
    }
}

fn key(account_id: Uuid, shop: &str, kind: &str, id: i64) -> String {
    format!("shopify-erase:{account_id}:{shop}:{kind}:{id}")
}

/// Open the fence for writing about this customer and/or order of this install. `None`: the
/// install is no longer the connected one (or the account was closed), or what would be written
/// was erased by a privacy request; write nothing.
pub async fn open(db: &PgPool, account_id: Uuid, shop: &str, generation: i32, customer_id: Option<i64>, order_id: Option<i64>) -> sqlx::Result<Option<Fence>> {
    let mut tx = db.begin().await?;
    let current: Option<i32> = sqlx::query_scalar(
        "SELECT generation FROM shopify_shops WHERE account_id = $1 AND shop_domain = $2 AND status = 'connected' FOR KEY SHARE",
    )
    .bind(account_id)
    .bind(shop)
    .fetch_optional(&mut *tx)
    .await?;
    if current != Some(generation) {
        return Ok(None);
    }
    // Always in the same order (store, customer, order), as `erasing` takes them.
    let mut keys = vec![key(account_id, shop, "shop", 0)];
    if let Some(c) = customer_id {
        keys.push(key(account_id, shop, "customer", c));
    }
    if let Some(o) = order_id {
        keys.push(key(account_id, shop, "order", o));
    }
    for k in &keys {
        sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))").bind(k).execute(&mut *tx).await?;
    }
    let erased: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM shopify_erasures WHERE account_id = $1 AND shop_domain = $2 AND (
            (kind = 'shop' AND shopify_id >= $3) OR (kind = 'customer' AND shopify_id = $4) OR (kind = 'order' AND shopify_id = $5)))",
    )
    .bind(account_id)
    .bind(shop)
    .bind(i64::from(generation))
    .bind(customer_id)
    .bind(order_id)
    .fetch_one(&mut *tx)
    .await?;
    if erased {
        return Ok(None);
    }
    Ok(Some(Fence { tx }))
}

/// For a privacy request: take the erasure locks for the store, or for a customer and orders,
/// exclusively, in a transaction the caller writes its markers in and commits. Writes already
/// inside a fence finish first; later ones see the markers.
pub async fn erasing(db: &PgPool, account_id: Uuid, shop: &str, whole_shop: bool, customer_id: Option<i64>, orders: &[i64]) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.begin().await?;
    let mut keys = Vec::new();
    if whole_shop {
        keys.push(key(account_id, shop, "shop", 0));
    }
    if let Some(c) = customer_id {
        keys.push(key(account_id, shop, "customer", c));
    }
    let mut orders = orders.to_vec();
    orders.sort_unstable();
    orders.dedup();
    keys.extend(orders.iter().map(|o| key(account_id, shop, "order", *o)));
    for k in &keys {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(k).execute(&mut *tx).await?;
    }
    Ok(tx)
}

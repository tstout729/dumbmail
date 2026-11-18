//! Products from the store's Admin API into the catalog (`catalog::store::upsert_products`, so they
//! count as verified and restocks and price drops start alerts), kept current by webhooks.
//!
//! Product, inventory and customer webhooks only put the item's id in `shopify_refresh` (one row
//! per item, so a bulk edit of the same product many times is one read). The worker reads them
//! back in small batches.

use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::client::{self, ApiError};
use super::customers::{amount, gid, num_id};
use super::shop::ShopRow;
use super::customers::parse_time;
use crate::features::catalog::store::{self as catalog, Origin, ProductInput, VariantInput};
use crate::state::AppState;

/// Products read per `nodes` query (keeps the query under Shopify's 1,000-point cost limit).
pub const PER_QUERY: usize = 4;

pub const PRODUCT_FIELDS: &str = "id updatedAt title handle descriptionHtml vendor productType tags status onlineStoreUrl publishedAt";
pub const VARIANT_FIELDS: &str = "id title sku price compareAtPrice inventoryQuantity availableForSale inventoryItem { id tracked } selectedOptions { name value }";

pub async fn queue_refresh<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid, kind: &str, id: i64) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO shopify_refresh (account_id, kind, shopify_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(account_id)
        .bind(kind)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// A product with its children, from a `nodes` read or assembled from bulk result lines.
#[derive(Debug, Default, Clone)]
pub struct Node {
    pub product: Value,
    pub variants: Vec<Value>,
    pub collections: Vec<String>,
    pub images: Vec<String>,
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Shopify's product as catalog input.
pub fn to_input(n: &Node, currency: Option<&str>, store_url: Option<&str>) -> Option<ProductInput> {
    let p = &n.product;
    let id = num_id(&p["id"])?;
    let status = p["status"].as_str().unwrap_or("ACTIVE");
    let published = status.eq_ignore_ascii_case("ACTIVE") && !p["publishedAt"].is_null();
    let url = text(&p["onlineStoreUrl"]).or_else(|| {
        let handle = text(&p["handle"])?;
        Some(format!("{}/products/{handle}", store_url?.trim_end_matches('/')))
    });
    let mut categories = n.collections.clone();
    if let Some(t) = text(&p["productType"]) {
        if !categories.contains(&t) {
            categories.push(t);
        }
    }
    let variants: Vec<VariantInput> = n
        .variants
        .iter()
        .filter_map(|v| {
            let vid = num_id(&v["id"])?;
            let tracked = v["inventoryItem"]["tracked"].as_bool().unwrap_or(true);
            let options: Map<String, Value> = v["selectedOptions"]
                .as_array()
                .map(|a| a.iter().filter_map(|o| Some((o["name"].as_str()?.to_string(), json!(o["value"].as_str()?)))).collect())
                .unwrap_or_default();
            Some(VariantInput {
                external_id: vid.to_string(),
                title: text(&v["title"]).filter(|t| t != "Default Title"),
                options: (!options.is_empty()).then_some(options),
                sku: text(&v["sku"]),
                price: amount(&v["price"]),
                compare_at_price: Some(amount(&v["compareAtPrice"]).unwrap_or(0.0)),
                available: v["availableForSale"].as_bool(),
                inventory: if tracked { v["inventoryQuantity"].as_i64() } else { None },
                image_url: None,
            })
        })
        .collect();
    let tracked_total: Option<i64> = {
        let tracked: Vec<i64> = n.variants.iter().filter(|v| v["inventoryItem"]["tracked"].as_bool().unwrap_or(true)).filter_map(|v| v["inventoryQuantity"].as_i64()).collect();
        (!tracked.is_empty()).then(|| tracked.iter().map(|q| (*q).max(0)).sum())
    };
    let mut custom = Map::new();
    custom.insert("shopify_id".into(), json!(id.to_string()));
    if let Some(h) = text(&p["handle"]) {
        custom.insert("handle".into(), json!(h));
    }
    Some(ProductInput {
        external_id: id.to_string(),
        title: text(&p["title"]),
        description: text(&p["descriptionHtml"]),
        url,
        image_url: n.images.first().cloned(),
        images: Some(n.images.iter().take(20).cloned().collect()),
        price: None,
        compare_at_price: None,
        currency: currency.map(String::from),
        available: Some(variants.iter().any(|v| v.available != Some(false))),
        inventory: tracked_total,
        categories: Some(categories),
        collections: None,
        tags: Some(p["tags"].as_array().map(|t| t.iter().filter_map(text).collect()).unwrap_or_default()),
        brand: text(&p["vendor"]),
        vendor: text(&p["vendor"]),
        custom: Some(custom),
        published: Some(published),
        published_at: p["publishedAt"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&chrono::Utc)),
        variants: Some(variants),
    })
}

/// Remember which product each inventory item belongs to (inventory webhooks name only the item).
pub async fn remember_inventory_items(db: &PgPool, account_id: Uuid, nodes: &[Node]) -> sqlx::Result<()> {
    let (mut items, mut products) = (Vec::new(), Vec::new());
    for n in nodes {
        let Some(pid) = num_id(&n.product["id"]) else { continue };
        for v in &n.variants {
            if let Some(iid) = num_id(&v["inventoryItem"]["id"]) {
                items.push(iid);
                products.push(pid);
            }
        }
    }
    if items.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO shopify_inventory_items (account_id, inventory_item_id, product_id)
         SELECT $1, i, p FROM unnest($2::bigint[], $3::bigint[]) AS x(i, p)
         ON CONFLICT (account_id, inventory_item_id) DO UPDATE SET product_id = EXCLUDED.product_id",
    )
    .bind(account_id)
    .bind(&items)
    .bind(&products)
    .execute(db)
    .await?;
    Ok(())
}

/// How a batch of products is being saved.
#[derive(Debug, Clone, Copy, Default)]
pub struct SaveMode {
    /// Queue no back-in-stock or price-drop alerts (a history import, or the store's first load).
    pub quiet: bool,
    /// A full product scan (bulk snapshot taken when it started): a product Shopify updated or
    /// deleted after its snapshot is left as the webhook made it. `Some(scan id)` records it as seen.
    pub scan: Option<Uuid>,
}

/// Save products into the catalog, recording which store each came from. Returns how many were saved.
pub async fn save(db: &PgPool, shop: &ShopRow, nodes: &[Node], mode: SaveMode) -> anyhow::Result<u64> {
    // Inside the fence: not into a newer install, and nothing for a store a privacy request erased.
    let Some(fence) = super::fence::open(db, shop.account_id, &shop.shop_domain, shop.generation, None, None).await? else { return Ok(0) };
    let n = save_inner(db, shop, nodes, mode).await?;
    fence.done().await?;
    Ok(n)
}

async fn save_inner(db: &PgPool, shop: &ShopRow, nodes: &[Node], mode: SaveMode) -> anyhow::Result<u64> {
    let quiet = mode.quiet || shop.catalog_loaded_at.is_none();
    let mut keep: Vec<&Node> = Vec::new();
    for n in nodes {
        let Some(pid) = num_id(&n.product["id"]) else { continue };
        let at = parse_time(n.product["updatedAt"].as_str());
        if mode.scan.is_some() {
            // Newer than this snapshot (a webhook read it since, or it was deleted since): skip.
            let newer: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM shopify_products WHERE account_id = $1 AND product_id = $2 AND shop_domain = $3
                   AND ((shopify_updated_at IS NOT NULL AND $4::timestamptz IS NOT NULL AND shopify_updated_at > $4) OR (deleted_at IS NOT NULL AND ($4::timestamptz IS NULL OR deleted_at > $4))))",
            )
            .bind(shop.account_id)
            .bind(pid)
            .bind(&shop.shop_domain)
            .bind(at)
            .fetch_one(db)
            .await?;
            if newer {
                sqlx::query("UPDATE shopify_products SET seen_by = $3 WHERE account_id = $1 AND product_id = $2").bind(shop.account_id).bind(pid).bind(mode.scan).execute(db).await?;
                continue;
            }
        }
        sqlx::query(
            "INSERT INTO shopify_products (account_id, product_id, shop_domain, shopify_updated_at, seen_by, deleted_at, touched_at, generation) VALUES ($1, $2, $3, $4, $5, NULL, now(), (SELECT generation FROM shopify_shops WHERE account_id = $1))
             ON CONFLICT (account_id, product_id) DO UPDATE SET shop_domain = EXCLUDED.shop_domain, generation = EXCLUDED.generation, shopify_updated_at = COALESCE(EXCLUDED.shopify_updated_at, shopify_products.shopify_updated_at),
                seen_by = COALESCE(EXCLUDED.seen_by, shopify_products.seen_by), deleted_at = NULL, touched_at = now()",
        )
        .bind(shop.account_id)
        .bind(pid)
        .bind(&shop.shop_domain)
        .bind(at)
        .bind(mode.scan)
        .execute(db)
        .await?;
        keep.push(n);
    }
    let owned: Vec<Node> = keep.into_iter().cloned().collect();
    let inputs: Vec<ProductInput> = owned.iter().filter_map(|n| to_input(n, shop.currency.as_deref(), shop.primary_url.as_deref())).collect();
    remember_inventory_items(db, shop.account_id, &owned).await?;
    let mut saved = 0;
    for chunk in inputs.chunks(200) {
        let r = catalog::upsert_products_with(db, shop.account_id, chunk, Origin::Shopify, quiet).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        saved += r.created + r.updated + r.unchanged;
        for e in r.errors.iter().take(3) {
            tracing::warn!(account = %shop.account_id, error = %e, "a Shopify product couldn't be saved");
        }
    }
    Ok(saved)
}

/// A full scan of the store's products finished: products of this store it didn't see, and that
/// nothing touched since the scan started, are gone from Shopify. Unpublish them. Returns how many.
pub async fn finish_scan(db: &PgPool, shop: &ShopRow, scan: Uuid, started: chrono::DateTime<chrono::Utc>) -> sqlx::Result<u64> {
    let gone: Vec<i64> = sqlx::query_scalar(
        "UPDATE shopify_products SET deleted_at = now() WHERE account_id = $1 AND shop_domain = $2 AND deleted_at IS NULL
           AND seen_by IS DISTINCT FROM $3 AND touched_at < $4 RETURNING product_id",
    )
    .bind(shop.account_id)
    .bind(&shop.shop_domain)
    .bind(scan)
    .bind(started)
    .fetch_all(db)
    .await?;
    for id in &gone {
        unpublish(db, shop.account_id, *id).await?;
    }
    sqlx::query("UPDATE shopify_shops SET catalog_loaded_at = COALESCE(catalog_loaded_at, now()) WHERE account_id = $1 AND shop_domain = $2").bind(shop.account_id).bind(&shop.shop_domain).execute(db).await?;
    Ok(gone.len() as u64)
}

fn nodes_of(v: &Value) -> Vec<Value> {
    v["nodes"].as_array().cloned().unwrap_or_default()
}

/// Read products again by id and save them. Products Shopify no longer has are unpublished.
pub async fn refresh(state: &AppState, shop: &ShopRow, ids: &[i64]) -> Result<(), ApiError> {
    let gids: Vec<String> = ids.iter().map(|i| gid("Product", *i)).collect();
    let q = format!(
        "query($ids: [ID!]!) {{ nodes(ids: $ids) {{ ... on Product {{ {PRODUCT_FIELDS}
            variants(first: 100) {{ nodes {{ {VARIANT_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }}
            collections(first: 20) {{ nodes {{ title }} }}
            media(first: 10) {{ nodes {{ ... on MediaImage {{ image {{ url }} }} }} }} }} }} }}"
    );
    let data = client::graphql(state, shop, &q, json!({"ids": gids})).await?;
    let mut found = Vec::new();
    let mut nodes = Vec::new();
    for p in data["nodes"].as_array().into_iter().flatten() {
        let Some(pid) = num_id(&p["id"]) else { continue };
        found.push(pid);
        let mut variants = nodes_of(&p["variants"]);
        let mut page = p["variants"]["pageInfo"].clone();
        // Products with more than 100 variants: read the rest (Shopify allows up to 2,048).
        let mut pages = 0;
        while page["hasNextPage"].as_bool() == Some(true) && pages < 20 {
            pages += 1;
            let more = client::graphql(
                state,
                shop,
                &format!("query($id: ID!, $after: String) {{ product(id: $id) {{ variants(first: 100, after: $after) {{ nodes {{ {VARIANT_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"),
                json!({"id": gid("Product", pid), "after": page["endCursor"]}),
            )
            .await?;
            variants.extend(nodes_of(&more["product"]["variants"]));
            page = more["product"]["variants"]["pageInfo"].clone();
        }
        nodes.push(Node {
            product: p.clone(),
            variants,
            collections: nodes_of(&p["collections"]).iter().filter_map(|c| text(&c["title"])).collect(),
            images: nodes_of(&p["media"]).iter().filter_map(|m| text(&m["image"]["url"])).collect(),
        });
    }
    save(&state.db, shop, &nodes, SaveMode::default()).await.map_err(|_| ApiError::Network("database"))?;
    for id in ids.iter().filter(|i| !found.contains(i)) {
        mark_deleted(&state.db, shop, *id).await.map_err(|_| ApiError::Network("database"))?;
    }
    Ok(())
}

/// Shopify deleted the product (a webhook, or a read that no longer finds it): remembered, so an
/// older snapshot never brings it back, and unpublished.
pub async fn mark_deleted(db: &PgPool, shop: &ShopRow, product_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO shopify_products (account_id, product_id, shop_domain, deleted_at, generation) VALUES ($1, $2, $3, now(), (SELECT generation FROM shopify_shops WHERE account_id = $1))
         ON CONFLICT (account_id, product_id) DO UPDATE SET deleted_at = now(), touched_at = now(), generation = EXCLUDED.generation",
    )
    .bind(shop.account_id)
    .bind(product_id)
    .bind(&shop.shop_domain)
    .execute(db)
    .await?;
    unpublish(db, shop.account_id, product_id).await
}

/// The store deleted the product: it stops showing in emails (its history stays).
pub async fn unpublish(db: &PgPool, account_id: Uuid, product_id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE catalog_products SET published = false, source_published = false, updated_at = now() WHERE account_id = $1 AND external_id = $2 AND source = 'shopify'")
        .bind(account_id)
        .bind(product_id.to_string())
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM shopify_inventory_items WHERE account_id = $1 AND product_id = $2").bind(account_id).bind(product_id).execute(db).await?;
    Ok(())
}

/// An inventory level changed: read its product again (if we know it).
pub async fn inventory_changed(db: &PgPool, account_id: Uuid, inventory_item_id: i64) -> sqlx::Result<()> {
    let pid: Option<i64> = sqlx::query_scalar("SELECT product_id FROM shopify_inventory_items WHERE account_id = $1 AND inventory_item_id = $2")
        .bind(account_id)
        .bind(inventory_item_id)
        .fetch_optional(db)
        .await?;
    if let Some(pid) = pid {
        queue_refresh(db, account_id, "product", pid).await?;
    }
    Ok(())
}

/// Product page and image for line items, from the catalog (order webhooks don't carry them).
pub async fn links(db: &PgPool, account_id: Uuid, product_ids: &[String]) -> sqlx::Result<std::collections::HashMap<String, (Option<String>, Option<String>, Vec<String>)>> {
    let rows: Vec<(String, Option<String>, Option<String>, Vec<String>)> =
        sqlx::query_as("SELECT external_id, url, image_url, categories FROM catalog_products WHERE account_id = $1 AND external_id = ANY($2)")
            .bind(account_id)
            .bind(product_ids)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(id, url, img, cats)| (id, (url, img, cats))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_from_graphql() {
        let n = Node {
            product: json!({"id": "gid://shopify/Product/42", "title": "Mug", "handle": "mug", "vendor": "Acme", "productType": "Kitchen", "tags": ["gift"], "status": "ACTIVE", "onlineStoreUrl": null, "publishedAt": "2026-01-01T00:00:00Z", "descriptionHtml": "<p>Nice</p>"}),
            variants: vec![
                json!({"id": "gid://shopify/ProductVariant/1", "title": "Default Title", "price": "12.50", "compareAtPrice": "15.00", "inventoryQuantity": 3, "availableForSale": true, "inventoryItem": {"id": "gid://shopify/InventoryItem/9", "tracked": true}}),
                json!({"id": "gid://shopify/ProductVariant/2", "title": "Big", "price": "14.00", "compareAtPrice": null, "inventoryQuantity": -2, "availableForSale": false, "inventoryItem": {"id": "gid://shopify/InventoryItem/10", "tracked": true}}),
            ],
            collections: vec!["Summer".into()],
            images: vec!["https://cdn.shopify.com/a.jpg".into()],
        };
        let p = to_input(&n, Some("USD"), Some("https://shop.example.com/")).unwrap();
        assert_eq!(p.external_id, "42");
        assert_eq!(p.url.as_deref(), Some("https://shop.example.com/products/mug"));
        assert_eq!(p.categories.as_deref(), Some(&["Summer".to_string(), "Kitchen".to_string()][..]));
        assert_eq!(p.inventory, Some(3));
        assert_eq!(p.published, Some(true));
        let v = p.variants.unwrap();
        assert_eq!(v[0].title, None);
        assert_eq!(v[0].price, Some(12.5));
        assert_eq!(v[1].compare_at_price, Some(0.0));
        assert_eq!(v[1].available, Some(false));
        let mut draft = n.clone();
        draft.product["status"] = json!("DRAFT");
        assert_eq!(to_input(&draft, None, None).unwrap().published, Some(false));
    }
}

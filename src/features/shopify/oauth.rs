//! Connecting a store: the authorization-code grant, signed callbacks, and what we set up in the
//! store once it's connected (webhooks, the web pixel, the theme embed's settings).
//!
//! An install always starts from a signed-in DumbMail person (dashboard or MCP): they get a link with
//! a single-use `state` tied to their account, and the callback only finishes in a browser signed in
//! to that same account. So nobody can connect their store to someone else's account (or someone
//! else's store to theirs) by sending a link.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Redirect;
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::client::{self, ApiError};
use super::shop::{self, valid_shop};
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// What the app asks for. `write_*` includes `read_*`.
/// - customers: profiles and consent, and writing unsubscribes back
/// - orders (+ all orders, for history older than 60 days): order, refund, fulfilment and checkout events
/// - products, inventory: the catalog and stock
/// - discounts: per-person codes
/// - pixels, customer events: the checkout web pixel
pub const SCOPES: &str = "write_customers,read_orders,read_all_orders,read_products,read_inventory,write_discounts,write_pixels,read_customer_events";
/// Scopes we can't work without: a grant missing one of these is refused.
const REQUIRED: &[&str] = &["write_customers", "read_orders", "read_products"];

/// An install link is good for this long.
const STATE_MINUTES: i64 = 15;

/// Topics this app subscribes each store to. The three privacy topics are set in the app's
/// configuration instead (connectors/shopify/shopify.app.toml), as Shopify requires.
pub const TOPICS: &[&str] = &[
    "ORDERS_CREATE", "ORDERS_PAID", "ORDERS_UPDATED", "ORDERS_CANCELLED", "ORDERS_FULFILLED", "ORDERS_PARTIALLY_FULFILLED",
    "REFUNDS_CREATE", "CHECKOUTS_CREATE", "CHECKOUTS_UPDATE",
    "CUSTOMERS_CREATE", "CUSTOMERS_UPDATE", "CUSTOMERS_DELETE", "CUSTOMERS_EMAIL_MARKETING_CONSENT_UPDATE", "CUSTOMERS_MARKETING_CONSENT_UPDATE",
    "PRODUCTS_CREATE", "PRODUCTS_UPDATE", "PRODUCTS_DELETE", "INVENTORY_LEVELS_UPDATE", "APP_UNINSTALLED", "BULK_OPERATIONS_FINISH",
];

pub fn redirect_uri(state: &AppState) -> String {
    format!("{}/api/shopify/callback", state.cfg.public_url())
}

pub fn webhook_uri(state: &AppState) -> String {
    format!("{}/webhooks/shopify", state.cfg.public_url())
}

/// Verify Shopify's `hmac` over a query string: every other parameter, sorted, `k=v` joined by `&`,
/// HMAC-SHA256 with the app secret, hex, compared in constant time.
pub fn query_hmac_ok(secret: &str, params: &BTreeMap<String, String>) -> bool {
    let Some(given) = params.get("hmac") else { return false };
    let Ok(given) = hex::decode(given) else { return false };
    let msg = params.iter().filter(|(k, _)| k.as_str() != "hmac" && k.as_str() != "signature").map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    mac.verify_slice(&given).is_ok()
}

/// The `timestamp` Shopify signed is recent (stops a captured link being replayed later).
fn fresh(params: &BTreeMap<String, String>) -> bool {
    params.get("timestamp").and_then(|t| t.parse::<i64>().ok()).is_some_and(|t| (Utc::now().timestamp() - t).abs() < 3600)
}

/// Start an install for the signed-in account: returns the link to Shopify's grant screen.
pub async fn start(state: &AppState, ctx: &Ctx, shop_input: &str) -> AppResult<Value> {
    let s = &state.cfg.shopify;
    let key = s.key().ok_or_else(|| AppError::Conflict("Shopify isn't available on this server (SHOPIFY_API_KEY and SHOPIFY_API_SECRET aren't set)".into()))?;
    if s.secret().is_none() {
        return Err(AppError::Conflict("Shopify isn't available on this server (SHOPIFY_API_KEY and SHOPIFY_API_SECRET aren't set)".into()));
    }
    let domain = shop::shop_from_input(shop_input).ok_or_else(|| AppError::bad("enter your store's myshopify.com address, like your-store.myshopify.com"))?;
    if let Some(cur) = shop::load(&state.db, ctx.account_id).await? {
        if cur.status == "connected" && cur.shop_domain != domain {
            return Err(AppError::Conflict(format!("{} is already connected. One store per DumbMail account: disconnect it first.", cur.shop_domain)));
        }
    }
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_shops WHERE shop_domain = $1 AND status = 'connected' AND account_id <> $2)")
        .bind(&domain)
        .bind(ctx.account_id)
        .fetch_one(&state.db)
        .await?;
    if taken {
        return Err(AppError::Conflict("that store is already connected to another DumbMail account".into()));
    }
    let st = auth::random_token(24);
    sqlx::query("DELETE FROM shopify_oauth_states WHERE created_at < now() - make_interval(mins => $1)").bind(STATE_MINUTES as i32).execute(&state.db).await?;
    sqlx::query("INSERT INTO shopify_oauth_states (state_hash, account_id, user_id, shop_domain) VALUES ($1, $2, $3, $4)")
        .bind(auth::sha256_hex(&st))
        .bind(ctx.account_id)
        .bind(ctx.user_id)
        .bind(&domain)
        .execute(&state.db)
        .await?;
    let mut url = url::Url::parse(&format!("https://{domain}/admin/oauth/authorize")).map_err(|e| AppError::Internal(e.into()))?;
    url.query_pairs_mut()
        .append_pair("client_id", key)
        .append_pair("scope", SCOPES)
        .append_pair("redirect_uri", &redirect_uri(state))
        .append_pair("state", &st);
    Ok(json!({
        "url": url.to_string(),
        "shop": domain,
        "expires_in_minutes": STATE_MINUTES,
        "note": "Open this link in a browser signed in to DumbMail, then approve the app in Shopify."
    }))
}

fn settings_page(state: &AppState, query: &str) -> Redirect {
    Redirect::to(&format!("{}/settings/shopify{query}", state.cfg.public_url()))
}

/// The app's URL in Shopify (opening the app from the store's admin, or right after an App Store
/// install): signed by Shopify. Sends the person to Settings → Shopify, where a signed-in person
/// connects the store.
pub async fn app_entry(State(state): State<AppState>, Query(q): Query<BTreeMap<String, String>>) -> Redirect {
    let Some(secret) = state.cfg.shopify.secret() else { return settings_page(&state, "?error=unavailable") };
    if !query_hmac_ok(secret, &q) || !fresh(&q) {
        return settings_page(&state, "?error=signature");
    }
    let Some(domain) = q.get("shop").and_then(|s| valid_shop(s)) else { return settings_page(&state, "?error=shop") };
    let connected: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_shops WHERE shop_domain = $1 AND status = 'connected')")
        .bind(&domain)
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);
    if connected {
        settings_page(&state, "")
    } else {
        settings_page(&state, &format!("?shop={}", url::form_urlencoded::byte_serialize(domain.as_bytes()).collect::<String>()))
    }
}

/// Shopify's redirect after the merchant approves. Errors go back to the settings page as a short
/// code it explains, never as free text.
pub async fn callback(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<BTreeMap<String, String>>) -> Redirect {
    match finish(&state, &headers, &q).await {
        Ok(()) => settings_page(&state, "?connected=1"),
        Err(code) => settings_page(&state, &format!("?error={code}")),
    }
}

async fn finish(state: &AppState, headers: &HeaderMap, q: &BTreeMap<String, String>) -> Result<(), &'static str> {
    let secret = state.cfg.shopify.secret().ok_or("unavailable")?;
    if !query_hmac_ok(secret, q) {
        return Err("signature");
    }
    if !fresh(q) {
        return Err("expired");
    }
    let domain = q.get("shop").and_then(|s| valid_shop(s)).ok_or("shop")?;
    let st = q.get("state").ok_or("expired")?;
    let code = q.get("code").filter(|c| !c.is_empty() && c.len() < 512).ok_or("expired")?;
    // Single use: the state is gone whatever happens next.
    let row: Option<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "DELETE FROM shopify_oauth_states WHERE state_hash = $1 AND created_at > now() - make_interval(mins => $2) RETURNING account_id, user_id, shop_domain",
    )
    .bind(auth::sha256_hex(st))
    .bind(STATE_MINUTES as i32)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| "failed")?;
    let (account_id, user_id, want_domain) = row.ok_or("expired")?;
    if want_domain != domain {
        return Err("shop");
    }
    // The browser finishing the install must be signed in to the account that started it.
    let session = session_account(state, headers).await.ok_or("signin")?;
    if session != account_id {
        return Err("signin");
    }
    let tokens = client::token_request(state, &domain, &[("code", code), ("expiring", "1")]).await.map_err(|e| match e {
        ApiError::Unauthorized => "denied",
        _ => "shopify",
    })?;
    let granted: Vec<&str> = tokens.scope.split(',').map(str::trim).collect();
    let has = |s: &str| granted.contains(&s) || s.strip_prefix("read_").is_some_and(|r| granted.contains(&format!("write_{r}").as_str()));
    if REQUIRED.iter().any(|s| !has(s)) {
        return Err("scopes");
    }
    save_connection(state, account_id, user_id, &domain, &tokens).await?;
    // Webhooks, shop details, the pixel and the theme embed's settings: in the background, so a slow
    // store doesn't hold the browser.
    let mut tx = state.db.begin().await.map_err(|_| "failed")?;
    crate::jobs::enqueue(&mut *tx, "shopify_setup", json!({"account_id": account_id}), None).await.map_err(|_| "failed")?;
    tx.commit().await.map_err(|_| "failed")?;
    Ok(())
}

async fn session_account(state: &AppState, headers: &HeaderMap) -> Option<Uuid> {
    let jar = axum_extra::extract::CookieJar::from_headers(headers);
    let token = jar.get(auth::SESSION_COOKIE)?.value().to_string();
    sqlx::query_scalar("SELECT account_id FROM sessions WHERE token_hash = $1 AND expires_at > now()")
        .bind(auth::sha256_hex(&token))
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

async fn save_connection(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, domain: &str, t: &client::Tokens) -> Result<(), &'static str> {
    let mut tx = state.db.begin().await.map_err(|_| "failed")?;
    let before: Option<String> = sqlx::query_scalar("SELECT shop_domain FROM shopify_shops WHERE account_id = $1 FOR UPDATE")
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| "failed")?;
    // A store connected elsewhere stays there (the unique index also guarantees it).
    let res = sqlx::query(
        "INSERT INTO shopify_shops (account_id, shop_domain, status, scopes, connected_by, connected_at)
         VALUES ($1, $2, 'connected', $3, $4, now())
         ON CONFLICT (account_id) DO UPDATE SET shop_domain = EXCLUDED.shop_domain, status = 'connected', scopes = EXCLUDED.scopes,
            connected_by = EXCLUDED.connected_by, connected_at = CASE WHEN shopify_shops.status = 'connected' THEN shopify_shops.connected_at ELSE now() END,
            disconnected_at = NULL, last_error = NULL, storefront_error = NULL, updated_at = now()
         WHERE shopify_shops.status <> 'connected' OR shopify_shops.shop_domain = EXCLUDED.shop_domain",
    )
    .bind(account_id)
    .bind(domain)
    .bind(&t.scope)
    .bind(user_id)
    .execute(&mut *tx)
    .await;
    match res {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return Err("one_store"),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => return Err("taken"),
        Err(_) => return Err("failed"),
    }
    // A different store than before: its ids mean nothing here, so forget the old store's links.
    if before.as_deref().is_some_and(|b| b != domain) {
        for table in ["shopify_customers", "shopify_inventory_items", "shopify_refresh"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE account_id = $1")).bind(account_id).execute(&mut *tx).await.map_err(|_| "failed")?;
        }
    }
    client::save_tokens(&mut *tx, &state.cfg.secret_key, account_id, t).await.map_err(|_| "failed")?;
    tx.commit().await.map_err(|_| "failed")?;
    Ok(())
}

/// Job: after connecting, read the shop's details and subscribe to its webhooks. Safe to run again.
pub async fn setup(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    let Some(s) = shop::load(&state.db, account_id).await? else { return Ok(()) };
    if s.status != "connected" {
        return Ok(());
    }
    let r = setup_inner(state, &s).await;
    if let Err(e) = &r {
        shop::note_error(&state.db, account_id, Some(&format!("Setting up the store didn't finish: {e}. DumbMail will try again."))).await?;
        if e.retry_after().is_some() {
            anyhow::bail!("shopify setup: {e}");
        }
    }
    Ok(())
}

async fn setup_inner(state: &AppState, s: &shop::ShopRow) -> Result<(), ApiError> {
    let data = client::graphql(
        state,
        s,
        "query { shop { name currencyCode ianaTimezone primaryDomain { url } } currentAppInstallation { id } webhookSubscriptions(first: 100) { nodes { id topic uri } } }",
        json!({}),
    )
    .await?;
    let shop_info = &data["shop"];
    let installation = data["currentAppInstallation"]["id"].as_str().map(String::from);
    let uri = webhook_uri(state);
    let existing: Vec<(String, String)> = data["webhookSubscriptions"]["nodes"]
        .as_array()
        .map(|a| a.iter().filter(|n| n["uri"].as_str() == Some(uri.as_str())).filter_map(|n| Some((n["topic"].as_str()?.to_string(), n["id"].as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let mut ids: Vec<String> = existing.iter().map(|(_, id)| id.clone()).collect();
    for topic in TOPICS {
        if existing.iter().any(|(t, _)| t == topic) {
            continue;
        }
        let d = client::graphql(
            state,
            s,
            "mutation($topic: WebhookSubscriptionTopic!, $sub: WebhookSubscriptionInput!) { webhookSubscriptionCreate(topic: $topic, webhookSubscription: $sub) { webhookSubscription { id } userErrors { field message } } }",
            json!({"topic": topic, "sub": {"uri": uri}}),
        )
        .await?;
        let p = &d["webhookSubscriptionCreate"];
        match p["webhookSubscription"]["id"].as_str() {
            Some(id) => ids.push(id.to_string()),
            None => tracing::warn!(account = %s.account_id, topic, "Shopify refused a webhook subscription"),
        }
    }
    sqlx::query(
        "UPDATE shopify_shops SET shop_name = $2, currency = $3, shop_timezone = $4, primary_url = $5, webhook_ids = $6, last_error = NULL, updated_at = now() WHERE account_id = $1",
    )
    .bind(s.account_id)
    .bind(shop_info["name"].as_str())
    .bind(shop_info["currencyCode"].as_str())
    .bind(shop_info["ianaTimezone"].as_str())
    .bind(shop_info["primaryDomain"]["url"].as_str())
    .bind(&ids)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::Network("database"))?;
    storefront(state, s, installation.as_deref()).await;
    Ok(())
}

/// Put DumbMail on the storefront without pasted code: the app's web pixel (checkout events) and the
/// script address the theme app embed reads. Both need the app's extensions deployed with the
/// Shopify CLI (connectors/shopify); without them Shopify refuses and the pasted snippet and custom
/// pixel stay the way to do it. Problems are saved for the settings page, never fatal.
async fn storefront(state: &AppState, s: &shop::ShopRow, installation: Option<&str>) {
    let pk: Option<String> = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(s.account_id).fetch_optional(&state.db).await.ok().flatten();
    let Some(pk) = pk else { return };
    let base = state.cfg.public_url().to_string();
    let mut problems = Vec::new();
    if let Some(owner) = installation {
        let r = client::graphql(
            state,
            s,
            "mutation($m: [MetafieldsSetInput!]!) { metafieldsSet(metafields: $m) { metafields { id } userErrors { field message code } } }",
            json!({"m": [{"ownerId": owner, "namespace": "dumbmail", "key": "script_url", "type": "single_line_text_field", "value": format!("{base}/onsite.js?k={pk}")}]}),
        )
        .await;
        match r {
            Ok(d) if client::user_error(&d["metafieldsSet"]).is_none() => {}
            _ => problems.push("the theme embed's settings couldn't be saved"),
        }
    }
    let settings = json!({"base": base, "key": pk}).to_string();
    let existing = s.web_pixel_id.clone();
    let r = match &existing {
        Some(id) => client::graphql(state, s, "mutation($id: ID!, $p: WebPixelInput!) { webPixelUpdate(id: $id, webPixel: $p) { webPixel { id } userErrors { field message code } } }", json!({"id": id, "p": {"settings": settings}})).await.map(|d| d["webPixelUpdate"].clone()),
        None => client::graphql(state, s, "mutation($p: WebPixelInput!) { webPixelCreate(webPixel: $p) { webPixel { id } userErrors { field message code } } }", json!({"p": {"settings": settings}})).await.map(|d| d["webPixelCreate"].clone()),
    };
    let pixel_id = match r {
        Ok(p) if client::user_error(&p).is_none() => p["webPixel"]["id"].as_str().map(String::from).or(existing),
        _ => {
            problems.push("the checkout pixel couldn't be installed (add it by hand from Settings → Website)");
            existing
        }
    };
    let err = (!problems.is_empty()).then(|| {
        let mut m = problems.join("; ");
        if let Some(f) = m.get_mut(0..1) {
            f.make_ascii_uppercase();
        }
        format!("{m}.")
    });
    let _ = sqlx::query("UPDATE shopify_shops SET web_pixel_id = $2, storefront_error = $3, updated_at = now() WHERE account_id = $1")
        .bind(s.account_id)
        .bind(pixel_id)
        .bind(err)
        .execute(&state.db)
        .await;
}

/// Disconnect: remove this install's webhooks and pixel from the store (best effort, one request),
/// then forget the token. A running import stops.
pub async fn disconnect(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = shop::connected(&state.db, account_id).await?;
    let mut removed = true;
    let ids: Vec<&String> = s.webhook_ids.iter().take(100).collect();
    let mut decl: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut vars = serde_json::Map::new();
    for (i, id) in ids.iter().enumerate() {
        decl.push(format!("$w{i}: ID!"));
        parts.push(format!("w{i}: webhookSubscriptionDelete(id: $w{i}) {{ deletedWebhookSubscriptionId }}"));
        vars.insert(format!("w{i}"), json!(id));
    }
    if let Some(px) = &s.web_pixel_id {
        decl.push("$px: ID!".into());
        parts.push("px: webPixelDelete(id: $px) { deletedWebPixelId }".into());
        vars.insert("px".into(), json!(px));
    }
    if !parts.is_empty() {
        let q = format!("mutation({}) {{ {} }}", decl.join(", "), parts.join(" "));
        if let Err(e) = client::graphql(state, &s, &q, Value::Object(vars)).await {
            tracing::warn!(account = %account_id, error = %e, "couldn't remove Shopify webhooks on disconnect");
            removed = false;
        }
    }
    forget(&state.db, account_id, "disconnected").await?;
    Ok(json!({
        "ok": true,
        "webhooks_removed": removed,
        "note": if removed { "Disconnected. DumbMail no longer receives anything from the store. Profiles, events and products already brought in stay." } else { "Disconnected, but Shopify didn't confirm the webhooks were removed; uninstall the app in Shopify to stop them. Anything that still arrives is ignored." }
    }))
}

/// Forget the store's tokens and stop its work (disconnect, or uninstall from Shopify's side).
pub async fn forget(db: &sqlx::PgPool, account_id: Uuid, status: &str) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE shopify_shops SET status = $2, access_token_enc = NULL, refresh_token_enc = NULL, token_expires_at = NULL, refresh_expires_at = NULL,
            webhook_ids = '{}', web_pixel_id = NULL, disconnected_at = now(), work_queued_at = NULL, updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .bind(status)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE shopify_imports SET status = 'cancelled', finished_at = now(), result_url_enc = NULL, updated_at = now() WHERE account_id = $1 AND status IN ('running', 'paused')")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM shopify_refresh WHERE account_id = $1").bind(account_id).execute(&mut *tx).await?;
    // Unprocessed deliveries: privacy requests still get done; the rest is dropped with its data.
    sqlx::query("DELETE FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic NOT IN ('customers/data_request', 'customers/redact', 'shop/redact')")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut m: BTreeMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let msg = m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(msg.as_bytes());
        m.insert("hmac".into(), hex::encode(mac.finalize().into_bytes()));
        m
    }

    #[test]
    fn query_signatures() {
        // Shopify's documented example.
        let m: BTreeMap<String, String> = [
            ("code", "0907a61c0c8d55e99db179b68161bc00"),
            ("hmac", "700e2dadb827fcc8609e9d5ce208b2e9cdaab9df07390d2cbca10d7c328fc4bf"),
            ("shop", "some-shop.myshopify.com"),
            ("state", "0.6784241404160823"),
            ("timestamp", "1337178173"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert!(query_hmac_ok("hush", &m));
        let mut bad = m.clone();
        bad.insert("shop".into(), "evil.myshopify.com".into());
        assert!(!query_hmac_ok("hush", &bad));
        assert!(!query_hmac_ok("other", &m));
        let ours = sign("s3cret", &[("shop", "a.myshopify.com"), ("timestamp", "1")]);
        assert!(query_hmac_ok("s3cret", &ours));
        let mut no_hmac = ours.clone();
        no_hmac.remove("hmac");
        assert!(!query_hmac_ok("s3cret", &no_hmac));
    }
}

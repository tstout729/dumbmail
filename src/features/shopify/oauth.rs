//! Connecting a store: the authorization-code grant, signed callbacks, and what we set up in the
//! store once it's connected (webhooks, the web pixel, the theme embed's settings).
//!
//! An install always starts from a signed-in DumbMail person (dashboard or MCP). They get a DumbMail
//! link (`/api/shopify/install`) with a single-use `state`; opening it in their browser, signed in as
//! the same person, sets a short-lived signed cookie tied to that state and goes on to Shopify. The
//! callback finishes only in that browser (the cookie), signed in as that person, while they may
//! still manage integrations. So nobody can connect their store to someone else's account, someone
//! else's store to theirs, or finish a teammate's install by forwarding a link.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Redirect;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
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
/// The browser cookie that ties the callback to the person who opened the install link.
const NONCE_COOKIE: &str = "dm_shopify_install";
const NONCE_PATH: &str = "/api/shopify";

/// Topics this app subscribes each store to. The three privacy topics are set in the app's
/// configuration instead (connectors/shopify/shopify.app.toml), as Shopify requires.
pub const TOPICS: &[&str] = &[
    "ORDERS_CREATE", "ORDERS_PAID", "ORDERS_UPDATED", "ORDERS_CANCELLED", "ORDERS_FULFILLED", "ORDERS_PARTIALLY_FULFILLED",
    "REFUNDS_CREATE", "CHECKOUTS_CREATE", "CHECKOUTS_UPDATE",
    "CUSTOMERS_CREATE", "CUSTOMERS_UPDATE", "CUSTOMERS_DELETE", "CUSTOMERS_EMAIL_MARKETING_CONSENT_UPDATE", "CUSTOMERS_MARKETING_CONSENT_UPDATE",
    "PRODUCTS_CREATE", "PRODUCTS_UPDATE", "PRODUCTS_DELETE", "INVENTORY_LEVELS_UPDATE", "APP_UNINSTALLED", "BULK_OPERATIONS_FINISH",
];

/// `ORDERS_CREATE` → `orders/create`, as people see topics.
pub fn topic_name(t: &str) -> String {
    let lower = t.to_ascii_lowercase();
    for (prefix, name) in [("customers_email_marketing_consent_", "customers_email_marketing_consent/"), ("customers_marketing_consent_", "customers_marketing_consent/"), ("inventory_levels_", "inventory_levels/"), ("bulk_operations_", "bulk_operations/"), ("app_", "app/"), ("orders_", "orders/"), ("refunds_", "refunds/"), ("checkouts_", "checkouts/"), ("customers_", "customers/"), ("products_", "products/")] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return format!("{name}{rest}");
        }
    }
    lower
}

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

/// May this person connect or disconnect the store? Today: an owner or admin of the account.
// TODO(wiring round): replace with the team layer's integrations permission (a `Perm`) once #13's
// access rules are merged into this branch.
async fn may_manage(db: &sqlx::PgPool, account_id: Uuid, user_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE account_id = $1 AND user_id = $2 AND role IN ('owner', 'admin'))")
        .bind(account_id)
        .bind(user_id)
        .fetch_one(db)
        .await
}

/// Start an install for the signed-in account: returns the DumbMail link to open, which goes on to
/// Shopify's grant screen.
pub async fn start(state: &AppState, ctx: &Ctx, shop_input: &str) -> AppResult<Value> {
    let s = &state.cfg.shopify;
    if !s.available() {
        return Err(AppError::Conflict("Shopify isn't available on this server (SHOPIFY_API_KEY and SHOPIFY_API_SECRET aren't set)".into()));
    }
    let domain = shop::shop_from_input(shop_input).ok_or_else(|| AppError::bad("enter your store's myshopify.com address, like your-store.myshopify.com"))?;
    if let Some(user) = ctx.user_id {
        if !may_manage(&state.db, ctx.account_id, user).await? {
            return Err(AppError::Forbidden("only an owner or admin of this account can connect a store".into()));
        }
    }
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
    let mut url = url::Url::parse(&format!("{}/api/shopify/install", state.cfg.public_url())).map_err(|e| AppError::Internal(e.into()))?;
    url.query_pairs_mut().append_pair("state", &st);
    Ok(json!({
        "url": url.to_string(),
        "shop": domain,
        "expires_in_minutes": STATE_MINUTES,
        "note": "Open this link in a browser signed in to DumbMail as the person connecting the store (an owner or admin), then approve the app in Shopify."
    }))
}

fn settings_page(state: &AppState, query: &str) -> Redirect {
    Redirect::to(&format!("{}/settings/shopify{query}", state.cfg.public_url()))
}

/// The signed-in person in this browser: (account, user).
async fn session(state: &AppState, headers: &HeaderMap) -> Option<(Uuid, Uuid)> {
    let jar = CookieJar::from_headers(headers);
    let token = jar.get(auth::SESSION_COOKIE)?.value().to_string();
    sqlx::query_as("SELECT account_id, user_id FROM sessions WHERE token_hash = $1 AND expires_at > now()")
        .bind(auth::sha256_hex(&token))
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

#[derive(serde::Deserialize)]
pub struct InstallQuery {
    state: Option<String>,
}

/// GET /api/shopify/install?state=…: the link `start` returns. In the browser of the person who
/// started it (or, for an install started with an API key, the signed-in owner or admin who opens
/// it first), set the install cookie and go on to Shopify.
pub async fn install(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar, Query(q): Query<InstallQuery>) -> (CookieJar, Redirect) {
    match install_inner(&state, &headers, q.state.as_deref()).await {
        Ok((cookie, url)) => (jar.add(cookie), Redirect::to(&url)),
        Err(code) => (jar, settings_page(&state, &format!("?error={code}"))),
    }
}

async fn install_inner(state: &AppState, headers: &HeaderMap, st: Option<&str>) -> Result<(Cookie<'static>, String), &'static str> {
    let key = state.cfg.shopify.key().ok_or("unavailable")?;
    let st = st.filter(|s| !s.is_empty() && s.len() < 200).ok_or("expired")?;
    let (account_id, user_id) = session(state, headers).await.ok_or("signin")?;
    let hash = auth::sha256_hex(st);
    // Bind the attempt to this person (if an API key started it), or require it's theirs.
    let row: Option<String> = sqlx::query_scalar(
        "UPDATE shopify_oauth_states SET user_id = COALESCE(user_id, $3)
         WHERE state_hash = $1 AND account_id = $2 AND (user_id IS NULL OR user_id = $3) AND created_at > now() - make_interval(mins => $4)
         RETURNING shop_domain",
    )
    .bind(&hash)
    .bind(account_id)
    .bind(user_id)
    .bind(STATE_MINUTES as i32)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| "failed")?;
    let domain = row.ok_or("signin")?;
    if !may_manage(&state.db, account_id, user_id).await.map_err(|_| "failed")? {
        return Err("role");
    }
    let cookie = Cookie::build((NONCE_COOKIE, auth::sign(&state.cfg.secret_key, &format!("shopify-install:{hash}"))))
        .path(NONCE_PATH)
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::minutes(STATE_MINUTES))
        .build();
    let mut url = url::Url::parse(&format!("https://{domain}/admin/oauth/authorize")).map_err(|_| "shop")?;
    url.query_pairs_mut()
        .append_pair("client_id", key)
        .append_pair("scope", SCOPES)
        .append_pair("redirect_uri", &redirect_uri(state))
        .append_pair("state", st);
    Ok((cookie, url.to_string()))
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
/// code it explains, never as free text. The install cookie is cleared either way.
pub async fn callback(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar, Query(q): Query<BTreeMap<String, String>>) -> (CookieJar, Redirect) {
    let nonce = jar.get(NONCE_COOKIE).map(|c| c.value().to_string());
    let jar = jar.remove(Cookie::build(NONCE_COOKIE).path(NONCE_PATH).build());
    match finish(&state, &headers, nonce.as_deref(), &q).await {
        Ok(()) => (jar, settings_page(&state, "?connected=1")),
        Err(code) => (jar, settings_page(&state, &format!("?error={code}"))),
    }
}

async fn finish(state: &AppState, headers: &HeaderMap, nonce: Option<&str>, q: &BTreeMap<String, String>) -> Result<(), &'static str> {
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
    let hash = auth::sha256_hex(st);
    // This browser opened the install link for this very attempt.
    let cookie_ok = nonce.and_then(|n| auth::verify(&state.cfg.secret_key, n)).is_some_and(|p| p == format!("shopify-install:{hash}"));
    // Single use: the state is gone whatever happens next.
    let row: Option<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "DELETE FROM shopify_oauth_states WHERE state_hash = $1 AND created_at > now() - make_interval(mins => $2) RETURNING account_id, user_id, shop_domain",
    )
    .bind(&hash)
    .bind(STATE_MINUTES as i32)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| "failed")?;
    let (account_id, user_id, want_domain) = row.ok_or("expired")?;
    if want_domain != domain {
        return Err("shop");
    }
    if !cookie_ok {
        return Err("browser");
    }
    // The browser finishing the install must be signed in as the person who started it…
    let (session_account, session_user) = session(state, headers).await.ok_or("signin")?;
    if session_account != account_id || user_id != Some(session_user) {
        return Err("signin");
    }
    // …who may still manage integrations.
    if !may_manage(&state.db, account_id, session_user).await.map_err(|_| "failed")? {
        return Err("role");
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
    save_connection(state, account_id, Some(session_user), &domain, &tokens).await
}

/// Save the connection as a new install (generation), and queue its setup in the same transaction.
async fn save_connection(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, domain: &str, t: &client::Tokens) -> Result<(), &'static str> {
    let mut tx = state.db.begin().await.map_err(|_| "failed")?;
    let before: Option<String> = sqlx::query_scalar("SELECT shop_domain FROM shopify_shops WHERE account_id = $1 FOR UPDATE")
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| "failed")?;
    let generation: i32 = sqlx::query_scalar("SELECT COALESCE(max(generation), 0) + 1 FROM shopify_installs WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| "failed")?;
    // A store connected elsewhere stays there (the unique index also guarantees it). The lease is
    // cleared: a worker still running for the previous install can't write any more.
    let res = sqlx::query(
        "INSERT INTO shopify_shops (account_id, shop_domain, status, scopes, connected_by, connected_at, generation)
         VALUES ($1, $2, 'connected', $3, $4, now(), $5)
         ON CONFLICT (account_id) DO UPDATE SET shop_domain = EXCLUDED.shop_domain, status = 'connected', scopes = EXCLUDED.scopes,
            connected_by = EXCLUDED.connected_by, connected_at = now(), generation = EXCLUDED.generation,
            lease_until = NULL, lease_token = NULL, work_queued_at = NULL, missing_topics = '{}', setup_after = NULL, cleanup_error = NULL,
            catalog_loaded_at = CASE WHEN shopify_shops.shop_domain = EXCLUDED.shop_domain THEN shopify_shops.catalog_loaded_at END,
            webhook_ids = CASE WHEN shopify_shops.shop_domain = EXCLUDED.shop_domain THEN shopify_shops.webhook_ids ELSE '{}' END,
            web_pixel_id = CASE WHEN shopify_shops.shop_domain = EXCLUDED.shop_domain THEN shopify_shops.web_pixel_id END,
            disconnected_at = NULL, last_error = NULL, storefront_error = NULL, updated_at = now()
         WHERE shopify_shops.status <> 'connected' OR shopify_shops.shop_domain = EXCLUDED.shop_domain",
    )
    .bind(account_id)
    .bind(domain)
    .bind(&t.scope)
    .bind(user_id)
    .bind(generation)
    .execute(&mut *tx)
    .await;
    match res {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return Err("one_store"),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => return Err("taken"),
        Err(_) => return Err("failed"),
    }
    sqlx::query("INSERT INTO shopify_installs (account_id, generation, shop_domain) VALUES ($1, $2, $3)")
        .bind(account_id)
        .bind(generation)
        .bind(domain)
        .execute(&mut *tx)
        .await
        .map_err(|_| "failed")?;
    // Work queued for the previous install goes: deliveries (privacy requests stay), items to read,
    // imports.
    sqlx::query("DELETE FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL AND topic NOT IN ('customers/data_request', 'customers/redact', 'shop/redact')")
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| "failed")?;
    sqlx::query("DELETE FROM shopify_refresh WHERE account_id = $1").bind(account_id).execute(&mut *tx).await.map_err(|_| "failed")?;
    sqlx::query("UPDATE shopify_imports SET status = 'cancelled', finished_at = now(), result_url_enc = NULL, updated_at = now() WHERE account_id = $1 AND status IN ('running', 'paused')")
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| "failed")?;
    // A different store than before: its inventory ids mean nothing here. Customer links stay (they
    // name their store) so privacy requests for the old store still find its data.
    if before.as_deref().is_some_and(|b| b != domain) {
        sqlx::query("DELETE FROM shopify_inventory_items WHERE account_id = $1").bind(account_id).execute(&mut *tx).await.map_err(|_| "failed")?;
    }
    client::save_tokens(&mut *tx, &state.cfg.secret_key, account_id, t).await.map_err(|_| "failed")?;
    // Webhooks, shop details, the pixel and the theme embed's settings: in the background, so a slow
    // store doesn't hold the browser. Queued with the connection, so neither exists without the other.
    crate::jobs::enqueue(&mut *tx, "shopify_setup", json!({"account_id": account_id, "generation": generation}), None).await.map_err(|_| "failed")?;
    tx.commit().await.map_err(|_| "failed")?;
    Ok(())
}

/// Job: after connecting, read the shop's details and subscribe to its webhooks, then start the
/// history import (once per install). Safe to run again; a job for an earlier install does nothing.
/// If Shopify refuses any webhook subscription, the store shows as connected but not receiving
/// those topics, and setup tries again.
pub async fn setup(state: &AppState, account_id: Uuid, generation: Option<i32>) -> anyhow::Result<()> {
    let Some(s) = shop::load(&state.db, account_id).await? else { return Ok(()) };
    if s.status != "connected" || generation.is_some_and(|g| g != s.generation) {
        return Ok(());
    }
    let r = setup_inner(state, &s).await;
    match &r {
        Err(e) => {
            sqlx::query("UPDATE shopify_shops SET last_error = $3, last_error_at = now(), setup_after = now() + interval '10 minutes' WHERE account_id = $1 AND generation = $2")
                .bind(account_id)
                .bind(s.generation)
                .bind(format!("Setting up the store didn't finish: {e}. DumbMail will try again."))
                .execute(&state.db)
                .await?;
            if e.retry_after().is_some() {
                anyhow::bail!("shopify setup: {e}");
            }
        }
        Ok(missing) if !missing.is_empty() => {
            // Retried by the scheduler (setup_after) as well as by the job queue.
            anyhow::bail!("shopify setup: {} webhook subscriptions refused", missing.len());
        }
        Ok(_) => start_history(state, &s).await?,
    }
    Ok(())
}

/// Connecting starts the history import, once per install. A product reconcile already running is
/// cancelled: the import reads every product anyway.
async fn start_history(state: &AppState, s: &shop::ShopRow) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let started: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_imports WHERE account_id = $1 AND kind = 'history' AND generation = $2)")
        .bind(s.account_id)
        .bind(s.generation)
        .fetch_one(&mut *tx)
        .await?;
    if started {
        return Ok(());
    }
    sqlx::query("UPDATE shopify_imports SET status = 'cancelled', finished_at = now(), result_url_enc = NULL, updated_at = now() WHERE account_id = $1 AND kind = 'products' AND status IN ('running', 'paused')")
        .bind(s.account_id)
        .execute(&mut *tx)
        .await?;
    let r = sqlx::query(
        "INSERT INTO shopify_imports (id, account_id, kind, status, phases, phase, orders_since, counts, generation, shop_domain)
         SELECT $1, $2, 'history', 'running', ARRAY['customers', 'products', 'orders'], 'customers', now() - make_interval(days => $3), '{}', $4, $5
         WHERE EXISTS (SELECT 1 FROM shopify_shops WHERE account_id = $2 AND generation = $4 AND status = 'connected')
         ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(s.account_id)
    .bind((super::import::DEFAULT_ORDER_YEARS * 365) as i32)
    .bind(s.generation)
    .bind(&s.shop_domain)
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() == 1 {
        shop::kick(&mut tx, s.account_id).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Returns the topics Shopify refused (empty when every subscription exists).
async fn setup_inner(state: &AppState, s: &shop::ShopRow) -> Result<Vec<String>, ApiError> {
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
    let mut missing = Vec::new();
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
        match p["webhookSubscription"]["id"].as_str().filter(|_| client::user_error(p).is_none()) {
            Some(id) => ids.push(id.to_string()),
            None => {
                tracing::warn!(account = %s.account_id, topic, "Shopify refused a webhook subscription");
                missing.push(topic_name(topic));
            }
        }
    }
    let note = (!missing.is_empty()).then(|| format!("Connected, but Shopify refused to send DumbMail {}. DumbMail will try again.", missing.join(", ")));
    sqlx::query(
        "UPDATE shopify_shops SET shop_name = $3, currency = $4, shop_timezone = $5, primary_url = $6, webhook_ids = $7, missing_topics = $8,
            setup_after = CASE WHEN cardinality($8::text[]) > 0 THEN now() + interval '10 minutes' END,
            last_error = $9, last_error_at = CASE WHEN $9::text IS NULL THEN last_error_at ELSE now() END, updated_at = now()
         WHERE account_id = $1 AND generation = $2",
    )
    .bind(s.account_id)
    .bind(s.generation)
    .bind(shop_info["name"].as_str())
    .bind(shop_info["currencyCode"].as_str())
    .bind(shop_info["ianaTimezone"].as_str())
    .bind(shop_info["primaryDomain"]["url"].as_str())
    .bind(&ids)
    .bind(&missing)
    .bind(note)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::Network("database"))?;
    storefront(state, s, installation.as_deref()).await;
    Ok(missing)
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
    let _ = sqlx::query("UPDATE shopify_shops SET web_pixel_id = $3, storefront_error = $4, updated_at = now() WHERE account_id = $1 AND generation = $2")
        .bind(s.account_id)
        .bind(s.generation)
        .bind(pixel_id)
        .bind(err)
        .execute(&state.db)
        .await;
}

/// Disconnect: remove this install's webhooks, pixel and the theme embed's settings from the store
/// (one request; every part's `userErrors` checked), then forget the token. A running import stops.
/// Whatever Shopify didn't confirm is reported (and kept in status).
pub async fn disconnect(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = shop::connected(&state.db, account_id).await?;
    let ids: Vec<&String> = s.webhook_ids.iter().take(100).collect();
    let mut decl: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut vars = serde_json::Map::new();
    for (i, id) in ids.iter().enumerate() {
        decl.push(format!("$w{i}: ID!"));
        parts.push(format!("w{i}: webhookSubscriptionDelete(id: $w{i}) {{ deletedWebhookSubscriptionId userErrors {{ field message }} }}"));
        vars.insert(format!("w{i}"), json!(id));
    }
    if let Some(px) = &s.web_pixel_id {
        decl.push("$px: ID!".into());
        parts.push("px: webPixelDelete(id: $px) { deletedWebPixelId userErrors { field message code } }".into());
        vars.insert("px".into(), json!(px));
    }
    let mut unfinished: Vec<&str> = Vec::new();
    // The theme embed's settings live on the app installation.
    match client::graphql(state, &s, "query { currentAppInstallation { id } }", json!({})).await {
        Ok(d) => match d["currentAppInstallation"]["id"].as_str() {
            Some(owner) => {
                decl.push("$mf: [MetafieldIdentifierInput!]!".into());
                parts.push("mf: metafieldsDelete(metafields: $mf) { deletedMetafields { key namespace ownerId } userErrors { field message } }".into());
                vars.insert("mf".into(), json!([{"ownerId": owner, "namespace": "dumbmail", "key": "script_url"}]));
            }
            None => unfinished.push("the theme embed's settings"),
        },
        Err(_) => unfinished.push("the theme embed's settings"),
    }
    if !parts.is_empty() {
        let q = format!("mutation({}) {{ {} }}", decl.join(", "), parts.join(" "));
        match client::graphql(state, &s, &q, Value::Object(vars)).await {
            Ok(d) => {
                let failed = |k: &str| d[k].is_null() || client::user_error(&d[k]).is_some();
                if (0..ids.len()).any(|i| failed(&format!("w{i}"))) {
                    unfinished.push("webhooks");
                }
                if s.web_pixel_id.is_some() && failed("px") {
                    unfinished.push("the checkout pixel");
                }
                if parts.iter().any(|p| p.starts_with("mf:")) && failed("mf") {
                    unfinished.push("the theme embed's settings");
                }
            }
            Err(e) => {
                tracing::warn!(account = %account_id, error = %e, "couldn't clean up the Shopify store on disconnect");
                unfinished.extend(["webhooks", "the checkout pixel", "the theme embed's settings"]);
            }
        }
    }
    unfinished.dedup();
    forget(&state.db, account_id, "disconnected", Some(s.generation)).await?;
    let cleanup = (!unfinished.is_empty()).then(|| format!("Shopify didn't confirm removing {}. Uninstall the DumbMail app in Shopify to remove them; anything that still arrives is ignored.", unfinished.join(", ")));
    sqlx::query("UPDATE shopify_shops SET cleanup_error = $2 WHERE account_id = $1").bind(account_id).bind(&cleanup).execute(&state.db).await?;
    Ok(json!({
        "ok": true,
        "cleaned_up": unfinished.is_empty(),
        "not_removed": unfinished,
        "note": cleanup.unwrap_or_else(|| "Disconnected. DumbMail's webhooks, pixel and theme embed settings are removed from the store. Profiles, events and products already brought in stay.".into())
    }))
}

/// Forget the store's tokens and stop its work (disconnect, or uninstall from Shopify's side). With
/// `generation`, only that install (a delayed uninstall never disconnects a newer install). Returns
/// whether anything changed.
pub async fn forget(db: &sqlx::PgPool, account_id: Uuid, status: &str, generation: Option<i32>) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let changed: Option<i32> = sqlx::query_scalar(
        "UPDATE shopify_shops SET status = $2, access_token_enc = NULL, refresh_token_enc = NULL, token_expires_at = NULL, refresh_expires_at = NULL,
            webhook_ids = '{}', web_pixel_id = NULL, disconnected_at = now(), work_queued_at = NULL, lease_until = NULL, lease_token = NULL, updated_at = now()
         WHERE account_id = $1 AND status = 'connected' AND ($3::int IS NULL OR generation = $3) RETURNING generation",
    )
    .bind(account_id)
    .bind(status)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(generation_done) = changed else {
        tx.commit().await?;
        return Ok(false);
    };
    sqlx::query("UPDATE shopify_installs SET uninstalled_at = COALESCE(uninstalled_at, now()) WHERE account_id = $1 AND generation = $2").bind(account_id).bind(generation_done).execute(&mut *tx).await?;
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
    tx.commit().await?;
    Ok(true)
}

/// Stores whose setup should be tried again (a refused webhook subscription, or a failed setup).
pub async fn setup_due(db: &sqlx::PgPool) -> sqlx::Result<()> {
    let due: Vec<(Uuid, i32)> = sqlx::query_as(
        "UPDATE shopify_shops SET setup_after = now() + interval '10 minutes' WHERE status = 'connected' AND setup_after < now() RETURNING account_id, generation",
    )
    .fetch_all(db)
    .await?;
    for (account_id, generation) in due {
        crate::jobs::enqueue(db, "shopify_setup", json!({"account_id": account_id, "generation": generation}), None).await?;
    }
    Ok(())
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

    #[test]
    fn topic_names() {
        assert_eq!(topic_name("ORDERS_CREATE"), "orders/create");
        assert_eq!(topic_name("CUSTOMERS_EMAIL_MARKETING_CONSENT_UPDATE"), "customers_email_marketing_consent/update");
        assert_eq!(topic_name("APP_UNINSTALLED"), "app/uninstalled");
        assert_eq!(topic_name("ORDERS_PARTIALLY_FULFILLED"), "orders/partially_fulfilled");
    }
}

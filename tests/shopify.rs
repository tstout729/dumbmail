//! Shopify app, end to end against a local stand-in for Shopify's documented APIs: OAuth (with the
//! signed callback and expiring tokens), the GraphQL Admin API (including bulk operations with
//! JSONL result files served by byte range), and webhook deliveries with real HMACs.

#[macro_use]
mod common;
use common::*;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use base64::Engine;
use hmac::Mac;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const KEY: &str = "test-client-id";
const SECRET: &str = "test-app-secret";
const SHOP: &str = "acme-goods.myshopify.com";

#[derive(Default)]
struct Shopify {
    /// Current valid access token (requests with another get 401).
    token: String,
    tokens_issued: u32,
    refreshes: u32,
    expires_in: Option<i64>,
    granted_scope: String,
    customers: HashMap<i64, Value>,
    products: HashMap<i64, Value>,
    files: HashMap<String, String>,
    bulk_ops: Vec<String>,
    /// Most bytes served per file request, whatever range was asked for (forces many slices).
    max_chunk: usize,
    webhooks_created: Vec<String>,
    deletes: u32,
    consent_updates: Vec<Value>,
    discounts: HashSet<String>,
    discount_calls: u32,
    node_reads: Vec<String>,
    throttle_next: u32,
    queries: u32,
}

type Mock = Arc<Mutex<Shopify>>;

fn customer(id: i64, email: Option<&str>, state: &str, phone: Option<&str>, sms: &str) -> Value {
    json!({
        "id": format!("gid://shopify/Customer/{id}"), "firstName": "Ann", "lastName": format!("Lee{id}"), "tags": ["vip"],
        "numberOfOrders": "2", "amountSpent": {"amount": "150.00", "currencyCode": "USD"}, "createdAt": "2024-01-01T00:00:00Z",
        "defaultEmailAddress": email.map(|e| json!({"emailAddress": e, "marketingState": state, "marketingOptInLevel": "SINGLE_OPT_IN", "marketingUpdatedAt": "2025-01-01T00:00:00Z"})),
        "defaultPhoneNumber": phone.map(|p| json!({"phoneNumber": p, "smsMarketingConsent": {"state": sms, "optInLevel": "SINGLE_OPT_IN", "updatedAt": "2025-01-01T00:00:00Z"}})),
        "defaultAddress": {"city": "Portland", "province": "Oregon", "countryCodeV2": "US", "zip": "97201"}
    })
}

fn product(id: i64, title: &str, qty: i64, price: &str) -> Value {
    json!({
        "id": format!("gid://shopify/Product/{id}"), "title": title, "handle": title.to_lowercase(), "descriptionHtml": "<p>Good</p>", "vendor": "Acme",
        "productType": "Kitchen", "tags": ["gift"], "status": "ACTIVE", "onlineStoreUrl": format!("https://acme.test/products/{}", title.to_lowercase()), "publishedAt": "2025-01-01T00:00:00Z",
        "variants": {"nodes": [{"id": format!("gid://shopify/ProductVariant/{id}1"), "title": "Default Title", "sku": format!("SKU{id}"), "price": price, "compareAtPrice": null,
            "inventoryQuantity": qty, "availableForSale": qty > 0, "inventoryItem": {"id": format!("gid://shopify/InventoryItem/{id}9"), "tracked": true}, "selectedOptions": []}],
            "pageInfo": {"hasNextPage": false, "endCursor": null}},
        "collections": {"nodes": [{"title": "Summer"}]},
        "media": {"nodes": [{"image": {"url": format!("https://cdn.shopify.com/{id}.jpg")}}]}
    })
}

fn gql_ok(data: Value) -> (StatusCode, axum::Json<Value>) {
    (StatusCode::OK, axum::Json(json!({"data": data, "extensions": {"cost": {"requestedQueryCost": 10, "actualQueryCost": 5, "throttleStatus": {"maximumAvailable": 2000, "currentlyAvailable": 1990, "restoreRate": 100}}}})))
}

async fn graphql(State(m): State<Mock>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> (StatusCode, axum::Json<Value>) {
    let q = body["query"].as_str().unwrap_or("").to_string();
    let vars = body["variables"].clone();
    let mut m = m.lock().unwrap();
    if headers.get("x-shopify-access-token").and_then(|v| v.to_str().ok()) != Some(m.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"errors": "[API] Invalid API key or access token (unrecognized login or wrong password)"})));
    }
    m.queries += 1;
    if m.throttle_next > 0 {
        m.throttle_next -= 1;
        return (StatusCode::OK, axum::Json(json!({"errors": [{"message": "Throttled", "extensions": {"code": "THROTTLED"}}],
            "extensions": {"cost": {"requestedQueryCost": 300, "throttleStatus": {"maximumAvailable": 2000, "currentlyAvailable": 100, "restoreRate": 100}}}})));
    }
    if q.contains("currentAppInstallation") {
        return gql_ok(json!({"shop": {"name": "Acme Goods", "currencyCode": "USD", "ianaTimezone": "America/Los_Angeles", "primaryDomain": {"url": "https://acme.test"}},
            "currentAppInstallation": {"id": "gid://shopify/AppInstallation/1"}, "webhookSubscriptions": {"nodes": []}}));
    }
    if q.contains("webhookSubscriptionCreate") {
        let topic = vars["topic"].as_str().unwrap_or("").to_string();
        m.webhooks_created.push(topic);
        let id = format!("gid://shopify/WebhookSubscription/{}", m.webhooks_created.len());
        return gql_ok(json!({"webhookSubscriptionCreate": {"webhookSubscription": {"id": id}, "userErrors": []}}));
    }
    if q.contains("metafieldsSet") {
        return gql_ok(json!({"metafieldsSet": {"metafields": [{"id": "gid://shopify/Metafield/1"}], "userErrors": []}}));
    }
    if q.contains("webPixelCreate") {
        return gql_ok(json!({"webPixelCreate": {"webPixel": {"id": "gid://shopify/WebPixel/1"}, "userErrors": []}}));
    }
    if q.contains("webhookSubscriptionDelete") {
        m.deletes += 1;
        return gql_ok(json!({}));
    }
    if q.contains("bulkOperationRunQuery") {
        let bq = vars["q"].as_str().unwrap_or("");
        let phase = if bq.contains("customers {") { "customers" } else if bq.contains("products {") { "products" } else { "orders" };
        m.bulk_ops.push(phase.to_string());
        return gql_ok(json!({"bulkOperationRunQuery": {"bulkOperation": {"id": format!("gid://shopify/BulkOperation/{}", m.bulk_ops.len()), "status": "CREATED"}, "userErrors": []}}));
    }
    if q.contains("on BulkOperation") {
        let n: usize = vars["id"].as_str().unwrap_or("").rsplit('/').next().unwrap().parse().unwrap();
        let phase = m.bulk_ops[n - 1].clone();
        let url = m.files.get(&phase).filter(|f| !f.is_empty()).map(|_| format!("{}/files/{phase}.jsonl", m.files["__base"]));
        return gql_ok(json!({"node": {"id": vars["id"], "status": "COMPLETED", "errorCode": null, "objectCount": "3", "url": url}}));
    }
    if q.contains("nodes(ids:") {
        let ids: Vec<String> = serde_json::from_value(vars["ids"].clone()).unwrap();
        m.node_reads.extend(ids.clone());
        let nodes: Vec<Value> = ids
            .iter()
            .map(|g| {
                let id: i64 = g.rsplit('/').next().unwrap().parse().unwrap();
                if g.contains("/Customer/") { m.customers.get(&id).cloned().unwrap_or(Value::Null) } else { m.products.get(&id).cloned().unwrap_or(Value::Null) }
            })
            .collect();
        return gql_ok(json!({"nodes": nodes}));
    }
    if q.contains("customerEmailMarketingConsentUpdate") || q.contains("customerSmsMarketingConsentUpdate") {
        m.consent_updates.push(vars["input"].clone());
        let key = if q.contains("Email") { "customerEmailMarketingConsentUpdate" } else { "customerSmsMarketingConsentUpdate" };
        return gql_ok(json!({key: {"customer": {"id": vars["input"]["customerId"]}, "userErrors": []}}));
    }
    if q.contains("discountCodeBasicCreate") {
        m.discount_calls += 1;
        let code = vars["d"]["code"].as_str().unwrap_or("").to_string();
        if !m.discounts.insert(code) {
            return gql_ok(json!({"discountCodeBasicCreate": {"codeDiscountNode": null, "userErrors": [{"field": ["basicCodeDiscount", "code"], "message": "Code must be unique.", "code": "TAKEN"}]}}));
        }
        return gql_ok(json!({"discountCodeBasicCreate": {"codeDiscountNode": {"id": format!("gid://shopify/DiscountCodeNode/{}", m.discount_calls)}, "userErrors": []}}));
    }
    if q.contains("codeDiscountNodeByCode") {
        let found = m.discounts.contains(vars["c"].as_str().unwrap_or(""));
        return gql_ok(json!({"codeDiscountNodeByCode": if found { json!({"id": "gid://shopify/DiscountCodeNode/99"}) } else { Value::Null }}));
    }
    gql_ok(json!({}))
}

async fn token(State(m): State<Mock>, axum::Form(f): axum::Form<HashMap<String, String>>) -> (StatusCode, axum::Json<Value>) {
    let mut m = m.lock().unwrap();
    if f.get("client_id").map(String::as_str) != Some(KEY) || f.get("client_secret").map(String::as_str) != Some(SECRET) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid_client"})));
    }
    let ok = match f.get("grant_type").map(String::as_str) {
        Some("refresh_token") => {
            m.refreshes += 1;
            f.get("refresh_token").is_some_and(|r| r.starts_with("shprt_"))
        }
        _ => f.get("code").map(String::as_str) == Some("good-code"),
    };
    if !ok {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_request"})));
    }
    m.tokens_issued += 1;
    m.token = format!("shpat_token{}", m.tokens_issued);
    let mut v = json!({"access_token": m.token, "scope": m.granted_scope, "refresh_token": format!("shprt_{}", m.tokens_issued), "refresh_token_expires_in": 7776000});
    if let Some(e) = m.expires_in {
        v["expires_in"] = json!(e);
    }
    (StatusCode::OK, axum::Json(v))
}

async fn file(State(m): State<Mock>, Path(name): Path<String>, headers: HeaderMap) -> (StatusCode, HeaderMap, String) {
    let m = m.lock().unwrap();
    let phase = name.trim_end_matches(".jsonl");
    let content = m.files.get(phase).cloned().unwrap_or_default();
    let bytes = content.as_bytes();
    let (start, end) = headers
        .get("range")
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .map(|r| {
            let (a, b) = r.split_once('-').unwrap();
            (a.parse::<usize>().unwrap(), b.parse::<usize>().ok().unwrap_or(bytes.len().saturating_sub(1)))
        })
        .unwrap_or((0, bytes.len().saturating_sub(1)));
    let mut h = HeaderMap::new();
    if start >= bytes.len() {
        h.insert("content-range", format!("bytes */{}", bytes.len()).parse().unwrap());
        return (StatusCode::RANGE_NOT_SATISFIABLE, h, String::new());
    }
    let end = end.min(bytes.len() - 1).min(start + m.max_chunk.max(1) - 1);
    h.insert("content-range", format!("bytes {start}-{end}/{}", bytes.len()).parse().unwrap());
    (StatusCode::PARTIAL_CONTENT, h, String::from_utf8_lossy(&bytes[start..=end]).into_owned())
}

async fn mock_shopify() -> (Mock, String) {
    let mock: Mock = Arc::new(Mutex::new(Shopify { max_chunk: 1 << 20, granted_scope: "write_customers,read_orders,read_all_orders,read_products,read_inventory,write_discounts,write_pixels,read_customer_events".into(), ..Default::default() }));
    let app = axum::Router::new()
        .route("/{shop}/admin/oauth/access_token", post(token))
        .route("/{shop}/admin/api/{version}/graphql.json", post(graphql))
        .route("/files/{name}", get(file))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    mock.lock().unwrap().files.insert("__base".into(), base.clone());
    (mock, base)
}

async fn harness_with(base: &str) -> Option<Harness> {
    let base = base.to_string();
    Harness::with(move |c| {
        c.shopify.shopify_api_key = Some(KEY.into());
        c.shopify.shopify_api_secret = Some(SECRET.into());
        c.shopify.shopify_api_base = Some(base);
    })
    .await
}

macro_rules! shopify_harness {
    ($base:expr) => {
        match harness_with(&$base).await {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

fn signed_query(pairs: &[(&str, &str)]) -> String {
    let m: BTreeMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let msg = m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    let hmac = hex::encode(mac.finalize().into_bytes());
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in &m {
        q.append_pair(k, v);
    }
    q.append_pair("hmac", &hmac);
    q.finish()
}

/// GET with an explicit cookie (or none); returns status and Location.
async fn get_redirect(h: &Harness, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    let res = h.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let loc = res.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let status = res.status();
    let _ = res.into_body().collect().await;
    (status, loc)
}

/// One scheduler pass for the Shopify feature (tick 0 runs every periodic task).
async fn tick(h: &Harness) {
    use dumbmail::features::Feature;
    dumbmail::features::shopify::Module.tick(&h.state, 0).await.unwrap();
}

fn now_ts() -> String {
    chrono::Utc::now().timestamp().to_string()
}

/// Start an install as the signed-in account and finish Shopify's callback in the same browser.
async fn connect(h: &mut Harness) -> String {
    let v = h.ok("POST", "/api/shopify/connect", Some(json!({"shop": "acme-goods"}))).await;
    let url = url::Url::parse(v["url"].as_str().unwrap()).unwrap();
    assert_eq!(url.host_str(), Some(SHOP));
    let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    let q = signed_query(&[("code", "good-code"), ("shop", SHOP), ("state", &state), ("timestamp", &now_ts())]);
    let cookie = h.cookie.clone();
    let (s, loc) = get_redirect(h, &format!("/api/shopify/callback?{q}"), cookie.as_deref()).await;
    assert!(s.is_redirection(), "{s}");
    assert!(loc.ends_with("/settings/shopify?connected=1"), "{loc}");
    h.drain().await;
    state
}

fn webhook_sig(body: &str) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

async fn hook(h: &Harness, topic: &str, id: &str, body: &Value) -> StatusCode {
    let b = body.to_string();
    let (s, _, _) = h
        .raw("POST", "/webhooks/shopify", &[("content-type", "application/json"), ("x-shopify-hmac-sha256", &webhook_sig(&b)), ("x-shopify-topic", topic), ("x-shopify-shop-domain", SHOP), ("x-shopify-webhook-id", id)], &b)
        .await;
    s
}

fn order_body(id: i64, customer_id: i64, email: &str, created: chrono::DateTime<chrono::Utc>) -> Value {
    json!({
        "id": id, "admin_graphql_api_id": format!("gid://shopify/Order/{id}"), "name": format!("#{}", id - 4000), "order_number": id - 4000,
        "email": email, "created_at": created.to_rfc3339(), "processed_at": created.to_rfc3339(), "updated_at": created.to_rfc3339(), "cancelled_at": null,
        "currency": "USD", "total_price": "100.00", "subtotal_price": "90.00", "total_discounts": "10.00", "total_tax": "5.00",
        "financial_status": "paid", "fulfillment_status": null, "test": false, "order_status_url": "https://acme.test/status",
        "discount_codes": [{"code": "WELCOME10", "amount": "10.00", "type": "percentage"}],
        "customer": {"id": customer_id, "email": email, "first_name": "Ann", "last_name": "Lee"},
        "line_items": [
            {"id": 11, "product_id": 101, "variant_id": 1011, "title": "Mug", "variant_title": "Default Title", "sku": "SKU101", "vendor": "Acme", "quantity": 2, "price": "30.00", "total_discount": "0.00"},
            {"id": 12, "product_id": 102, "variant_id": 1021, "title": "Bowl", "variant_title": "Blue", "sku": "SKU102", "vendor": "Acme", "quantity": 1, "price": "40.00", "total_discount": "10.00"}
        ],
        "refunds": [], "fulfillments": []
    })
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn event(h: &Harness, metric: &str, unique: &str) -> Option<(Option<f64>, Value, Option<bool>, Uuid)> {
    sqlx::query_as("SELECT e.value, e.properties, e.from_browser, e.profile_id FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = $1 AND e.unique_id = $2")
        .bind(metric)
        .bind(unique)
        .fetch_optional(&h.state.db)
        .await
        .unwrap()
}

async fn count_events(h: &Harness, metric: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = $1").bind(metric).fetch_one(&h.state.db).await.unwrap()
}

async fn consent(h: &Harness, email: &str) -> (String, String, Value) {
    sqlx::query_as("SELECT email_consent, sms_consent, properties FROM profiles WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn not_available_without_app_credentials() {
    let mut h = harness!();
    h.signup().await;
    let v = h.ok("GET", "/api/shopify", None).await;
    assert_eq!(v["available"], false);
    assert_eq!(v["connected"], false);
    let (s, v) = h.req("POST", "/api/shopify/connect", Some(json!({"shop": "acme-goods"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("isn't available on this server"));
    // Webhooks are refused outright.
    let (s, _, _) = h.raw("POST", "/webhooks/shopify", &[("x-shopify-hmac-sha256", "x")], "{}").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn connecting_a_store() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    assert_eq!(h.ok("GET", "/api/shopify", None).await["available"], true);
    // Only real myshopify.com domains.
    for bad in ["evil.com", "acme.myshopify.com.evil.com", "https://evil.com/?x=.myshopify.com"] {
        let (s, _) = h.req("POST", "/api/shopify/connect", Some(json!({"shop": bad}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    let v = h.ok("POST", "/api/shopify/connect", Some(json!({"shop": "acme-goods"}))).await;
    let url = url::Url::parse(v["url"].as_str().unwrap()).unwrap();
    let qp: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(qp["client_id"], KEY);
    assert_eq!(qp["redirect_uri"], "https://pl.test/api/shopify/callback");
    assert!(qp["scope"].contains("write_customers") && qp["scope"].contains("read_all_orders"));
    let state = qp["state"].clone();
    let cookie = h.cookie.clone();

    // A forged callback (bad signature, another shop, stale timestamp, unknown state) gets nowhere.
    let good = |state: &str| signed_query(&[("code", "good-code"), ("shop", SHOP), ("state", state), ("timestamp", &now_ts())]);
    let tampered = good(&state).replace("good-code", "evil-code");
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{tampered}"), cookie.as_deref()).await.1.ends_with("error=signature"));
    let other_shop = signed_query(&[("code", "good-code"), ("shop", "other.myshopify.com"), ("state", &state), ("timestamp", &now_ts())]);
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{other_shop}"), cookie.as_deref()).await.1.ends_with("error=shop"));
    // That consumed the state: it's single use.
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{}", good(&state)), cookie.as_deref()).await.1.ends_with("error=expired"));
    let old = signed_query(&[("code", "good-code"), ("shop", SHOP), ("state", "x"), ("timestamp", "1000")]);
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{old}"), cookie.as_deref()).await.1.ends_with("error=expired"));

    // A link started by this account, finished in a browser that isn't signed in to it, is refused.
    let v = h.ok("POST", "/api/shopify/connect", Some(json!({"shop": SHOP}))).await;
    let state = url::Url::parse(v["url"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{}", good(&state)), None).await.1.ends_with("error=signin"));
    assert_eq!(mock.lock().unwrap().tokens_issued, 0, "no code was exchanged");

    // The real thing.
    connect(&mut h).await;
    {
        let m = mock.lock().unwrap();
        assert_eq!(m.tokens_issued, 1);
        assert_eq!(m.webhooks_created.len(), 20);
        assert!(m.webhooks_created.contains(&"ORDERS_CREATE".to_string()) && m.webhooks_created.contains(&"APP_UNINSTALLED".to_string()));
    }
    let (s, v) = h.req("GET", "/api/shopify", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["connected"], true);
    assert_eq!(v["connection"]["name"], "Acme Goods");
    assert_eq!(v["connection"]["token_hint"], "…ken1");
    assert_eq!(v["connection"]["web_pixel_installed"], true);
    assert!(v["storefront"]["embed_link"].as_str().unwrap().contains("activateAppId=test-client-id/dumbmail"));
    assert!(!v.to_string().contains("shpat_"), "the token never leaves the server: {v}");
    let enc: String = sqlx::query_scalar("SELECT access_token_enc FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert!(!enc.contains("shpat_"), "stored encrypted");

    // A second account can't take the same store.
    let mine = h.cookie.clone();
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "B"}))).await;
    let (s, v) = h.req("POST", "/api/shopify/connect", Some(json!({"shop": SHOP}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], false, "account B sees nothing of A's store");

    // The app URL from Shopify's side, signed: connected stores go to their settings page.
    let entry = signed_query(&[("shop", SHOP), ("timestamp", &now_ts()), ("host", "YWRtaW4")]);
    assert!(get_redirect(&h, &format!("/api/shopify/app?{entry}"), None).await.1.ends_with("/settings/shopify"));
    let fresh = signed_query(&[("shop", "new-store.myshopify.com"), ("timestamp", &now_ts())]);
    assert!(get_redirect(&h, &format!("/api/shopify/app?{fresh}"), None).await.1.ends_with("/settings/shopify?shop=new-store.myshopify.com"));
    assert!(get_redirect(&h, "/api/shopify/app?shop=acme-goods.myshopify.com&hmac=00", None).await.1.ends_with("error=signature"));

    // Disconnect: a dry run first, then webhooks and the pixel removed and the token forgotten.
    h.cookie = mine;
    let v = h.ok("POST", "/api/shopify/disconnect", Some(json!({}))).await;
    assert_eq!(v["dry_run"], true);
    let v = h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    assert_eq!(v["webhooks_removed"], true);
    assert_eq!(mock.lock().unwrap().deletes, 1, "one request removes every webhook");
    let (status, tok): (String, Option<String>) = sqlx::query_as("SELECT status, access_token_enc FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok), ("disconnected", None));
    h.cleanup().await;
}

#[tokio::test]
async fn webhooks_are_verified_and_deduplicated() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let body = order_body(5001, 1, "ann@example.com", chrono::Utc::now());
    let b = body.to_string();
    // Wrong signature, or one made with another secret: 401 and nothing stored.
    let (s, _, _) = h.raw("POST", "/webhooks/shopify", &[("x-shopify-hmac-sha256", "bm9wZQ=="), ("x-shopify-topic", "orders/create"), ("x-shopify-shop-domain", SHOP), ("x-shopify-webhook-id", "w1")], &b).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"another-secret").unwrap();
    mac.update(b.as_bytes());
    let wrong = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let (s, _, _) = h.raw("POST", "/webhooks/shopify", &[("x-shopify-hmac-sha256", &wrong), ("x-shopify-topic", "orders/create"), ("x-shopify-shop-domain", SHOP), ("x-shopify-webhook-id", "w1")], &b).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Signed, but the body was changed after signing.
    let (s, _, _) = h.raw("POST", "/webhooks/shopify", &[("x-shopify-hmac-sha256", &webhook_sig(&b)), ("x-shopify-topic", "orders/create"), ("x-shopify-shop-domain", SHOP), ("x-shopify-webhook-id", "w1")], &b.replace("100.00", "1.00")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_webhooks").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);

    // The same delivery three times is stored and processed once.
    for _ in 0..3 {
        assert_eq!(hook(&h, "orders/create", "w1", &body).await, StatusCode::OK);
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_webhooks").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    // A store that isn't connected anywhere: acknowledged, ignored.
    let b2 = body.to_string();
    let (s, _, _) = h.raw("POST", "/webhooks/shopify", &[("x-shopify-hmac-sha256", &webhook_sig(&b2)), ("x-shopify-topic", "orders/create"), ("x-shopify-shop-domain", "stranger.myshopify.com"), ("x-shopify-webhook-id", "w9")], &b2).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 1);
    // Processed bodies are cleared.
    let kept: Option<Value> = sqlx::query_scalar("SELECT payload FROM shopify_webhooks WHERE webhook_id = 'w1'").fetch_one(&h.state.db).await.unwrap();
    assert!(kept.is_none());
    // Product and customer webhooks keep only the id.
    hook(&h, "customers/update", "w2", &json!({"id": 77, "email": "secret@example.com", "first_name": "Zed"})).await;
    let kept: Value = sqlx::query_scalar("SELECT payload FROM shopify_webhooks WHERE webhook_id = 'w2'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(kept, json!({"id": 77}));
    let v = h.ok("GET", "/api/shopify", None).await;
    assert_eq!(v["health"]["state"], "server");
    assert_eq!(v["connection"]["last_webhook_topic"], "customers/update");
    h.cleanup().await;
}

#[tokio::test]
async fn orders_become_trusted_events_once() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let acct = account_id(&h).await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    // A live flow on Placed Order, to see which orders start it.
    h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    sqlx::query("UPDATE flows SET status = 'live'").execute(&h.state.db).await.unwrap();
    connect(&mut h).await;

    // The browser pixel reports order 5001 first (with a made-up total).
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "profile": {"email": "ann@example.com"}, "properties": {"tracked_by": "shopify_pixel", "total": 9999}, "value": 9999, "unique_id": "order:5001"}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    // Server-only metrics can't be forged from a browser.
    for metric in ["Refunded Order", "Fulfilled Order", "Cancelled Order", "Ordered Product", "Fulfilled Partial Order"] {
        let (s, _, body) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
            &json!({"metric": metric, "profile": {"email": "ann@example.com"}, "value": 5}).to_string()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{metric}: {body}");
    }
    h.drain().await;
    let flow_runs = || async { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM flow_runs").fetch_one(&h.state.db).await.unwrap() };
    assert_eq!(flow_runs().await, 1, "the browser's order started the flow, as it does today");

    // Then Shopify's webhook: the same event, now with the real amounts, trusted.
    hook(&h, "orders/create", "w1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 1, "one order, one event");
    let (value, props, from_browser, _) = event(&h, "Placed Order", "order:5001").await.unwrap();
    assert_eq!(value, Some(100.0));
    assert_eq!(from_browser, Some(false));
    assert_eq!(props["tracked_by"], "shopify");
    assert_eq!(props["order_name"], "#1001");
    assert_eq!(props["items"][0]["product_id"], "101");
    assert_eq!(props["items"][1]["variant"], "Blue");
    assert_eq!(props["discount_codes"], json!(["WELCOME10"]));
    assert_eq!(flow_runs().await, 1, "upgrading the browser's copy doesn't start the flow again");
    assert_eq!(count_events(&h, "Ordered Product").await, 2);
    assert_eq!(event(&h, "Ordered Product", "order:5001:12").await.unwrap().0, Some(30.0));
    // The order doesn't touch consent.
    assert_eq!(consent(&h, "ann@example.com").await.0, "never_subscribed");

    // The other way round: Shopify first, then the browser's copy is a duplicate.
    hook(&h, "orders/create", "w2", &order_body(5002, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "profile": {"email": "bob@example.com"}, "value": 1, "unique_id": "order:5002"}).to_string()).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 2);
    assert_eq!(event(&h, "Placed Order", "order:5002").await.unwrap().0, Some(100.0));
    assert_eq!(flow_runs().await, 2);

    // An old order seen for the first time (an edit to last week's order) is history: no flow.
    hook(&h, "orders/updated", "w3", &order_body(5003, 3, "cat@example.com", chrono::Utc::now() - chrono::Duration::days(7))).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 3);
    assert_eq!(flow_runs().await, 2, "history doesn't start flows");

    // Refunds take value off once, whichever webhook says so and however often.
    let refund = json!({"id": 900, "order_id": 5001, "created_at": chrono::Utc::now().to_rfc3339(),
        "transactions": [{"kind": "refund", "status": "success", "amount": "30.00"}], "refund_line_items": [{"line_item_id": 12, "quantity": 1, "subtotal": 30.0}]});
    hook(&h, "refunds/create", "w4", &refund).await;
    hook(&h, "refunds/create", "w5", &refund).await; // a second delivery of the same refund
    h.drain().await;
    assert_eq!(event(&h, "Placed Order", "order:5001").await.unwrap().0, Some(70.0));
    assert_eq!(event(&h, "Ordered Product", "order:5001:12").await.unwrap().0, Some(0.0));
    assert_eq!(event(&h, "Ordered Product", "order:5001:11").await.unwrap().0, Some(60.0));
    assert_eq!(event(&h, "Refunded Order", "refund:900").await.unwrap().0, Some(30.0));
    let mut updated = order_body(5001, 1, "ann@example.com", chrono::Utc::now());
    updated["refunds"] = json!([refund]);
    hook(&h, "orders/updated", "w6", &updated).await;
    // An older payload without the refund arrives late: the refund stays counted.
    hook(&h, "orders/paid", "w7", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert_eq!(event(&h, "Placed Order", "order:5001").await.unwrap().0, Some(70.0));
    assert_eq!(count_events(&h, "Refunded Order").await, 1);

    // Cancelled: revenue goes to 0 and a Cancelled Order event is recorded.
    let mut cancelled = updated.clone();
    cancelled["cancelled_at"] = json!(chrono::Utc::now().to_rfc3339());
    cancelled["cancel_reason"] = json!("customer");
    hook(&h, "orders/cancelled", "w8", &cancelled).await;
    h.drain().await;
    assert_eq!(event(&h, "Placed Order", "order:5001").await.unwrap().0, Some(0.0));
    assert_eq!(event(&h, "Cancelled Order", "order:5001").await.unwrap().0, Some(100.0));
    // Revenue as the overview and reports count it: the sum of Placed Order values.
    let revenue: f64 = sqlx::query_scalar("SELECT sum(e.value) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Placed Order'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(revenue, 200.0);

    // Shipments.
    let mut partial = order_body(5002, 1, "ann@example.com", chrono::Utc::now());
    partial["fulfillment_status"] = json!("partial");
    partial["fulfillments"] = json!([{"id": 61, "status": "success", "created_at": chrono::Utc::now().to_rfc3339()}]);
    hook(&h, "orders/partially_fulfilled", "w9", &partial).await;
    let mut full = partial.clone();
    full["fulfillment_status"] = json!("fulfilled");
    full["fulfillments"] = json!([{"id": 61, "status": "success"}, {"id": 62, "status": "success"}]);
    hook(&h, "orders/fulfilled", "w10", &full).await;
    hook(&h, "orders/fulfilled", "w11", &full).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Fulfilled Partial Order").await, 1);
    assert_eq!(count_events(&h, "Fulfilled Order").await, 1);

    // Test orders are ignored.
    let mut test = order_body(5009, 1, "ann@example.com", chrono::Utc::now());
    test["test"] = json!(true);
    hook(&h, "orders/create", "w12", &test).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5009").await.is_none());
    h.cleanup().await;
}

#[tokio::test]
async fn checkouts_match_the_pixel() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts").fetch_one(&h.state.db).await.unwrap();
    connect(&mut h).await;
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Started Checkout", "profile": {"email": "ann@example.com"}, "value": 5, "unique_id": "checkout:tok123"}).to_string()).await;
    let checkout = json!({"token": "tok123", "email": "ann@example.com", "completed_at": null, "created_at": chrono::Utc::now().to_rfc3339(), "currency": "USD", "total_price": "64.00",
        "abandoned_checkout_url": "https://acme.test/checkouts/tok123/recover", "line_items": [{"product_id": 101, "variant_id": 1011, "title": "Mug", "quantity": 2, "price": "32.00"}]});
    hook(&h, "checkouts/create", "c1", &checkout).await;
    let mut done = checkout.clone();
    done["token"] = json!("tok999");
    done["completed_at"] = json!(chrono::Utc::now().to_rfc3339());
    hook(&h, "checkouts/update", "c2", &done).await;
    let mut anonymous = checkout.clone();
    anonymous["token"] = json!("tok555");
    anonymous["email"] = Value::Null;
    hook(&h, "checkouts/create", "c3", &anonymous).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Started Checkout").await, 1);
    let (value, props, from_browser, _) = event(&h, "Started Checkout", "checkout:tok123").await.unwrap();
    assert_eq!((value, from_browser), (Some(64.0), Some(false)));
    assert_eq!(props["checkout_url"], "https://acme.test/checkouts/tok123/recover");
    assert_eq!(props["item_count"], 2);
    h.cleanup().await;
}

#[tokio::test]
async fn customers_and_consent() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    {
        let mut m = mock.lock().unwrap();
        m.customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", Some("+15035550101"), "SUBSCRIBED"));
        m.customers.insert(2, customer(2, Some("bob@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"));
        m.customers.insert(3, customer(3, Some("cat@example.com"), "UNSUBSCRIBED", None, "NOT_SUBSCRIBED"));
        m.customers.insert(4, customer(4, Some("dee@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
        m.customers.insert(5, customer(5, Some("eve@example.com"), "PENDING", Some("+15035550105"), "PENDING"));
    }
    // Dee unsubscribed from DumbMail before; Cat was subscribed here.
    let acct = account_id(&h).await;
    for (email, c) in [("dee@example.com", "unsubscribed"), ("cat@example.com", "subscribed")] {
        sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, $3, $4)").bind(Uuid::now_v7()).bind(acct).bind(email).bind(c).execute(&h.state.db).await.unwrap();
    }
    for id in 1..=5 {
        hook(&h, "customers/update", &format!("c{id}"), &json!({"id": id})).await;
    }
    h.drain().await;
    let (e, s, p) = consent(&h, "ann@example.com").await;
    assert_eq!((e.as_str(), s.as_str()), ("subscribed", "subscribed"));
    assert_eq!(p["shopify_customer_id"], "1");
    assert_eq!(p["shopify_tags"], json!(["vip"]));
    assert_eq!(p["city"], "Portland");
    assert_eq!(p["country"], "US");
    assert_eq!(p["shopify_total_spent"], 150.0);
    let (e, _, p) = consent(&h, "bob@example.com").await;
    assert_eq!(e, "never_subscribed", "not subscribed in Shopify is not subscribed here");
    assert_eq!(p["shopify_email_marketing"], "not_subscribed");
    assert_eq!(consent(&h, "cat@example.com").await.0, "unsubscribed", "Shopify's unsubscribe arrives");
    assert_eq!(consent(&h, "dee@example.com").await.0, "unsubscribed", "a DumbMail unsubscribe wins over Shopify's subscribed");
    let (e, s, _) = consent(&h, "eve@example.com").await;
    assert_eq!((e.as_str(), s.as_str()), ("never_subscribed", "never_subscribed"), "pending is not consent");
    // Five customers, read in one request.
    assert_eq!(mock.lock().unwrap().node_reads.len(), 5);

    // Ann changes her email in Shopify; the new address hasn't subscribed. Nothing carries over.
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann.new@example.com"), "NOT_SUBSCRIBED", Some("+15035550101"), "SUBSCRIBED"));
    hook(&h, "customers/update", "c6", &json!({"id": 1})).await;
    h.drain().await;
    assert_eq!(consent(&h, "ann.new@example.com").await.0, "never_subscribed");
    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed", "the old address keeps its own consent");
    let phone_on_new: Option<String> = sqlx::query_scalar("SELECT phone_number FROM profiles WHERE email = 'ann.new@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(phone_on_new, None, "the number stays with the profile that has it");

    // Unsubscribes in DumbMail are written back to Shopify: Dee unsubscribed here while Shopify
    // still has her subscribed. Ann's old address isn't Shopify's any more (customer 1 has the new
    // one), so unsubscribing it here writes nothing.
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed', sms_consent = 'unsubscribed' WHERE email = 'ann@example.com'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    {
        let m = mock.lock().unwrap();
        assert_eq!(m.consent_updates.len(), 1, "{:?}", m.consent_updates);
        assert_eq!(m.consent_updates[0]["customerId"], "gid://shopify/Customer/4");
        assert_eq!(m.consent_updates[0]["emailMarketingConsent"]["marketingState"], "UNSUBSCRIBED");
    }
    // Written once.
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().consent_updates.len(), 1);
    // Bob unsubscribes from an email here: written back on the next pass.
    sqlx::query("UPDATE shopify_customers SET email_state = 'SUBSCRIBED' WHERE customer_id = 2").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE email = 'bob@example.com'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().consent_updates.len(), 2);
    sqlx::query("UPDATE profiles SET email_consent = 'never_subscribed' WHERE email = 'bob@example.com'").execute(&h.state.db).await.unwrap();

    // Shopify deleting a customer drops the link only.
    hook(&h, "customers/delete", "c8", &json!({"id": 2})).await;
    h.drain().await;
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_customers WHERE customer_id = 2").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(links, 0);
    assert_eq!(consent(&h, "bob@example.com").await.0, "never_subscribed");
    h.cleanup().await;
}

#[tokio::test]
async fn products_stay_current() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    mock.lock().unwrap().products.insert(101, product(101, "Mug", 0, "12.00"));
    // A bulk edit touching the same product 50 times is one read.
    for i in 0..50 {
        hook(&h, "products/update", &format!("p{i}"), &json!({"id": 101, "title": "Mug"})).await;
    }
    h.drain().await;
    assert_eq!(mock.lock().unwrap().node_reads.len(), 1);
    let p = h.ok("GET", "/api/catalog/products/101", None).await;
    assert_eq!(p["source"], "shopify", "{p}");
    assert_eq!(p["verified"], true);
    assert_eq!(p["in_stock"], false);
    assert_eq!(p["url"], "https://acme.test/products/mug");
    assert_eq!(p["image_url"], "https://cdn.shopify.com/101.jpg");
    // Stock arrives: the inventory webhook names only the item; we know its product.
    mock.lock().unwrap().products.insert(101, product(101, "Mug", 5, "10.00"));
    hook(&h, "inventory_levels/update", "i1", &json!({"inventory_item_id": 1019, "location_id": 1, "available": 5})).await;
    h.drain().await;
    let p = h.ok("GET", "/api/catalog/products/101", None).await;
    assert_eq!(p["in_stock"], true);
    assert_eq!(p["price"], 10.0);
    // Deleted in Shopify: unpublished here.
    hook(&h, "products/delete", "d1", &json!({"id": 101})).await;
    h.drain().await;
    let p = h.ok("GET", "/api/catalog/products/101", None).await;
    assert_eq!(p["published"], false);
    h.cleanup().await;
}

fn jsonl(lines: &[Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

#[tokio::test]
async fn history_import() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let ago = |d: i64| (chrono::Utc::now() - chrono::Duration::days(d)).to_rfc3339();
    {
        let mut m = mock.lock().unwrap();
        m.files.insert("customers".into(), jsonl(&[
            customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"),
            customer(2, Some("bob@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"),
        ]));
        m.files.insert("products".into(), jsonl(&[
            json!({"id": "gid://shopify/Product/101", "title": "Mug", "handle": "mug", "status": "ACTIVE", "publishedAt": ago(400), "vendor": "Acme", "tags": [], "onlineStoreUrl": null}),
            json!({"id": "gid://shopify/ProductVariant/1011", "title": "Default Title", "price": "12.00", "inventoryQuantity": 4, "availableForSale": true, "inventoryItem": {"id": "gid://shopify/InventoryItem/1019", "tracked": true}, "__parentId": "gid://shopify/Product/101"}),
            json!({"id": "gid://shopify/Collection/7", "title": "Summer", "__parentId": "gid://shopify/Product/101"}),
            json!({"id": "gid://shopify/MediaImage/3", "image": {"url": "https://cdn.shopify.com/m.jpg"}, "__parentId": "gid://shopify/Product/101"}),
            json!({"__parentId": "gid://shopify/Product/101"}),
        ]));
        let order = |id: i64, cust: i64, email: &str, days: i64, cancelled: bool, refunds: Value| json!({
            "id": format!("gid://shopify/Order/{id}"), "name": format!("#{}", id - 4000), "createdAt": ago(days), "processedAt": ago(days), "updatedAt": ago(days),
            "cancelledAt": if cancelled { json!(ago(days)) } else { Value::Null }, "cancelReason": null, "test": false, "email": email, "phone": null, "currencyCode": "USD",
            "displayFinancialStatus": "PAID", "displayFulfillmentStatus": "FULFILLED", "discountCodes": [],
            "totalPriceSet": {"shopMoney": {"amount": "50.00"}}, "subtotalPriceSet": {"shopMoney": {"amount": "50.00"}}, "totalDiscountsSet": {"shopMoney": {"amount": "0.0"}},
            "totalShippingPriceSet": {"shopMoney": {"amount": "0.0"}}, "totalTaxSet": {"shopMoney": {"amount": "0.0"}},
            "customer": {"id": format!("gid://shopify/Customer/{cust}"), "firstName": "A", "lastName": "B"},
            "refunds": refunds, "fulfillments": [{"id": format!("gid://shopify/Fulfillment/{id}"), "createdAt": ago(days), "status": "SUCCESS"}]
        });
        let line = |order: i64, line: i64| json!({"id": format!("gid://shopify/LineItem/{line}"), "title": "Mug", "variantTitle": null, "sku": "SKU101", "vendor": "Acme", "quantity": 1,
            "originalUnitPriceSet": {"shopMoney": {"amount": "50.00"}}, "totalDiscountSet": {"shopMoney": {"amount": "0.00"}}, "product": {"id": "gid://shopify/Product/101"},
            "variant": {"id": "gid://shopify/ProductVariant/1011"}, "image": {"url": "https://cdn.shopify.com/m.jpg"}, "__parentId": format!("gid://shopify/Order/{order}")});
        m.files.insert("orders".into(), jsonl(&[
            order(5001, 1, "ann@example.com", 300, false, json!([{"id": "gid://shopify/Refund/9", "createdAt": ago(299), "totalRefundedSet": {"shopMoney": {"amount": "20.00"}}}])),
            line(5001, 1),
            order(5002, 2, "bob@example.com", 200, true, json!([])),
            line(5002, 2),
            order(5003, 1, "ann@example.com", 2, false, json!([])),
            line(5003, 3),
        ]));
        // Serve at most 200 bytes per request, so records span many reads.
        m.max_chunk = 200;
    }
    // A segment-triggered flow and a metric flow, both live: history must start neither.
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Buyers", "definition": {"match": "all", "conditions": [{"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1}]}}))).await;
    let seg_id = seg["id"].as_str().unwrap().to_string();
    h.ok("POST", "/api/flows", Some(json!({"name": "Welcome buyers", "trigger": {"type": "segment", "segment_id": seg_id}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    sqlx::query("UPDATE flows SET status = 'live'").execute(&h.state.db).await.unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id.parse().unwrap()).await.unwrap();

    let v = h.ok("POST", "/api/shopify/import", Some(json!({}))).await;
    assert_eq!(v["dry_run"], true);
    let v = h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    assert_eq!(v["status"], "running");
    let (s, _) = h.req("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "one import at a time");
    // A segment refresh in the middle of the import starts no one.
    for _ in 0..40 {
        h.fast_forward().await;
        dumbmail::flows::refresh_segment(&h.state, seg_id.parse().unwrap()).await.unwrap();
        if h.ok("GET", "/api/shopify/import", None).await["import"]["status"] != "running" {
            break;
        }
    }
    let v = h.ok("GET", "/api/shopify/import", None).await;
    assert_eq!(v["import"]["status"], "done", "{v}");
    assert_eq!(v["import"]["counts"]["customers"], 2);
    assert_eq!(v["import"]["counts"]["orders"], 3);
    assert_eq!(mock.lock().unwrap().bulk_ops, vec!["customers", "products", "orders"]);
    dumbmail::flows::refresh_segment(&h.state, seg_id.parse().unwrap()).await.unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(runs, 0, "imported history starts no flows, metric or segment");
    let members: i32 = sqlx::query_scalar("SELECT member_count FROM segments").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(members, 2, "but people are in the segment");
    // A later refresh (after someone new enters for real) triggers again.
    let later: bool = dumbmail::features::shopify::absorb_segment_entries(&h.state.db, account_id(&h).await, Some(chrono::Utc::now() + chrono::Duration::seconds(1))).await.unwrap();
    assert!(!later);

    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed");
    assert_eq!(consent(&h, "bob@example.com").await.0, "never_subscribed");
    assert_eq!(event(&h, "Placed Order", "order:5001").await.unwrap().0, Some(30.0), "less the refund");
    assert_eq!(event(&h, "Placed Order", "order:5002").await.unwrap().0, Some(0.0), "cancelled");
    assert_eq!(event(&h, "Refunded Order", "refund:9").await.unwrap().0, Some(20.0));
    assert_eq!(count_events(&h, "Fulfilled Order").await, 3);
    // Imported events carry their own time and don't count as fresh activity.
    let last: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT last_event_at FROM profiles WHERE email = 'bob@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(last < chrono::Utc::now() - chrono::Duration::days(199));
    let p = h.ok("GET", "/api/catalog/products/101", None).await;
    assert_eq!(p["categories"], json!(["Summer"]));
    assert_eq!(p["image_url"], "https://cdn.shopify.com/m.jpg");
    assert_eq!(p["inventory"], 4);

    // Running it again adds nothing twice.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM events").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    for _ in 0..40 {
        h.fast_forward().await;
        if h.ok("GET", "/api/shopify/import", None).await["import"]["status"] != "running" {
            break;
        }
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM events").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(before, after);
    h.cleanup().await;
}

#[tokio::test]
async fn import_pauses_resumes_and_replays_safely() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    {
        let mut m = mock.lock().unwrap();
        m.files.insert("customers".into(), jsonl(&(1..=30).map(|i| customer(i, Some(&format!("c{i}@example.com")), "SUBSCRIBED", None, "NOT_SUBSCRIBED")).collect::<Vec<_>>()));
        m.files.insert("products".into(), String::new());
        m.files.insert("orders".into(), String::new());
        m.max_chunk = 900;
    }
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    let v = h.ok("POST", "/api/shopify/import/pause", None).await;
    assert_eq!(v["status"], "paused");
    let reads = mock.lock().unwrap().queries;
    h.fast_forward().await;
    assert_eq!(mock.lock().unwrap().queries, reads, "paused: nothing runs");
    h.ok("POST", "/api/shopify/import/resume", None).await;
    for _ in 0..60 {
        h.fast_forward().await;
        if h.ok("GET", "/api/shopify/import", None).await["import"]["status"] != "running" {
            break;
        }
    }
    assert_eq!(h.ok("GET", "/api/shopify/import", None).await["import"]["status"], "done");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 30);
    // A crash that lost the saved position, so the customers are read again from the start: nothing doubles.
    sqlx::query("UPDATE shopify_imports SET status = 'running', phase = 'customers', bulk_id = NULL, result_offset = 0, finished_at = NULL").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE shopify_shops SET work_queued_at = NULL").execute(&h.state.db).await.unwrap();
    dumbmail::features::shopify::shop::kick_now(&h.state.db, account_id(&h).await).await.unwrap();
    for _ in 0..60 {
        h.fast_forward().await;
        if h.ok("GET", "/api/shopify/import", None).await["import"]["status"] != "running" {
            break;
        }
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 30);
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_customers").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(linked, 30);
    // Throttled by Shopify: the worker waits and carries on.
    mock.lock().unwrap().throttle_next = 1;
    let v = h.ok("POST", "/api/shopify/products/resync", None).await;
    assert_eq!(v["products_sync"]["status"], "running");
    for _ in 0..10 {
        h.fast_forward().await;
    }
    let v = h.ok("GET", "/api/shopify/import", None).await;
    assert_eq!(v["products_sync"]["status"], "done", "{v}");
    // Cancel.
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    let v = h.ok("POST", "/api/shopify/import/cancel", None).await;
    assert_eq!(v["status"], "cancelled");
    let (s, _) = h.req("POST", "/api/shopify/import/cancel", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.cleanup().await;
}

#[tokio::test]
async fn discount_codes() {
    use dumbmail::features::shopify::{create_discount_code, create_discount_codes, DiscountTemplate, DiscountValue};
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let acct = account_id(&h).await;
    let t = DiscountTemplate {
        key: "coupon-1".into(),
        title: "10% off".into(),
        value: DiscountValue::Percentage(10.0),
        minimum_subtotal: Some(25.0),
        expires_after_days: Some(30),
        ends_at: None,
        collection_ids: vec![],
        prefix: "HELLO".into(),
        combines_with_other_discounts: false,
    };
    let pid: Uuid = sqlx::query_scalar("INSERT INTO profiles (id, account_id, email) VALUES ($1, $2, 'ann@example.com') RETURNING id").bind(Uuid::now_v7()).bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert!(create_discount_code(&h.state, acct, &t, pid, "m1").await.is_err(), "no store connected");
    connect(&mut h).await;
    let code = create_discount_code(&h.state, acct, &t, pid, "m1").await.unwrap();
    assert!(code.starts_with("HELLO"));
    // The same grant again: the same code, no second call.
    assert_eq!(create_discount_code(&h.state, acct, &t, pid, "m1").await.unwrap(), code);
    assert_eq!(mock.lock().unwrap().discount_calls, 1);
    // Shopify made the code but we crashed before saving it: the retry finds it and keeps it.
    sqlx::query("DELETE FROM shopify_discount_codes").execute(&h.state.db).await.unwrap();
    assert_eq!(create_discount_code(&h.state, acct, &t, pid, "m1").await.unwrap(), code);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_discount_codes").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    // Another account's person can't get one.
    let stranger = Uuid::now_v7();
    assert!(create_discount_code(&h.state, acct, &t, stranger, "m2").await.is_err());
    // A batch stops at Shopify's rate limit and says which were made.
    let grants: Vec<(Uuid, String)> = (0..3).map(|i| (pid, format!("batch{i}"))).collect();
    mock.lock().unwrap().throttle_next = 0;
    let out = create_discount_codes(&h.state, acct, &t, &grants).await.unwrap();
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|(_, r)| r.is_ok()));
    mock.lock().unwrap().throttle_next = 5;
    let out = create_discount_codes(&h.state, acct, &t, &[(pid, "x1".into()), (pid, "x2".into())]).await.unwrap();
    assert_eq!(out.len(), 1, "stopped at the first throttle");
    assert!(matches!(out[0].1, Err(dumbmail::error::AppError::Busy(_))));
    assert!(create_discount_codes(&h.state, acct, &t, &vec![(pid, "y".to_string()); 26]).await.is_err());
    // Redeemed by an order from Shopify carrying the code.
    mock.lock().unwrap().throttle_next = 0;
    let mut o = order_body(6001, 1, "ann@example.com", chrono::Utc::now());
    o["discount_codes"] = json!([{"code": code.to_lowercase(), "amount": "5.00"}]);
    hook(&h, "orders/create", "o1", &o).await;
    h.drain().await;
    let redeemed: Option<i64> = sqlx::query_scalar("SELECT order_id FROM shopify_discount_codes WHERE code = $1").bind(&code).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(redeemed, Some(6001));
    h.cleanup().await;
}

#[tokio::test]
async fn privacy_webhooks() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    hook(&h, "orders/create", "o2", &order_body(5002, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;

    // Data request: an export the owner can download, and only for this customer.
    hook(&h, "customers/data_request", "p1", &json!({"shop_id": 1, "shop_domain": SHOP, "orders_requested": [5001], "customer": {"id": 1, "email": "ann@example.com", "phone": null}, "data_request": {"id": 77}})).await;
    hook(&h, "customers/data_request", "p1", &json!({"shop_id": 1})).await; // redelivery
    h.drain().await;
    let v = h.ok("GET", "/api/shopify", None).await;
    let reqs = v["data_requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 1);
    let id = reqs[0]["id"].as_str().unwrap();
    let export = h.ok("GET", &format!("/api/shopify/data-requests/{id}"), None).await;
    assert_eq!(export["profiles"][0]["email"], "ann@example.com");
    assert!(export["events"].as_array().unwrap().len() >= 3);
    assert!(!export.to_string().contains("bob@example.com"));
    // Another account can't read it.
    let mine = h.cookie.clone();
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "B"}))).await;
    let (s, _) = h.req("GET", &format!("/api/shopify/data-requests/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cookie = mine;

    // Redact: the customer's profile and events are gone; others stay.
    hook(&h, "customers/redact", "p2", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 1, "email": "ann@example.com", "phone": null}, "orders_to_redact": [5001]})).await;
    h.drain().await;
    let ann: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE email = 'ann@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ann, 0);
    assert!(event(&h, "Placed Order", "order:5001").await.is_none());
    assert!(event(&h, "Placed Order", "order:5002").await.is_some());
    let exports: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_data_requests").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(exports, 0, "the export made for them goes too");

    // Shop redact while still connected: ignored.
    hook(&h, "shop/redact", "p3", &json!({"shop_id": 1, "shop_domain": SHOP})).await;
    h.drain().await;
    let shops: i64 = sqlx::query_scalar("SELECT count(*) FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(shops, 1);
    // Uninstalled from Shopify's side: the token goes at once.
    hook(&h, "app/uninstalled", "u1", &json!({"id": 1, "domain": SHOP})).await;
    let (status, tok): (String, Option<String>) = sqlx::query_as("SELECT status, access_token_enc FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok), ("uninstalled", None));
    // Further webhooks are ignored.
    hook(&h, "orders/create", "o3", &order_body(5003, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5003").await.is_none());
    // 48 hours later, shop redact: the connection's data goes; the account's profiles stay.
    hook(&h, "shop/redact", "p4", &json!({"shop_id": 1, "shop_domain": SHOP})).await;
    h.drain().await;
    for table in ["shopify_shops", "shopify_customers", "shopify_webhooks"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(n, 0, "{table}");
    }
    let bob: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE email = 'bob@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(bob, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn expiring_tokens_refresh_once() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    mock.lock().unwrap().expires_in = Some(3600);
    connect(&mut h).await;
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    // The token is about to expire: the worker refreshes it first, once.
    sqlx::query("UPDATE shopify_shops SET token_expires_at = now() + interval '10 seconds'").execute(&h.state.db).await.unwrap();
    hook(&h, "customers/create", "c1", &json!({"id": 1})).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().refreshes, 1);
    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed");
    // Shopify revoked it (401): one refresh and retry, then it works again.
    {
        let mut m = mock.lock().unwrap();
        m.token = "shpat_rotated_elsewhere".into();
    }
    hook(&h, "customers/update", "c2", &json!({"id": 1})).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().refreshes, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn one_worker_per_account_and_bounded_lanes() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    // Every lane busy (other stores working): the job steps aside and comes back.
    sqlx::query("UPDATE shopify_lanes SET account_id = gen_random_uuid(), lease_until = now() + interval '1 minute'").execute(&h.state.db).await.unwrap();
    hook(&h, "customers/create", "c1", &json!({"id": 1})).await;
    h.drain().await;
    assert!(mock.lock().unwrap().node_reads.is_empty());
    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'shopify_work'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(waiting, 1, "queued again for a few seconds later");
    // The account's own lease held by another worker: this one exits without doing anything.
    sqlx::query("UPDATE shopify_lanes SET account_id = NULL, lease_until = NULL").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE shopify_shops SET lease_until = now() + interval '1 minute', lease_token = gen_random_uuid()").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    assert!(mock.lock().unwrap().node_reads.is_empty());
    // The other worker died: once its lease runs out, the scheduler's safety net picks the work up.
    sqlx::query("UPDATE shopify_shops SET lease_until = now() - interval '1 second', work_queued_at = now() - interval '5 minutes'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().node_reads.len(), 1);
    let _ = acct;
    h.cleanup().await;
}

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
    /// The app was uninstalled: the refresh token no longer works either.
    uninstalled: bool,
    tokens_issued: u32,
    refreshes: u32,
    expires_in: Option<i64>,
    granted_scope: String,
    customers: HashMap<i64, Value>,
    products: HashMap<i64, Value>,
    /// Orders as the GraphQL Admin API returns them to a `nodes` read (refunds with transactions).
    orders: HashMap<i64, Value>,
    /// Further pages of a refund's connections, by (refund gid, field): page n answers cursor "n".
    refund_pages: HashMap<(String, String), Vec<Value>>,
    files: HashMap<String, String>,
    bulk_ops: Vec<String>,
    /// Whether each bulk operation asked for `groupObjects: true`.
    grouped: Vec<bool>,
    /// Serve result files exactly as given, even when grouping was asked for.
    ignore_grouping: bool,
    /// Answer ranged file requests with the whole file (200, no Content-Range).
    ignore_range: bool,
    /// Answer ranged file requests with a 206 that starts at byte 0 whatever was asked.
    wrong_range: bool,
    /// Most bytes served per file request, whatever range was asked for (forces many slices).
    max_chunk: usize,
    webhooks_created: Vec<String>,
    /// Topics Shopify refuses to subscribe (a userError).
    refuse_topics: HashSet<String>,
    deletes: u32,
    metafield_deletes: u32,
    /// The pixel delete answers with a userError.
    pixel_delete_fails: bool,
    consent_updates: Vec<Value>,
    /// The next consent updates answer with a userError.
    consent_fail: u32,
    /// Discounts made, by code, with the input they were made from.
    discounts: HashMap<String, Value>,
    discount_inputs: Vec<Value>,
    discount_calls: u32,
    node_reads: Vec<String>,
    throttle_next: u32,
    queries: u32,
    /// Each `nodes` read takes this long (a slow store).
    nodes_delay_ms: u64,
}

type Mock = Arc<Mutex<Shopify>>;

fn customer(id: i64, email: Option<&str>, state: &str, phone: Option<&str>, sms: &str) -> Value {
    customer_at(id, email, state, phone, sms, "2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z")
}

/// A customer with Shopify's own update time and the time of its consent.
fn customer_at(id: i64, email: Option<&str>, state: &str, phone: Option<&str>, sms: &str, updated: &str, consent_at: &str) -> Value {
    json!({
        "id": format!("gid://shopify/Customer/{id}"), "updatedAt": updated, "firstName": "Ann", "lastName": format!("Lee{id}"), "tags": ["vip"],
        "numberOfOrders": "2", "amountSpent": {"amount": "150.00", "currencyCode": "USD"}, "createdAt": "2024-01-01T00:00:00Z",
        "defaultEmailAddress": email.map(|e| json!({"emailAddress": e, "marketingState": state, "marketingOptInLevel": "SINGLE_OPT_IN", "marketingUpdatedAt": consent_at})),
        "defaultPhoneNumber": phone.map(|p| json!({"phoneNumber": p, "smsMarketingConsent": {"state": sms, "optInLevel": "SINGLE_OPT_IN", "updatedAt": consent_at}})),
        "defaultAddress": {"city": "Portland", "province": "Oregon", "countryCodeV2": "US", "zip": "97201"}
    })
}

fn product(id: i64, title: &str, qty: i64, price: &str) -> Value {
    json!({
        "id": format!("gid://shopify/Product/{id}"), "updatedAt": chrono::Utc::now().to_rfc3339(), "title": title, "handle": title.to_lowercase(), "descriptionHtml": "<p>Good</p>", "vendor": "Acme",
        "productType": "Kitchen", "tags": ["gift"], "status": "ACTIVE", "onlineStoreUrl": format!("https://acme.test/products/{}", title.to_lowercase()), "publishedAt": "2025-01-01T00:00:00Z",
        "variants": {"nodes": [{"id": format!("gid://shopify/ProductVariant/{id}1"), "title": "Default Title", "sku": format!("SKU{id}"), "price": price, "compareAtPrice": null,
            "inventoryQuantity": qty, "availableForSale": qty > 0, "inventoryItem": {"id": format!("gid://shopify/InventoryItem/{id}9"), "tracked": true}, "selectedOptions": []}],
            "pageInfo": {"hasNextPage": false, "endCursor": null}},
        "collections": {"nodes": [{"title": "Summer"}]},
        "media": {"nodes": [{"image": {"url": format!("https://cdn.shopify.com/{id}.jpg")}}]}
    })
}

/// One refund as the Admin API reports it: its transactions and refunded lines (shop money).
fn refund(id: i64, at: &str, txs: &[(&str, &str)], lines: &[(i64, &str)]) -> Value {
    json!({
        "id": format!("gid://shopify/Refund/{id}"), "createdAt": at,
        "transactions": {"nodes": txs.iter().map(|(status, amount)| json!({"kind": "REFUND", "status": status, "amountSet": {"shopMoney": {"amount": amount, "currencyCode": "USD"}}})).collect::<Vec<_>>()},
        "refundLineItems": {"nodes": lines.iter().map(|(l, a)| json!({"lineItem": {"id": format!("gid://shopify/LineItem/{l}")}, "subtotalSet": {"shopMoney": {"amount": a}}})).collect::<Vec<_>>()}
    })
}

fn gql_order(id: i64, updated: &str, refunds: Vec<Value>) -> Value {
    json!({"id": format!("gid://shopify/Order/{id}"), "updatedAt": updated, "refunds": refunds})
}

/// Shopify revokes the app's tokens when it's uninstalled.
fn uninstall_in_shopify(m: &Mock) {
    let mut m = m.lock().unwrap();
    m.token = "revoked-by-uninstall".into();
    m.uninstalled = true;
}

fn gql_ok(data: Value) -> (StatusCode, axum::Json<Value>) {
    (StatusCode::OK, axum::Json(json!({"data": data, "extensions": {"cost": {"requestedQueryCost": 10, "actualQueryCost": 5, "throttleStatus": {"maximumAvailable": 2000, "currentlyAvailable": 1990, "restoreRate": 100}}}})))
}

/// The GraphQL aliases (`w0:`, `px:`) a combined mutation asks for.
fn aliases(q: &str, prefix: &str) -> Vec<String> {
    q.split_whitespace().filter(|w| w.ends_with(':') && w.starts_with(prefix)).map(|w| w.trim_end_matches(':').to_string()).collect()
}

async fn graphql(State(m): State<Mock>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> (StatusCode, axum::Json<Value>) {
    let q = body["query"].as_str().unwrap_or("").to_string();
    let vars = body["variables"].clone();
    let delay = m.lock().unwrap().nodes_delay_ms;
    if delay > 0 && q.contains("nodes(ids:") {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
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
    if q.contains("webhookSubscriptionDelete") || q.contains("metafieldsDelete") {
        // Disconnect: one request with an alias per thing removed.
        if q.contains("webhookSubscriptionDelete") {
            m.deletes += 1;
        }
        let mut out = serde_json::Map::new();
        for a in aliases(&q, "w") {
            out.insert(a.clone(), json!({"deletedWebhookSubscriptionId": vars[&a], "userErrors": []}));
        }
        if q.contains("webPixelDelete") {
            out.insert("px".into(), if m.pixel_delete_fails { json!({"deletedWebPixelId": null, "userErrors": [{"field": ["id"], "message": "Web pixel not found", "code": "NOT_FOUND"}]}) } else { json!({"deletedWebPixelId": vars["px"], "userErrors": []}) });
        }
        if q.contains("metafieldsDelete") {
            m.metafield_deletes += 1;
            out.insert("mf".into(), json!({"deletedMetafields": [{"key": "script_url", "namespace": "dumbmail", "ownerId": "gid://shopify/AppInstallation/1"}], "userErrors": []}));
        }
        return gql_ok(Value::Object(out));
    }
    if q.contains("currentAppInstallation") {
        return gql_ok(json!({"shop": {"name": "Acme Goods", "currencyCode": "USD", "ianaTimezone": "America/Los_Angeles", "primaryDomain": {"url": "https://acme.test"}},
            "currentAppInstallation": {"id": "gid://shopify/AppInstallation/1"}, "webhookSubscriptions": {"nodes": []}}));
    }
    if q.contains("webhookSubscriptionCreate") {
        let topic = vars["topic"].as_str().unwrap_or("").to_string();
        if m.refuse_topics.contains(&topic) {
            return gql_ok(json!({"webhookSubscriptionCreate": {"webhookSubscription": null, "userErrors": [{"field": ["webhookSubscription"], "message": "Topic not allowed"}]}}));
        }
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
    if q.contains("bulkOperationRunQuery") {
        let bq = vars["q"].as_str().unwrap_or("");
        let phase = if bq.contains("customers {") { "customers" } else if bq.contains("products {") { "products" } else { "orders" };
        m.bulk_ops.push(phase.to_string());
        m.grouped.push(q.contains("groupObjects: true"));
        return gql_ok(json!({"bulkOperationRunQuery": {"bulkOperation": {"id": format!("gid://shopify/BulkOperation/{}", m.bulk_ops.len()), "status": "CREATED"}, "userErrors": []}}));
    }
    if q.contains("on BulkOperation") {
        let n: usize = vars["id"].as_str().unwrap_or("").rsplit('/').next().unwrap().parse().unwrap();
        let phase = m.bulk_ops[n - 1].clone();
        let url = m.files.get(&phase).filter(|f| !f.is_empty()).map(|_| format!("{}/files/{phase}.jsonl?op={n}", m.files["__base"]));
        return gql_ok(json!({"node": {"id": vars["id"], "status": "COMPLETED", "errorCode": null, "objectCount": "3", "url": url}}));
    }
    if q.contains("... on Refund") {
        let id = vars["id"].as_str().unwrap_or("").to_string();
        let field = if q.contains("transactions(") { "transactions" } else { "refundLineItems" };
        let n: usize = vars["after"].as_str().and_then(|c| c.parse().ok()).unwrap_or(0);
        let pages = m.refund_pages.get(&(id, field.to_string())).cloned().unwrap_or_default();
        let nodes = pages.get(n).cloned().unwrap_or(json!([]));
        let more = n + 1 < pages.len();
        return gql_ok(json!({"node": {field: {"nodes": nodes, "pageInfo": {"hasNextPage": more, "endCursor": (n + 1).to_string()}}}}));
    }
    if q.contains("nodes(ids:") {
        let ids: Vec<String> = serde_json::from_value(vars["ids"].clone()).unwrap();
        m.node_reads.extend(ids.clone());
        let nodes: Vec<Value> = ids
            .iter()
            .map(|g| {
                let id: i64 = g.rsplit('/').next().unwrap().parse().unwrap();
                let found = if g.contains("/Customer/") {
                    m.customers.get(&id)
                } else if g.contains("/Order/") {
                    m.orders.get(&id)
                } else {
                    m.products.get(&id)
                };
                found.cloned().unwrap_or(Value::Null)
            })
            .collect();
        return gql_ok(json!({"nodes": nodes}));
    }
    if q.contains("customerEmailMarketingConsentUpdate") || q.contains("customerSmsMarketingConsentUpdate") {
        let key = if q.contains("Email") { "customerEmailMarketingConsentUpdate" } else { "customerSmsMarketingConsentUpdate" };
        if m.consent_fail > 0 {
            m.consent_fail -= 1;
            return gql_ok(json!({key: {"customer": null, "userErrors": [{"field": ["input", "customerId"], "message": "Customer is invalid", "code": "INVALID"}]}}));
        }
        m.consent_updates.push(vars["input"].clone());
        return gql_ok(json!({key: {"customer": {"id": vars["input"]["customerId"]}, "userErrors": []}}));
    }
    if q.contains("discountCodeBasicCreate") {
        m.discount_calls += 1;
        let code = vars["d"]["code"].as_str().unwrap_or("").to_string();
        m.discount_inputs.push(vars["d"].clone());
        if m.discounts.contains_key(&code) {
            return gql_ok(json!({"discountCodeBasicCreate": {"codeDiscountNode": null, "userErrors": [{"field": ["basicCodeDiscount", "code"], "message": "Code must be unique.", "code": "TAKEN"}]}}));
        }
        m.discounts.insert(code, vars["d"].clone());
        return gql_ok(json!({"discountCodeBasicCreate": {"codeDiscountNode": {"id": format!("gid://shopify/DiscountCodeNode/{}", m.discount_calls)}, "userErrors": []}}));
    }
    if q.contains("codeDiscountNodeByCode") {
        let node = m.discounts.get(vars["c"].as_str().unwrap_or("")).map(|d| {
            let value = &d["customerGets"]["value"];
            let v = if value["percentage"].is_null() { json!({"amount": {"amount": value["discountAmount"]["amount"], "currencyCode": "USD"}}) } else { json!({"percentage": value["percentage"]}) };
            json!({"id": "gid://shopify/DiscountCodeNode/99", "codeDiscount": {"title": d["title"], "usageLimit": d["usageLimit"], "appliesOncePerCustomer": d["appliesOncePerCustomer"],
                "combinesWith": d["combinesWith"], "customerGets": {"value": v}}})
        });
        return gql_ok(json!({"codeDiscountNodeByCode": node}));
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
            !m.uninstalled && f.get("refresh_token").is_some_and(|r| r.starts_with("shprt_"))
        }
        _ => {
            let ok = f.get("code").map(String::as_str) == Some("good-code");
            if ok {
                m.uninstalled = false;
            }
            ok
        }
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

/// Shopify's grouped output: each object's children right after it (by `__parentId`).
fn group_jsonl(content: &str) -> String {
    let lines: Vec<Value> = content.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
    let mut out = String::new();
    fn emit(v: &Value, all: &[Value], out: &mut String) {
        out.push_str(&v.to_string());
        out.push('\n');
        if let Some(id) = v["id"].as_str() {
            for c in all.iter().filter(|c| c["__parentId"].as_str() == Some(id)) {
                emit(c, all, out);
            }
        }
    }
    for v in lines.iter().filter(|v| v.get("__parentId").is_none()) {
        emit(v, &lines, &mut out);
    }
    // Children whose parent isn't in the file stay at the end.
    for v in lines.iter().filter(|v| v["__parentId"].as_str().is_some_and(|p| !lines.iter().any(|o| o["id"].as_str() == Some(p)))) {
        out.push_str(&v.to_string());
        out.push('\n');
    }
    out
}

async fn file(State(m): State<Mock>, Path(name): Path<String>, axum::extract::Query(qs): axum::extract::Query<HashMap<String, String>>, headers: HeaderMap) -> (StatusCode, HeaderMap, String) {
    let m = m.lock().unwrap();
    let phase = name.trim_end_matches(".jsonl");
    let mut content = m.files.get(phase).cloned().unwrap_or_default();
    let op: usize = qs.get("op").and_then(|o| o.parse().ok()).unwrap_or(0);
    if !m.ignore_grouping && op > 0 && m.grouped.get(op - 1) == Some(&true) {
        content = group_jsonl(&content);
    }
    let bytes = content.as_bytes();
    let mut h = HeaderMap::new();
    if m.ignore_range {
        return (StatusCode::OK, h, content.clone());
    }
    let (start, end) = headers
        .get("range")
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .map(|r| {
            let (a, b) = r.split_once('-').unwrap();
            (a.parse::<usize>().unwrap(), b.parse::<usize>().ok().unwrap_or(bytes.len().saturating_sub(1)))
        })
        .unwrap_or((0, bytes.len().saturating_sub(1)));
    let start = if m.wrong_range { 0 } else { start };
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

/// GET with an explicit cookie header (or none); returns status, Location and the cookies it set.
async fn get_full(h: &Harness, path: &str, cookie: Option<&str>) -> (StatusCode, String, Vec<String>) {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    let res = h.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let loc = res.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let set: Vec<String> = res.headers().get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string()).collect();
    let status = res.status();
    let _ = res.into_body().collect().await;
    (status, loc, set)
}

async fn get_redirect(h: &Harness, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let (s, l, _) = get_full(h, path, cookie).await;
    (s, l)
}

/// One scheduler pass for the Shopify feature (tick 0 runs every periodic task).
async fn tick(h: &Harness) {
    use dumbmail::features::Feature;
    dumbmail::features::shopify::Module.tick(&h.state, 0).await.unwrap();
}

fn now_ts() -> String {
    chrono::Utc::now().timestamp().to_string()
}

fn state_of(url: &str) -> String {
    url::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string()
}

/// Start an install as the signed-in person: open DumbMail's install link (which sets the install
/// cookie and sends the browser to Shopify). Returns the state and the browser's cookies.
async fn begin_install(h: &mut Harness, shop: &str) -> (String, String) {
    let v = h.ok("POST", "/api/shopify/connect", Some(json!({"shop": shop}))).await;
    let link = v["url"].as_str().unwrap().to_string();
    assert!(link.starts_with("https://pl.test/api/shopify/install?state="), "{link}");
    let path = link.trim_start_matches("https://pl.test");
    let session = h.cookie.clone().unwrap();
    let (s, loc, set) = get_full(h, path, Some(&session)).await;
    assert!(s.is_redirection(), "{s} {loc}");
    let to = url::Url::parse(&loc).unwrap();
    assert_eq!(to.path(), "/admin/oauth/authorize", "{loc}");
    let nonce = set.iter().find(|c| c.starts_with("dm_shopify_install=")).cloned().expect("install cookie");
    (state_of(&loc), format!("{session}; {nonce}"))
}

async fn finish_install(h: &Harness, shop: &str, state: &str, cookies: Option<&str>) -> String {
    let q = signed_query(&[("code", "good-code"), ("shop", shop), ("state", state), ("timestamp", &now_ts())]);
    get_redirect(h, &format!("/api/shopify/callback?{q}"), cookies).await.1
}

/// Let the history import that connecting starts run to the end.
async fn settle_imports(h: &Harness) {
    for _ in 0..60 {
        h.fast_forward().await;
        let running: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_imports WHERE status = 'running')").fetch_one(&h.state.db).await.unwrap();
        if !running {
            return;
        }
    }
}

/// Connect the store as the signed-in account (install link, Shopify's callback in the same
/// browser), and let the setup and first import finish.
async fn connect(h: &mut Harness) -> String {
    connect_shop(h, SHOP).await
}

async fn connect_shop(h: &mut Harness, shop: &str) -> String {
    let (state, cookies) = begin_install(h, shop).await;
    let loc = finish_install(h, shop, &state, Some(&cookies)).await;
    assert!(loc.ends_with("/settings/shopify?connected=1"), "{loc}");
    h.drain().await;
    settle_imports(h).await;
    state
}

fn webhook_sig(body: &str) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

async fn hook(h: &Harness, topic: &str, id: &str, body: &Value) -> StatusCode {
    hook_with(h, topic, id, body, &[]).await
}

async fn hook_with(h: &Harness, topic: &str, id: &str, body: &Value, extra: &[(&str, &str)]) -> StatusCode {
    let b = body.to_string();
    let sig = webhook_sig(&b);
    let mut headers = vec![("content-type", "application/json"), ("x-shopify-hmac-sha256", sig.as_str()), ("x-shopify-topic", topic), ("x-shopify-shop-domain", SHOP), ("x-shopify-webhook-id", id)];
    headers.extend_from_slice(extra);
    let (s, _, _) = h.raw("POST", "/webhooks/shopify", &headers, &b).await;
    s
}

fn order_body(id: i64, customer_id: i64, email: &str, created: chrono::DateTime<chrono::Utc>) -> Value {
    let m = |a: &str| json!({"shop_money": {"amount": a, "currency_code": "USD"}, "presentment_money": {"amount": a, "currency_code": "USD"}});
    json!({
        "id": id, "admin_graphql_api_id": format!("gid://shopify/Order/{id}"), "name": format!("#{}", id - 4000), "order_number": id - 4000,
        "email": email, "created_at": created.to_rfc3339(), "processed_at": created.to_rfc3339(), "updated_at": created.to_rfc3339(), "cancelled_at": null,
        "currency": "USD", "presentment_currency": "USD", "total_price": "100.00", "subtotal_price": "90.00", "total_discounts": "10.00", "total_tax": "5.00",
        "total_price_set": m("100.00"), "subtotal_price_set": m("90.00"), "total_discounts_set": m("10.00"), "total_tax_set": m("5.00"), "total_shipping_price_set": m("0.00"),
        "financial_status": "paid", "fulfillment_status": null, "test": false, "order_status_url": "https://acme.test/status",
        "discount_codes": [{"code": "WELCOME10", "amount": "10.00", "type": "percentage"}],
        "customer": {"id": customer_id, "email": email, "first_name": "Ann", "last_name": "Lee"},
        "line_items": [
            {"id": 11, "product_id": 101, "variant_id": 1011, "title": "Mug", "variant_title": "Default Title", "sku": "SKU101", "vendor": "Acme", "quantity": 2, "price": "30.00", "price_set": m("30.00"), "total_discount": "0.00", "total_discount_set": m("0.00")},
            {"id": 12, "product_id": 102, "variant_id": 1021, "title": "Bowl", "variant_title": "Blue", "sku": "SKU102", "vendor": "Acme", "quantity": 1, "price": "40.00", "price_set": m("40.00"), "total_discount": "10.00", "total_discount_set": m("10.00")}
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

async fn value(h: &Harness, metric: &str, unique: &str) -> Option<f64> {
    event(h, metric, unique).await.and_then(|e| e.0)
}

async fn count_events(h: &Harness, metric: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = $1").bind(metric).fetch_one(&h.state.db).await.unwrap()
}

async fn consent(h: &Harness, email: &str) -> (String, String, Value) {
    sqlx::query_as("SELECT email_consent, sms_consent, properties FROM profiles WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap()
}

async fn sms_consent(h: &Harness, phone: &str) -> String {
    sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE phone_number = $1").bind(phone).fetch_one(&h.state.db).await.unwrap()
}

async fn profile_id_of(h: &Harness, email: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT id FROM profiles WHERE email = $1").bind(email).fetch_optional(&h.state.db).await.unwrap()
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

/// A live flow on this metric (to see which events start flows).
async fn live_flow(h: &mut Harness, metric: &str) {
    h.ok("POST", "/api/flows", Some(json!({"name": format!("On {metric}"), "trigger": {"type": "metric", "metric": metric}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    sqlx::query("UPDATE flows SET status = 'live' WHERE name NOT IN ('Order confirmed', 'Order shipped')").execute(&h.state.db).await.unwrap();
}

async fn flow_runs(h: &Harness) -> i64 {
    count(h, "SELECT count(*) FROM flow_runs").await
}

fn jsonl(lines: &[Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

/// Another person in the same account, signed in: their session cookie.
async fn teammate(h: &Harness, role: &str) -> (Uuid, String) {
    let acct = account_id(h).await;
    let uid = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')").bind(uid).bind(format!("{uid}@acme.test")).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, $3)").bind(uid).bind(acct).bind(role).execute(&h.state.db).await.unwrap();
    let token = format!("tok{}", uid.simple());
    sqlx::query("INSERT INTO sessions (id, token_hash, user_id, account_id, expires_at) VALUES ($4, $1, $2, $3, now() + interval '1 day')")
        .bind(dumbmail::auth::sha256_hex(&token))
        .bind(uid)
        .bind(acct)
        .bind(Uuid::now_v7())
        .execute(&h.state.db)
        .await
        .unwrap();
    (uid, format!("{}={token}", dumbmail::auth::SESSION_COOKIE))
}

// ---------------------------------------------------------------------------------------------

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
    let (state, cookies) = begin_install(&mut h, "acme-goods").await;
    let v = h.ok("POST", "/api/shopify/connect", Some(json!({"shop": "acme-goods"}))).await;
    assert_eq!(v["shop"], SHOP);
    // The install link went on to Shopify with the app's id, scopes and redirect.
    let session = h.cookie.clone().unwrap();
    let link = v["url"].as_str().unwrap().trim_start_matches("https://pl.test").to_string();
    let (_, loc, _) = get_full(&h, &link, Some(&session)).await;
    let qp: HashMap<String, String> = url::Url::parse(&loc).unwrap().query_pairs().into_owned().collect();
    assert_eq!(url::Url::parse(&loc).unwrap().host_str(), Some(SHOP));
    assert_eq!(qp["client_id"], KEY);
    assert_eq!(qp["redirect_uri"], "https://pl.test/api/shopify/callback");
    assert!(qp["scope"].contains("write_customers") && qp["scope"].contains("read_all_orders"));

    // A forged callback (bad signature, another shop, stale timestamp, unknown state) gets nowhere.
    let good = |state: &str| signed_query(&[("code", "good-code"), ("shop", SHOP), ("state", state), ("timestamp", &now_ts())]);
    let tampered = good(&state).replace("good-code", "evil-code");
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{tampered}"), Some(&cookies)).await.1.ends_with("error=signature"));
    let other_shop = signed_query(&[("code", "good-code"), ("shop", "other.myshopify.com"), ("state", &state), ("timestamp", &now_ts())]);
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{other_shop}"), Some(&cookies)).await.1.ends_with("error=shop"));
    // That consumed the state: it's single use.
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{}", good(&state)), Some(&cookies)).await.1.ends_with("error=expired"));
    let old = signed_query(&[("code", "good-code"), ("shop", SHOP), ("state", "x"), ("timestamp", "1000")]);
    assert!(get_redirect(&h, &format!("/api/shopify/callback?{old}"), Some(&cookies)).await.1.ends_with("error=expired"));
    assert_eq!(mock.lock().unwrap().tokens_issued, 0, "no code was exchanged");

    // The real thing; connecting also starts the history import.
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
    assert_eq!(v["health"]["state"], "waiting");
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

    // Disconnect: a dry run first, then webhooks, pixel and the embed's settings removed and the token forgotten.
    h.cookie = mine;
    let v = h.ok("POST", "/api/shopify/disconnect", Some(json!({}))).await;
    assert_eq!(v["dry_run"], true);
    let v = h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    assert_eq!(v["cleaned_up"], true, "{v}");
    assert_eq!(mock.lock().unwrap().deletes, 1, "one request removes every webhook");
    assert_eq!(mock.lock().unwrap().metafield_deletes, 1, "the theme embed's settings are cleared");
    let (status, tok): (String, Option<String>) = sqlx::query_as("SELECT status, access_token_enc FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok), ("disconnected", None));
    h.cleanup().await;
}

/// Item 11: the callback finishes only in the browser that opened the install link, signed in as
/// the person who started it, who may still manage integrations.
#[tokio::test]
async fn install_is_tied_to_the_person_who_started_it() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    // The link opened in a browser that isn't signed in: back to settings, no cookie.
    let v = h.ok("POST", "/api/shopify/connect", Some(json!({"shop": SHOP}))).await;
    let path = v["url"].as_str().unwrap().trim_start_matches("https://pl.test").to_string();
    let (_, loc, set) = get_full(&h, &path, None).await;
    assert!(loc.ends_with("error=signin") && set.is_empty(), "{loc}");
    // A teammate opening the owner's link is refused.
    let (_, mate) = teammate(&h, "admin").await;
    assert!(get_full(&h, &path, Some(&mate)).await.1.ends_with("error=signin"));

    // The owner opens it; the callback is then forwarded to the teammate (same account, signed in,
    // but without the owner's install cookie): refused.
    let (state, owner_cookies) = begin_install(&mut h, SHOP).await;
    assert!(finish_install(&h, SHOP, &state, Some(&mate)).await.ends_with("error=browser"));
    // Even with the owner's install cookie copied over, the teammate isn't the person who started it.
    let (state, owner_cookies2) = begin_install(&mut h, SHOP).await;
    let nonce = owner_cookies2.split("; ").find(|c| c.starts_with("dm_shopify_install=")).unwrap();
    assert!(finish_install(&h, SHOP, &state, Some(&format!("{mate}; {nonce}"))).await.ends_with("error=signin"));
    // The owner lost the right to manage integrations before finishing: refused.
    let (state, cookies) = begin_install(&mut h, SHOP).await;
    let owner: Uuid = sqlx::query_scalar("SELECT user_id FROM memberships WHERE role = 'owner'").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE memberships SET role = 'manager' WHERE user_id = $1").bind(owner).execute(&h.state.db).await.unwrap();
    assert!(finish_install(&h, SHOP, &state, Some(&cookies)).await.ends_with("error=role"), "a manager doesn't hold shopify.manage");
    sqlx::query("UPDATE memberships SET role = 'owner' WHERE user_id = $1").bind(owner).execute(&h.state.db).await.unwrap();
    assert_eq!(mock.lock().unwrap().tokens_issued, 0, "no code was exchanged");
    let _ = owner_cookies;
    // An analyst can't start one.
    let (_, analyst) = teammate(&h, "analyst").await;
    let mine = h.cookie.replace(analyst);
    let (s, _) = h.req("POST", "/api/shopify/connect", Some(json!({"shop": SHOP}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    h.cookie = mine;
    // The owner, in their own browser: connected.
    connect(&mut h).await;
    assert_eq!(mock.lock().unwrap().tokens_issued, 1);
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
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_webhooks").await, 0);

    // The same delivery three times is stored and processed once.
    for _ in 0..3 {
        assert_eq!(hook(&h, "orders/create", "w1", &body).await, StatusCode::OK);
    }
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_webhooks").await, 1);
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
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let acct = account_id(&h).await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    live_flow(&mut h, "Placed Order").await;

    // Before the store is connected, the browser pixel reports order 5001 (with a made-up total, and forged server-only
    // state that would hide later updates and fake a refund if it were trusted).
    let forged = json!({"total": "9999", "updated_at": "2099-01-01T00:00:00Z", "refunds": {"1": "50"}, "refunds_read_at": "2099-01-01T00:00:00Z", "line_ids": ["11", "12"]});
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "profile": {"email": "ann@example.com"}, "properties": {"tracked_by": "shopify_pixel", "total": 9999, "shopify_state": forged}, "value": 9999, "unique_id": "order:5001"}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(flow_runs(&h).await, 1, "the browser's order started the flow, as it does today");
    let browser_row: Uuid = sqlx::query_scalar("SELECT id FROM events WHERE unique_id = 'order:5001'").fetch_one(&h.state.db).await.unwrap();
    connect(&mut h).await;
    // Once connected, the app's metrics can't be sent from a browser for this account.
    for metric in ["Placed Order", "Started Checkout", "Refunded Order", "Fulfilled Order", "Cancelled Order", "Ordered Product", "Fulfilled Partial Order"] {
        let (s, _, body) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
            &json!({"metric": metric, "profile": {"email": "ann@example.com"}, "value": 5}).to_string()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{metric}: {body}");
    }

    // Then Shopify's webhook: the same event, now with the real amounts, trusted.
    hook(&h, "orders/create", "w1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 1, "one order, one event");
    let trusted_row: Uuid = sqlx::query_scalar("SELECT id FROM events WHERE unique_id = 'order:5001' AND metric_id = (SELECT id FROM metrics WHERE name = 'Placed Order')").fetch_one(&h.state.db).await.unwrap();
    assert_ne!(trusted_row, browser_row, "the trusted event is a fresh row, not the browser's promoted");
    let (value_, props, from_browser, _) = event(&h, "Placed Order", "order:5001").await.unwrap();
    assert_eq!(value_, Some(100.0));
    assert_eq!(from_browser, Some(false));
    assert_eq!(props["tracked_by"], "shopify");
    assert_eq!(props["order_name"], "#1001");
    assert_eq!(props["items"][0]["product_id"], "101");
    assert_eq!(props["items"][1]["variant"], "Blue");
    assert_eq!(props["discount_codes"], json!(["WELCOME10"]));
    assert_eq!(props["shopify_customer_id"], "1");
    assert_eq!(props["shopify_shop"], SHOP);
    assert_eq!(props["refunded_total"], 0.0, "nothing from the browser's copy is kept");
    // The flow sees the trusted order as a new event; Ann is already in it from the browser's copy.
    assert_eq!(flow_runs(&h).await, 1);
    assert_eq!(count_events(&h, "Ordered Product").await, 2);
    assert_eq!(value(&h, "Ordered Product", "order:5001:12").await, Some(30.0));
    // The order doesn't touch consent.
    assert_eq!(consent(&h, "ann@example.com").await.0, "never_subscribed");

    // Shopify's order; a browser copy after it is refused.
    hook(&h, "orders/create", "w2", &order_body(5002, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "profile": {"email": "bob@example.com"}, "value": 1, "unique_id": "order:5002"}).to_string()).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 2);
    assert_eq!(value(&h, "Placed Order", "order:5002").await, Some(100.0));
    assert_eq!(flow_runs(&h).await, 2);

    // An old order seen for the first time (an edit to last week's order) is history: no flow.
    hook(&h, "orders/updated", "w3", &order_body(5003, 3, "cat@example.com", chrono::Utc::now() - chrono::Duration::days(7))).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Placed Order").await, 3);
    assert_eq!(flow_runs(&h).await, 2, "history doesn't start flows");

    // A refund: read from the Admin API (shop money, successful transactions), applied once
    // however many webhooks mention it.
    let now = chrono::Utc::now().to_rfc3339();
    mock.lock().unwrap().orders.insert(5001, gql_order(5001, &now, vec![refund(900, &now, &[("SUCCESS", "30.00")], &[(12, "30.00")])]));
    let refund_hook = json!({"id": 900, "order_id": 5001, "created_at": now, "transactions": [{"kind": "refund", "status": "success", "amount": "30.00"}], "refund_line_items": [{"line_item_id": 12, "quantity": 1, "subtotal": 30.0}]});
    hook(&h, "refunds/create", "w4", &refund_hook).await;
    hook(&h, "refunds/create", "w5", &refund_hook).await; // a second delivery of the same refund
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(70.0));
    assert_eq!(value(&h, "Ordered Product", "order:5001:12").await, Some(0.0));
    assert_eq!(value(&h, "Ordered Product", "order:5001:11").await, Some(60.0));
    assert_eq!(value(&h, "Refunded Order", "refund:900").await, Some(30.0));
    let mut updated = order_body(5001, 1, "ann@example.com", chrono::Utc::now());
    updated["refunds"] = json!([refund_hook]);
    hook(&h, "orders/updated", "w6", &updated).await;
    // An order payload without the refund (orders/paid) keeps it counted.
    hook(&h, "orders/paid", "w7", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(70.0));
    assert_eq!(count_events(&h, "Refunded Order").await, 1);

    // Cancelled: revenue goes to 0 and a Cancelled Order event is recorded.
    let mut cancelled = order_body(5001, 1, "ann@example.com", chrono::Utc::now());
    cancelled["refunds"] = json!([refund_hook]);
    cancelled["cancelled_at"] = json!(chrono::Utc::now().to_rfc3339());
    cancelled["cancel_reason"] = json!("customer");
    hook(&h, "orders/cancelled", "w8", &cancelled).await;
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(0.0));
    assert_eq!(value(&h, "Cancelled Order", "order:5001").await, Some(100.0));
    // Revenue as the overview and reports count it: the sum of Placed Order values.
    let revenue: f64 = sqlx::query_scalar("SELECT sum(e.value) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Placed Order'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(revenue, 200.0);

    // Shipments.
    let mut partial = order_body(5002, 2, "bob@example.com", chrono::Utc::now());
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

/// Item 1: an order or checkout goes to the contact in that event, never a stale linked profile;
/// one with no contact is recorded for the linked profile but starts nothing.
#[tokio::test]
async fn events_go_to_the_events_own_contact() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    live_flow(&mut h, "Placed Order").await;
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    h.drain().await;
    let old = profile_id_of(&h, "ann@example.com").await.unwrap();

    // Customer 1 orders with a new address: the order is the new address's, and the link moves.
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann.new@example.com", chrono::Utc::now())).await;
    h.drain().await;
    let new = profile_id_of(&h, "ann.new@example.com").await.unwrap();
    let (_, props, _, owner) = event(&h, "Placed Order", "order:5001").await.unwrap();
    assert_eq!(owner, new, "not the old address's profile");
    assert_ne!(owner, old);
    assert_eq!(event(&h, "Ordered Product", "order:5001:11").await.unwrap().3, new);
    assert_eq!(props["shopify_customer_id"], "1");
    let linked: Uuid = sqlx::query_scalar("SELECT profile_id FROM shopify_customers WHERE customer_id = 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(linked, new, "the link moved to the order's contact");
    assert_eq!(flow_runs(&h).await, 1);
    let runs_for_old: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs WHERE profile_id = $1").bind(old).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(runs_for_old, 0);

    // A checkout with yet another address goes to that address.
    let checkout = json!({"token": "tok1", "email": "ann.third@example.com", "completed_at": null, "created_at": chrono::Utc::now().to_rfc3339(), "currency": "USD", "presentment_currency": "USD",
        "total_price": "64.00", "customer": {"id": 1, "email": "ann@example.com"}, "line_items": []});
    hook(&h, "checkouts/create", "k1", &checkout).await;
    h.drain().await;
    assert_eq!(event(&h, "Started Checkout", "checkout:tok1").await.unwrap().3, profile_id_of(&h, "ann.third@example.com").await.unwrap());

    // No email or phone on the order: recorded for the linked profile, marked, and no flow starts.
    let mut anon = order_body(5002, 1, "", chrono::Utc::now());
    anon["email"] = Value::Null;
    anon["customer"] = json!({"id": 1});
    hook(&h, "orders/create", "o2", &anon).await;
    h.drain().await;
    let (_, props, _, owner) = event(&h, "Placed Order", "order:5002").await.unwrap();
    let linked: Uuid = sqlx::query_scalar("SELECT profile_id FROM shopify_customers WHERE customer_id = 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(owner, linked);
    assert_eq!(props["$no_contact"], true);
    assert_eq!(flow_runs(&h).await, 1, "an order without a contact starts nothing");
    h.cleanup().await;
}

#[tokio::test]
async fn checkouts_match_the_pixel() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts").fetch_one(&h.state.db).await.unwrap();
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Started Checkout", "profile": {"email": "ann@example.com"}, "value": 5, "unique_id": "checkout:tok123"}).to_string()).await;
    connect(&mut h).await;
    let checkout = json!({"token": "tok123", "email": "ann@example.com", "completed_at": null, "created_at": chrono::Utc::now().to_rfc3339(), "currency": "USD", "presentment_currency": "USD", "total_price": "64.00",
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
    // Paid in another currency: recorded without a value (shop money isn't in the payload).
    let mut euro = checkout.clone();
    euro["token"] = json!("tok777");
    euro["presentment_currency"] = json!("EUR");
    hook(&h, "checkouts/create", "c4", &euro).await;
    h.drain().await;
    assert_eq!(count_events(&h, "Started Checkout").await, 2);
    let (v, props, from_browser, _) = event(&h, "Started Checkout", "checkout:tok123").await.unwrap();
    assert_eq!((v, from_browser), (Some(64.0), Some(false)));
    assert_eq!(props["checkout_url"], "https://acme.test/checkouts/tok123/recover");
    assert_eq!(props["item_count"], 2);
    let (v, props, _, _) = event(&h, "Started Checkout", "checkout:tok777").await.unwrap();
    assert_eq!(v, None);
    assert_eq!(props["currency"], "USD");
    assert!(props.get("total").is_none());
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
    mock.lock().unwrap().customers.insert(1, customer_at(1, Some("ann.new@example.com"), "NOT_SUBSCRIBED", Some("+15035550101"), "SUBSCRIBED", "2025-02-01T00:00:00Z", "2025-01-01T00:00:00Z"));
    hook(&h, "customers/update", "c6", &json!({"id": 1})).await;
    h.drain().await;
    assert_eq!(consent(&h, "ann.new@example.com").await.0, "never_subscribed");
    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed", "the old address keeps its own consent");
    let phone_on_new: Option<String> = sqlx::query_scalar("SELECT phone_number FROM profiles WHERE email = 'ann.new@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(phone_on_new, None, "the number stays with the profile that has it");

    // Unsubscribes in DumbMail are written back to Shopify, by address and number: Dee unsubscribed
    // here while Shopify still has her subscribed; Ann's old profile, which holds customer 1's SMS
    // number, unsubscribed from texts (the number is still customer 1's in Shopify). Ann's old email
    // isn't Shopify's any more (customer 1 has the new one), so that unsubscribe writes nothing.
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed', sms_consent = 'unsubscribed' WHERE email = 'ann@example.com'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    {
        let m = mock.lock().unwrap();
        assert_eq!(m.consent_updates.len(), 2, "{:?}", m.consent_updates);
        let email = m.consent_updates.iter().find(|u| u.get("emailMarketingConsent").is_some()).unwrap();
        assert_eq!(email["customerId"], "gid://shopify/Customer/4");
        assert_eq!(email["emailMarketingConsent"]["marketingState"], "UNSUBSCRIBED");
        let sms = m.consent_updates.iter().find(|u| u.get("smsMarketingConsent").is_some()).unwrap();
        assert_eq!(sms["customerId"], "gid://shopify/Customer/1");
    }
    // Written once.
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().consent_updates.len(), 2);
    // Bob unsubscribes from an email here: written back on the next pass.
    sqlx::query("UPDATE shopify_customers SET email_state = 'SUBSCRIBED' WHERE customer_id = 2").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE email = 'bob@example.com'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().consent_updates.len(), 3);
    sqlx::query("UPDATE profiles SET email_consent = 'never_subscribed' WHERE email = 'bob@example.com'").execute(&h.state.db).await.unwrap();

    // Shopify deleting a customer stops write-backs but keeps the link for privacy requests.
    hook(&h, "customers/delete", "c8", &json!({"id": 2})).await;
    h.drain().await;
    let deleted: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT deleted_at FROM shopify_customers WHERE customer_id = 2").fetch_one(&h.state.db).await.unwrap();
    assert!(deleted.is_some());
    assert_eq!(consent(&h, "bob@example.com").await.0, "never_subscribed");
    h.cleanup().await;
}

/// Item 2: an opt-out is never lost, and Shopify's consent only applies when it's newer.
#[tokio::test]
async fn opt_outs_are_never_lost() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;

    // Shopify refuses the first write-back (a userError): not marked done, retried, then confirmed.
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    h.drain().await;
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE email = 'ann@example.com'").execute(&h.state.db).await.unwrap();
    mock.lock().unwrap().consent_fail = 1;
    tick(&h).await;
    h.drain().await;
    let (state, attempts): (String, i32) = sqlx::query_as("SELECT email_state, email_wb_attempts FROM shopify_customers WHERE customer_id = 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((state.as_str(), attempts), ("SUBSCRIBED", 1), "a refused write-back stays queued");
    let v = h.ok("GET", "/api/shopify", None).await;
    assert_eq!(v["counts"]["unsubscribes_to_write_back"], 1);
    assert!(v["problems"].to_string().contains("haven't been accepted by Shopify"), "{v}");
    // Not before its backoff…
    tick(&h).await;
    h.drain().await;
    assert!(mock.lock().unwrap().consent_updates.is_empty());
    // …then it goes through.
    sqlx::query("UPDATE shopify_customers SET email_wb_after = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().consent_updates.len(), 1);
    let state: String = sqlx::query_scalar("SELECT email_state FROM shopify_customers WHERE customer_id = 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(state, "UNSUBSCRIBED");

    // A customer whose SMS number belongs to a different (phone-only) profile: its text unsubscribe
    // is still written back.
    let phone_profile = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, phone_number, sms_consent) VALUES ($1, $2, '+15035550102', 'never_subscribed')").bind(phone_profile).bind(acct).execute(&h.state.db).await.unwrap();
    mock.lock().unwrap().customers.insert(2, customer(2, Some("bob@example.com"), "NOT_SUBSCRIBED", Some("+15035550102"), "SUBSCRIBED"));
    hook(&h, "customers/update", "c2", &json!({"id": 2})).await;
    h.drain().await;
    assert_eq!(sms_consent(&h, "+15035550102").await, "subscribed");
    sqlx::query("UPDATE profiles SET sms_consent = 'unsubscribed' WHERE id = $1").bind(phone_profile).execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert!(mock.lock().unwrap().consent_updates.iter().any(|u| u["customerId"] == "gid://shopify/Customer/2" && u.get("smsMarketingConsent").is_some()));

    // A changed number gets a profile and its own consent (not left unlinked).
    mock.lock().unwrap().customers.insert(2, customer_at(2, Some("bob@example.com"), "NOT_SUBSCRIBED", Some("+15035550199"), "SUBSCRIBED", "2025-03-01T00:00:00Z", "2025-03-01T00:00:00Z"));
    hook(&h, "customers/update", "c3", &json!({"id": 2})).await;
    h.drain().await;
    assert_eq!(sms_consent(&h, "+15035550199").await, "subscribed");
    assert_eq!(sms_consent(&h, "+15035550102").await, "unsubscribed", "the old number keeps its own");

    // Consent applies only when newer: a late report from before the last applied one changes nothing.
    mock.lock().unwrap().customers.insert(3, customer_at(3, Some("cat@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED", "2025-05-01T00:00:00Z", "2025-05-01T00:00:00Z"));
    hook(&h, "customers/update", "c4", &json!({"id": 3})).await;
    h.drain().await;
    assert_eq!(consent(&h, "cat@example.com").await.0, "subscribed");
    let late = customer_at(3, Some("cat@example.com"), "UNSUBSCRIBED", None, "NOT_SUBSCRIBED", "2025-06-01T00:00:00Z", "2025-04-01T00:00:00Z");
    mock.lock().unwrap().customers.insert(3, late);
    hook(&h, "customers/update", "c5", &json!({"id": 3})).await;
    h.drain().await;
    assert_eq!(consent(&h, "cat@example.com").await.0, "subscribed", "an older consent time doesn't apply");
    // A bulk snapshot older than what's saved changes neither consent nor the link.
    let snapshot = customer_at(3, Some("cat.old@example.com"), "UNSUBSCRIBED", None, "NOT_SUBSCRIBED", "2025-01-15T00:00:00Z", "2025-07-01T00:00:00Z");
    mock.lock().unwrap().files.insert("customers".into(), jsonl(&[snapshot]));
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    assert_eq!(consent(&h, "cat@example.com").await.0, "subscribed");
    assert!(profile_id_of(&h, "cat.old@example.com").await.is_none(), "an older snapshot makes no profile");
    let email: String = sqlx::query_scalar("SELECT email FROM shopify_customers WHERE customer_id = 3").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(email, "cat@example.com");

    // INVALID and REDACTED never leave an address subscribed.
    mock.lock().unwrap().customers.insert(3, customer_at(3, Some("cat@example.com"), "INVALID", None, "NOT_SUBSCRIBED", "2025-08-01T00:00:00Z", "2025-08-01T00:00:00Z"));
    hook(&h, "customers/update", "c6", &json!({"id": 3})).await;
    h.drain().await;
    assert_eq!(consent(&h, "cat@example.com").await.0, "unsubscribed");
    mock.lock().unwrap().customers.insert(4, customer_at(4, Some("dee@example.com"), "SUBSCRIBED", Some("+15035550104"), "SUBSCRIBED", "2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z"));
    hook(&h, "customers/update", "c7", &json!({"id": 4})).await;
    h.drain().await;
    mock.lock().unwrap().customers.insert(4, customer_at(4, Some("dee@example.com"), "REDACTED", Some("+15035550104"), "REDACTED", "2025-09-01T00:00:00Z", "2025-09-01T00:00:00Z"));
    hook(&h, "customers/update", "c8", &json!({"id": 4})).await;
    h.drain().await;
    assert_eq!(consent(&h, "dee@example.com").await.0, "unsubscribed");
    assert_eq!(sms_consent(&h, "+15035550104").await, "unsubscribed");

    // Shopify's consent for one address never lands on another address's profile: customer 5's
    // link moves to the profile of an order's contact, and its subscribed address stays its own.
    mock.lock().unwrap().customers.insert(5, customer(5, Some("eve@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "orders/create", "o1", &order_body(5005, 5, "eve.other@example.com", chrono::Utc::now())).await;
    h.drain().await;
    hook(&h, "customers/update", "c9", &json!({"id": 5})).await;
    h.drain().await;
    assert_eq!(consent(&h, "eve@example.com").await.0, "subscribed");
    assert_eq!(consent(&h, "eve.other@example.com").await.0, "never_subscribed");
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
            "totalPriceSet": {"shopMoney": {"amount": "50.00", "currencyCode": "USD"}}, "subtotalPriceSet": {"shopMoney": {"amount": "50.00"}}, "totalDiscountsSet": {"shopMoney": {"amount": "0.0"}},
            "totalShippingPriceSet": {"shopMoney": {"amount": "0.0"}}, "totalTaxSet": {"shopMoney": {"amount": "0.0"}},
            "customer": {"id": format!("gid://shopify/Customer/{cust}"), "firstName": "A", "lastName": "B"},
            "refunds": refunds, "fulfillments": [{"id": format!("gid://shopify/Fulfillment/{id}"), "createdAt": ago(days), "status": "SUCCESS"}]
        });
        let line = |order: i64, line: i64| json!({"id": format!("gid://shopify/LineItem/{line}"), "title": "Mug", "variantTitle": null, "sku": "SKU101", "vendor": "Acme", "quantity": 1,
            "originalUnitPriceSet": {"shopMoney": {"amount": "50.00"}}, "totalDiscountSet": {"shopMoney": {"amount": "0.00"}}, "product": {"id": "gid://shopify/Product/101"},
            "variant": {"id": "gid://shopify/ProductVariant/1011"}, "image": {"url": "https://cdn.shopify.com/m.jpg"}, "__parentId": format!("gid://shopify/Order/{order}")});
        m.files.insert("orders".into(), jsonl(&[
            order(5001, 1, "ann@example.com", 300, false, json!([{"id": "gid://shopify/Refund/9"}])),
            line(5001, 1),
            order(5002, 2, "bob@example.com", 200, true, json!([])),
            line(5002, 2),
            order(5003, 1, "ann@example.com", 2, false, json!([])),
            line(5003, 3),
        ]));
        // The refund's own amounts come from the Admin API.
        m.orders.insert(5001, gql_order(5001, &ago(299), vec![refund(9, &ago(299), &[("SUCCESS", "20.00")], &[])]));
        // Serve at most 200 bytes per request, so records span many reads.
        m.max_chunk = 200;
    }
    // A segment-triggered flow and a metric flow, both live: history must start neither.
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Buyers", "definition": {"match": "all", "conditions": [{"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1}]}}))).await;
    let seg_id = seg["id"].as_str().unwrap().to_string();
    h.ok("POST", "/api/flows", Some(json!({"name": "Welcome buyers", "trigger": {"type": "segment", "segment_id": seg_id}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    sqlx::query("UPDATE flows SET status = 'live' WHERE name NOT IN ('Order confirmed', 'Order shipped')").execute(&h.state.db).await.unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id.parse().unwrap()).await.unwrap();

    let ops_before = mock.lock().unwrap().bulk_ops.len();
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
    h.fast_forward().await;
    let v = h.ok("GET", "/api/shopify/import", None).await;
    assert_eq!(v["import"]["status"], "done", "{v}");
    assert_eq!(v["import"]["counts"]["customers"], 2);
    assert_eq!(v["import"]["counts"]["orders"], 3);
    assert_eq!(mock.lock().unwrap().bulk_ops[ops_before..], ["customers", "products", "orders"]);
    dumbmail::flows::refresh_segment(&h.state, seg_id.parse().unwrap()).await.unwrap();
    assert_eq!(flow_runs(&h).await, 0, "imported history starts no flows, metric or segment");
    let members: i32 = sqlx::query_scalar("SELECT member_count FROM segments").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(members, 2, "but people are in the segment");
    // Nothing is left marked to absorb other people's entries.
    assert_eq!(count(&h, "SELECT count(*) FROM segments WHERE absorb_entries").await, 0);

    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed");
    assert_eq!(consent(&h, "bob@example.com").await.0, "never_subscribed");
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(30.0), "less the refund");
    assert_eq!(value(&h, "Placed Order", "order:5002").await, Some(0.0), "cancelled");
    assert_eq!(value(&h, "Refunded Order", "refund:9").await, Some(20.0));
    assert_eq!(count_events(&h, "Fulfilled Order").await, 3);
    // Imported events carry their own time and don't count as fresh activity.
    let last: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT last_event_at FROM profiles WHERE email = 'bob@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(last < chrono::Utc::now() - chrono::Duration::days(199));
    let p = h.ok("GET", "/api/catalog/products/101", None).await;
    assert_eq!(p["categories"], json!(["Summer"]));
    assert_eq!(p["image_url"], "https://cdn.shopify.com/m.jpg");
    assert_eq!(p["inventory"], 4);

    // Running it again adds nothing twice.
    let before = count(&h, "SELECT count(*) FROM events").await;
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    assert_eq!(before, count(&h, "SELECT count(*) FROM events").await);
    h.cleanup().await;
}

/// Item 13: connecting starts the history import.
#[tokio::test]
async fn connecting_starts_the_history_import() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    mock.lock().unwrap().files.insert("customers".into(), jsonl(&[customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED")]));
    connect(&mut h).await;
    let v = h.ok("GET", "/api/shopify/import", None).await;
    assert_eq!(v["import"]["status"], "done", "{v}");
    assert_eq!(v["import"]["counts"]["customers"], 1);
    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed");
    // Setup running again (a retry) doesn't start a second one.
    let (acct, generation): (Uuid, i32) = sqlx::query_as("SELECT account_id, generation FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    dumbmail::features::shopify::oauth::setup(&h.state, acct, Some(generation)).await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_imports WHERE kind = 'history'").await, 1);
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
    settle_imports(&h).await;
    assert_eq!(h.ok("GET", "/api/shopify/import", None).await["import"]["status"], "done");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 30);
    // A crash that lost the saved position, so the customers are read again from the start: nothing doubles.
    sqlx::query("UPDATE shopify_imports SET status = 'running', phase = 'customers', bulk_id = NULL, result_offset = 0, finished_at = NULL WHERE kind = 'history' AND id = (SELECT id FROM shopify_imports ORDER BY started_at DESC LIMIT 1)").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE shopify_shops SET work_queued_at = NULL").execute(&h.state.db).await.unwrap();
    dumbmail::features::shopify::shop::kick_now(&h.state.db, account_id(&h).await).await.unwrap();
    settle_imports(&h).await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 30);
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_customers").await, 30);
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

/// Item 4: bulk results ask Shopify to group children under their parents, and every child still
/// joins the parent its `__parentId` names, never the record before it.
#[tokio::test]
async fn bulk_children_join_their_own_parent() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    assert!(mock.lock().unwrap().grouped.iter().all(|g| *g), "every bulk operation asks for groupObjects");
    let variant = |p: i64, price: &str| json!({"id": format!("gid://shopify/ProductVariant/{p}1"), "title": "Default Title", "price": price, "inventoryQuantity": 1, "availableForSale": true,
        "inventoryItem": {"id": format!("gid://shopify/InventoryItem/{p}9"), "tracked": true}, "__parentId": format!("gid://shopify/Product/{p}")});
    let prod = |p: i64| json!({"id": format!("gid://shopify/Product/{p}"), "title": format!("P{p}"), "handle": format!("p{p}"), "status": "ACTIVE", "publishedAt": "2025-01-01T00:00:00Z", "tags": []});
    // Interleaved: P1, P2, then P1's variant, P2's variant, and a child of a product not in the file.
    let interleaved = jsonl(&[prod(1), prod(2), variant(1, "11.00"), variant(2, "22.00"), variant(9, "99.00")]);
    for ignore_grouping in [false, true] {
        {
            let mut m = mock.lock().unwrap();
            m.files.insert("products".into(), interleaved.clone());
            m.ignore_grouping = ignore_grouping;
        }
        h.ok("POST", "/api/shopify/products/resync", None).await;
        settle_imports(&h).await;
        assert_eq!(h.ok("GET", "/api/catalog/products/1", None).await["price"], 11.0, "grouping ignored: {ignore_grouping}");
        assert_eq!(h.ok("GET", "/api/catalog/products/2", None).await["price"], 22.0);
        let v = h.ok("GET", "/api/shopify/import", None).await;
        assert_eq!(v["products_sync"]["counts"]["reread_parents"], 1, "{v}");
    }
    let (s, _) = h.req("GET", "/api/catalog/products/9", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

/// Item 5: the history import (and the store's first catalog load) queues no back-in-stock or
/// price-drop alerts; a later reconcile still does.
#[tokio::test]
async fn history_import_queues_no_alerts() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    // Every alert queued is remembered (the jobs themselves run and go).
    sqlx::query("CREATE TABLE seen_jobs (kind text)").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE FUNCTION see_job() RETURNS trigger AS $$ BEGIN INSERT INTO seen_jobs VALUES (NEW.kind); RETURN NEW; END $$ LANGUAGE plpgsql").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE TRIGGER see_job AFTER INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION see_job()").execute(&h.state.db).await.unwrap();
    // The product was already in the catalog (from a feed or the API), dearer and out of stock.
    h.ok("POST", "/api/catalog/products", Some(json!({"products": [{"id": "101", "title": "Mug", "price": 20.0, "available": false, "inventory": 0}]}))).await;
    let prod = |price: &str, qty: i64| jsonl(&[
        json!({"id": "gid://shopify/Product/101", "updatedAt": chrono::Utc::now().to_rfc3339(), "title": "Mug", "handle": "mug", "status": "ACTIVE", "publishedAt": "2025-01-01T00:00:00Z", "tags": []}),
        json!({"id": "gid://shopify/ProductVariant/1011", "title": "Default Title", "price": price, "inventoryQuantity": qty, "availableForSale": qty > 0, "inventoryItem": {"id": "gid://shopify/InventoryItem/1019", "tracked": true}, "__parentId": "gid://shopify/Product/101"}),
    ]);
    mock.lock().unwrap().files.insert("products".into(), prod("12.00", 5));
    connect(&mut h).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/101", None).await["price"], 12.0);
    let alerts = || count(&h, "SELECT count(*) FROM seen_jobs WHERE kind IN ('catalog_price_drop', 'catalog_back_in_stock')");
    assert_eq!(alerts().await, 0, "the history import is quiet");
    // A reconcile after the first load is how a missed webhook's price drop is caught: it alerts.
    mock.lock().unwrap().files.insert("products".into(), prod("10.00", 5));
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    assert_eq!(count(&h, "SELECT count(*) FROM seen_jobs WHERE kind = 'catalog_price_drop'").await, 1);
    h.cleanup().await;
}

/// Item 6: refunds count only money that moved, a fuller later read corrects an earlier one, older
/// reads and older order snapshots change nothing, and amounts are the shop's, unrounded.
#[tokio::test]
async fn refunds_are_right_and_can_be_corrected() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let t = |mins: i64| (chrono::Utc::now() + chrono::Duration::minutes(mins)).to_rfc3339();
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now() - chrono::Duration::minutes(30))).await;
    h.drain().await;
    let refund_hook = |id: i64| json!({"id": id, "order_id": 5001, "created_at": t(0), "transactions": [], "refund_line_items": []});

    // A pending refund subtracts nothing.
    mock.lock().unwrap().orders.insert(5001, gql_order(5001, &t(-20), vec![refund(900, &t(-20), &[("PENDING", "30.00")], &[(12, "30.00")])]));
    hook(&h, "refunds/create", "r1", &refund_hook(900)).await;
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(100.0));
    assert_eq!(value(&h, "Ordered Product", "order:5001:12").await, Some(30.0));
    assert!(event(&h, "Refunded Order", "refund:900").await.is_none());
    // It succeeds, partly: 10 now…
    mock.lock().unwrap().orders.insert(5001, gql_order(5001, &t(-15), vec![refund(900, &t(-20), &[("SUCCESS", "10.00"), ("FAILURE", "20.00")], &[(12, "10.00")])]));
    hook(&h, "refunds/create", "r2", &refund_hook(900)).await;
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(90.0));
    assert_eq!(value(&h, "Refunded Order", "refund:900").await, Some(10.0));
    // …and a fuller later read corrects it (the same refund id, now 30).
    mock.lock().unwrap().orders.insert(5001, gql_order(5001, &t(-10), vec![refund(900, &t(-20), &[("SUCCESS", "10.00"), ("SUCCESS", "20.00")], &[(12, "30.00")])]));
    hook(&h, "orders/updated", "o2", &{
        let mut o = order_body(5001, 1, "ann@example.com", chrono::Utc::now() - chrono::Duration::minutes(30));
        o["updated_at"] = json!(t(-10));
        o["refunds"] = json!([refund_hook(900)]);
        o
    })
    .await;
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(70.0));
    assert_eq!(value(&h, "Ordered Product", "order:5001:12").await, Some(0.0));
    assert_eq!(value(&h, "Refunded Order", "refund:900").await, Some(30.0));
    // An older read (Shopify's updatedAt before the one applied) changes nothing.
    mock.lock().unwrap().orders.insert(5001, gql_order(5001, &t(-12), vec![refund(900, &t(-20), &[("SUCCESS", "5.00")], &[])]));
    hook(&h, "refunds/create", "r3", &refund_hook(900)).await;
    h.drain().await;
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(70.0));
    // An older order snapshot (orders/paid delayed) doesn't overwrite the newer one.
    let mut stale = order_body(5001, 1, "ann@example.com", chrono::Utc::now() - chrono::Duration::minutes(30));
    stale["updated_at"] = json!(t(-25));
    stale["total_price_set"] = json!({"shop_money": {"amount": "50.00", "currency_code": "USD"}});
    hook(&h, "orders/paid", "o3", &stale).await;
    h.drain().await;
    let (v, props, _, _) = event(&h, "Placed Order", "order:5001").await.unwrap();
    assert_eq!((v, props["total"].as_f64()), (Some(70.0), Some(100.0)));

    // Order and lines move together: a crash after the order's value was written but before its
    // lines' (simulated) is put right by the next delivery.
    sqlx::query("UPDATE events SET value = 30 WHERE unique_id = 'order:5001:12'").execute(&h.state.db).await.unwrap();
    mock.lock().unwrap().orders.insert(5001, gql_order(5001, &t(-5), vec![refund(900, &t(-20), &[("SUCCESS", "30.00")], &[(12, "30.00")])]));
    hook(&h, "refunds/create", "r4", &refund_hook(900)).await;
    h.drain().await;
    assert_eq!(value(&h, "Ordered Product", "order:5001:12").await, Some(0.0));

    // Shop money, exactly: the buyer paid in another currency; the shop's amounts (3 decimals) are used.
    let mut kwd = order_body(5002, 2, "bob@example.com", chrono::Utc::now());
    let both = |shop: &str, buyer: &str| json!({"shop_money": {"amount": shop, "currency_code": "KWD"}, "presentment_money": {"amount": buyer, "currency_code": "USD"}});
    kwd["currency"] = json!("KWD");
    kwd["presentment_currency"] = json!("USD");
    kwd["total_price_set"] = both("19.995", "65.00");
    kwd["total_price"] = json!("19.995");
    kwd["line_items"] = json!([{"id": 21, "product_id": 101, "title": "Mug", "quantity": 1, "price": "65.00", "price_set": both("19.995", "65.00"), "total_discount_set": both("0.000", "0.00")}]);
    hook(&h, "orders/create", "o4", &kwd).await;
    h.drain().await;
    let (v, props, _, _) = event(&h, "Placed Order", "order:5002").await.unwrap();
    assert_eq!(v, Some(19.995));
    assert_eq!(props["currency"], "KWD");
    assert_eq!(props["items"][0]["price"], 19.995);
    assert_eq!(value(&h, "Ordered Product", "order:5002:21").await, Some(19.995));
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
    // The template's stacking rules, as configured: no stacking at all.
    assert_eq!(mock.lock().unwrap().discount_inputs[0]["combinesWith"], json!({"orderDiscounts": false, "productDiscounts": false, "shippingDiscounts": false}));
    // The same grant again: the same code, no second call.
    assert_eq!(create_discount_code(&h.state, acct, &t, pid, "m1").await.unwrap(), code);
    assert_eq!(mock.lock().unwrap().discount_calls, 1);
    // Shopify made the code but we crashed before saving it: the retry finds it and keeps it.
    sqlx::query("DELETE FROM shopify_discount_codes").execute(&h.state.db).await.unwrap();
    assert_eq!(create_discount_code(&h.state, acct, &t, pid, "m1").await.unwrap(), code);
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_discount_codes").await, 1);
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

/// Item 7: a saved code belongs to one store, template, person and grant.
#[tokio::test]
async fn discount_codes_belong_to_one_shop_and_grant() {
    use dumbmail::features::shopify::{create_discount_code, DiscountTemplate, DiscountValue};
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let acct = account_id(&h).await;
    let t = DiscountTemplate { key: "coupon-1".into(), title: "$5 off".into(), value: DiscountValue::FixedAmount(5.0), minimum_subtotal: None, expires_after_days: None, ends_at: None, collection_ids: vec![], prefix: "".into(), combines_with_other_discounts: true };
    let ann: Uuid = sqlx::query_scalar("INSERT INTO profiles (id, account_id, email) VALUES ($1, $2, 'ann@example.com') RETURNING id").bind(Uuid::now_v7()).bind(acct).fetch_one(&h.state.db).await.unwrap();
    let bob: Uuid = sqlx::query_scalar("INSERT INTO profiles (id, account_id, email) VALUES ($1, $2, 'bob@example.com') RETURNING id").bind(Uuid::now_v7()).bind(acct).fetch_one(&h.state.db).await.unwrap();
    connect(&mut h).await;
    let a = create_discount_code(&h.state, acct, &t, ann, "welcome").await.unwrap();
    // Another person with the same grant key gets their own code.
    let b = create_discount_code(&h.state, acct, &t, bob, "welcome").await.unwrap();
    assert_ne!(a, b);
    assert_eq!(mock.lock().unwrap().discount_inputs[0]["combinesWith"], json!({"orderDiscounts": true, "productDiscounts": true, "shippingDiscounts": true}));

    // Crash recovery accepts a code only if Shopify's discount under it has this template's terms.
    let other = DiscountTemplate { title: "Something else".into(), ..t.clone() };
    sqlx::query("DELETE FROM shopify_discount_codes").execute(&h.state.db).await.unwrap();
    {
        // Shopify has the code, but made from different terms.
        let mut m = mock.lock().unwrap();
        let d = m.discounts.get_mut(&a).unwrap();
        d["title"] = json!("Something else");
    }
    assert!(create_discount_code(&h.state, acct, &t, ann, "welcome").await.is_err(), "not this template's discount");
    let _ = other;

    // Reconnecting a different store never hands out the first store's code.
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    connect_shop(&mut h, "second-store.myshopify.com").await;
    let b2 = create_discount_code(&h.state, acct, &t, bob, "welcome").await.unwrap();
    assert_ne!(b2, b, "a new code for the new store");
    let shop: String = sqlx::query_scalar("SELECT shop_domain FROM shopify_discount_codes WHERE code = $1").bind(&b2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(shop, "second-store.myshopify.com");
    // A code row made for another grant is never returned for this one.
    sqlx::query("UPDATE shopify_discount_codes SET grant_key = 'someone-else' WHERE code = $1").bind(&b2).execute(&h.state.db).await.unwrap();
    assert!(create_discount_code(&h.state, acct, &t, bob, "welcome").await.is_err());
    h.cleanup().await;
}

#[tokio::test]
async fn privacy_webhooks() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    hook(&h, "orders/create", "o2", &order_body(5002, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;

    // Data request: an export the owner can download, and only for this customer.
    let req = json!({"shop_id": 1, "shop_domain": SHOP, "orders_requested": [5001], "customer": {"id": 1, "email": "ann@example.com", "phone": null}, "data_request": {"id": 77}});
    hook(&h, "customers/data_request", "p1", &req).await;
    hook(&h, "customers/data_request", "p1", &req).await; // redelivery
    h.drain().await;
    let v = h.ok("GET", "/api/shopify", None).await;
    let reqs = v["data_requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["shopify_customer_id"], "1");
    assert!(!v.to_string().contains("ann@example.com"), "status names no customer emails: {v}");
    let id = reqs[0]["id"].as_str().unwrap();
    let export = h.ok("GET", &format!("/api/shopify/data-requests/{id}"), None).await;
    assert_eq!(export["profiles"][0]["email"], "ann@example.com");
    assert!(export["events"].as_array().unwrap().len() >= 3);
    assert!(!export.to_string().contains("bob@example.com"));
    // The notice went to the account's Owner.
    let sent = h.state.email.sent();
    assert!(sent.iter().any(|e| e.to == "owner@acme.test" && e.subject.contains("asked your Shopify store")), "{:?}", sent.iter().map(|e| &e.to).collect::<Vec<_>>());
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
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'ann@example.com'").await, 0);
    assert!(event(&h, "Placed Order", "order:5001").await.is_none());
    assert!(event(&h, "Placed Order", "order:5002").await.is_some());
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_data_requests").await, 0, "the export made for them goes too");

    // Shop redact while still connected: ignored.
    hook(&h, "shop/redact", "p3", &json!({"shop_id": 1, "shop_domain": SHOP})).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_shops").await, 1);
    // Uninstalled from Shopify's side: the token goes at once.
    uninstall_in_shopify(&mock);
    hook(&h, "app/uninstalled", "u1", &json!({"id": 1, "domain": SHOP})).await;
    let (status, tok): (String, Option<String>) = sqlx::query_as("SELECT status, access_token_enc FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok), ("uninstalled", None));
    // Further webhooks are ignored.
    hook(&h, "orders/create", "o3", &order_body(5003, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5003").await.is_none());
    // 48 hours later, shop redact: everything from the store goes, including profiles it made that
    // have nothing else on them.
    hook(&h, "shop/redact", "p4", &json!({"shop_id": 1, "shop_domain": SHOP})).await;
    h.drain().await;
    for table in ["shopify_shops", "shopify_customers", "shopify_customer_profiles"] {
        assert_eq!(count(&h, &format!("SELECT count(*) FROM {table}")).await, 0, "{table}");
    }
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_webhooks WHERE payload IS NOT NULL").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'bob@example.com'").await, 0);
    assert!(event(&h, "Placed Order", "order:5002").await.is_none());
    h.cleanup().await;
}

/// Item 3: privacy requests act on Shopify's ids and the store's provenance, never on a reused
/// email; erasures stick; the store is found after its connection is gone.
#[tokio::test]
async fn privacy_requests_use_shopify_ids() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let acct = account_id(&h).await;
    connect(&mut h).await;
    // Someone who merely shares an address with a Shopify customer: their own profile, their own data.
    let shared: Uuid = sqlx::query_scalar("INSERT INTO profiles (id, account_id, email, properties) VALUES ($1, $2, 'shared@example.com', '{\"note\": \"mine\"}') RETURNING id").bind(Uuid::now_v7()).bind(acct).fetch_one(&h.state.db).await.unwrap();
    // Ann: a Shopify customer, also on a list of the account's own.
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    // A guest order (no customer) by someone else.
    let mut guest = order_body(5005, 0, "guest@example.com", chrono::Utc::now());
    guest["customer"] = Value::Null;
    hook(&h, "orders/create", "o2", &guest).await;
    h.drain().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter"}))).await;
    let ann = profile_id_of(&h, "ann@example.com").await.unwrap();
    sqlx::query("INSERT INTO list_members (list_id, profile_id) VALUES ($1, $2)").bind(list["id"].as_str().unwrap().parse::<Uuid>().unwrap()).bind(ann).execute(&h.state.db).await.unwrap();

    // The signed body must name the store in the header.
    let other = json!({"shop_id": 1, "shop_domain": "other.myshopify.com", "customer": {"id": 1}, "orders_to_redact": []});
    assert_eq!(hook(&h, "customers/redact", "x1", &other).await, StatusCode::BAD_REQUEST);
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_webhooks WHERE webhook_id = 'x1'").await, 0);

    // A request for a Shopify customer we never linked, with the shared address: nothing of theirs.
    hook(&h, "customers/data_request", "d1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 99, "email": "shared@example.com"}, "orders_requested": [], "data_request": {"id": 1}})).await;
    h.drain().await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM shopify_data_requests").fetch_one(&h.state.db).await.unwrap();
    let export = h.ok("GET", &format!("/api/shopify/data-requests/{id}"), None).await;
    assert_eq!(export["profiles"], json!([]), "a shared email isn't a match");
    hook(&h, "customers/redact", "r1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 99, "email": "shared@example.com"}, "orders_to_redact": []})).await;
    h.drain().await;
    let still: Value = sqlx::query_scalar("SELECT properties FROM profiles WHERE id = $1").bind(shared).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(still["note"], "mine");

    // Redact Ann, and the guest order by id: Shopify data goes; Ann's profile stays (it's on a list
    // of the account's own) without the Shopify properties; the guest's profile, made only for
    // that order, goes.
    // The shopping agent held a copy of Ann's order in a verified conversation.
    sqlx::query("INSERT INTO agent_conversations (id, account_id, profile_id, place, verified, system_prompt, history) VALUES ($1, $2, $3, 'hub', true, 'x', '[\"order 5001\"]')")
        .bind(Uuid::now_v7())
        .bind(acct)
        .bind(ann)
        .execute(&h.state.db)
        .await
        .unwrap();
    hook(&h, "customers/redact", "r2", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 1, "email": "ann@example.com"}, "orders_to_redact": [5001, 5005]})).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM agent_conversations").await, 0, "the agent's copy of the erased order goes too");
    assert!(event(&h, "Placed Order", "order:5001").await.is_none());
    assert!(event(&h, "Placed Order", "order:5005").await.is_none());
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_customers WHERE customer_id = 1").await, 0);
    let props: Value = sqlx::query_scalar("SELECT properties FROM profiles WHERE id = $1").bind(ann).fetch_one(&h.state.db).await.unwrap();
    assert!(props.get("shopify_customer_id").is_none() && props.get("city").is_none(), "{props}");
    assert!(profile_id_of(&h, "guest@example.com").await.is_none());

    // Erased stays erased: a late webhook and an import skip them.
    hook(&h, "orders/updated", "o3", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5001").await.is_none());
    mock.lock().unwrap().files.insert("customers".into(), jsonl(&[customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED")]));
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_customers WHERE customer_id = 1").await, 0);

    // A product from the store, then the store is uninstalled and its connection row is gone.
    mock.lock().unwrap().products.insert(101, product(101, "Mug", 1, "12.00"));
    hook(&h, "products/update", "pu", &json!({"id": 101})).await;
    hook(&h, "orders/create", "o4", &order_body(5006, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;
    uninstall_in_shopify(&mock);
    hook(&h, "app/uninstalled", "u1", &json!({})).await;
    sqlx::query("DELETE FROM shopify_shops").execute(&h.state.db).await.unwrap();
    // A body left unprocessed for 30 days is deleted and counted.
    hook(&h, "shop/redact", "s1", &json!({"shop_id": 1, "shop_domain": SHOP})).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5006").await.is_none(), "found the account through its past installs");
    let (s, _) = h.req("GET", "/api/catalog/products/101", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the store's products leave the catalog");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE id = $1".replace("$1", &format!("'{shared}'")).as_str()).await, 1, "the shared-address profile is untouched");
    h.cleanup().await;
}

/// Item 3 (retention) and item 9 (privacy first, failures retried without blocking the rest).
#[tokio::test]
async fn privacy_requests_are_retried_without_blocking_others() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    // One request that keeps failing (a database error on its export), and one behind it.
    sqlx::query(
        "CREATE FUNCTION fail_request() RETURNS trigger AS $$ BEGIN IF NEW.shopify_request_id = 666 THEN RAISE EXCEPTION 'boom'; END IF; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_request BEFORE INSERT ON shopify_data_requests FOR EACH ROW EXECUTE FUNCTION fail_request()").execute(&h.state.db).await.unwrap();
    hook(&h, "customers/data_request", "d1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 5}, "orders_requested": [], "data_request": {"id": 666}})).await;
    hook(&h, "customers/data_request", "d2", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 6}, "orders_requested": [], "data_request": {"id": 667}})).await;
    h.drain().await;
    let (attempts, next): (i32, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as("SELECT attempts, next_at FROM shopify_webhooks WHERE webhook_id = 'd1'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(attempts, 1);
    assert!(next.is_some_and(|n| n > chrono::Utc::now()), "retried later");
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_data_requests WHERE shopify_request_id = 667").await, 1, "the one behind it went ahead");
    // Failing for over 7 days: shown as a problem.
    sqlx::query("UPDATE shopify_webhooks SET received_at = now() - interval '8 days' WHERE webhook_id = 'd1'").execute(&h.state.db).await.unwrap();
    let v = h.ok("GET", "/api/shopify", None).await;
    assert!(v["problems"].to_string().contains("failing for over 7 days"), "{v}");
    // The worker does privacy requests first, even with no privacy job queued.
    sqlx::query("DROP TRIGGER fail_request ON shopify_data_requests").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE shopify_webhooks SET next_at = NULL WHERE webhook_id = 'd1'").execute(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM jobs WHERE kind = 'shopify_privacy'").execute(&h.state.db).await.unwrap();
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await;
    sqlx::query("DELETE FROM jobs WHERE kind = 'shopify_privacy'").execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_data_requests WHERE shopify_request_id = 666").await, 1);

    // Retention: a body never processed is deleted after 30 days and counted.
    sqlx::query("INSERT INTO shopify_webhooks (account_id, webhook_id, topic, payload, received_at, generation, shop_domain) SELECT account_id, 'old', 'orders/create', '{\"id\": 1}', now() - interval '31 days', generation, $1 FROM shopify_shops")
        .bind(SHOP)
        .execute(&h.state.db)
        .await
        .unwrap();
    tick(&h).await;
    // Clean-up is queued in the background lane; run just that job (the worker would process the
    // stale delivery itself otherwise).
    let lane: String = sqlx::query_scalar("SELECT lane FROM jobs WHERE kind = 'shopify_cleanup'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(lane, dumbmail::jobs::BACKGROUND);
    dumbmail::features::run_job(&h.state, "shopify_cleanup", &json!({})).await.unwrap().unwrap();
    let kept: Option<Value> = sqlx::query_scalar("SELECT payload FROM shopify_webhooks WHERE webhook_id = 'old'").fetch_one(&h.state.db).await.unwrap();
    assert!(kept.is_none());
    let v = h.ok("GET", "/api/shopify", None).await;
    assert_eq!(v["connection"]["dropped_payloads"], 1);
    // The export tool is not described as read-only.
    let tool = dumbmail::features::shopify::api::tools().into_iter().find(|t| t["name"] == "get_shopify_data_request").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    h.cleanup().await;
}

/// Item 8: work belongs to one install.
#[tokio::test]
async fn work_belongs_to_one_install() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    let (gen1, connected_at): (i32, chrono::DateTime<chrono::Utc>) = sqlx::query_as("SELECT generation, connected_at FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    // The setup job was queued with the connection, for this install.
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_installs").await, 1);

    // A worker run for this install holds the lease; then the app is reinstalled.
    let lease = dumbmail::features::shopify::shop::acquire(&h.state, acct, gen1).await.unwrap().unwrap();
    assert!(dumbmail::features::shopify::shop::still_mine(&h.state.db, &lease).await.unwrap());
    sqlx::query("UPDATE shopify_shops SET lease_until = NULL, lease_token = NULL").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    connect(&mut h).await;
    let gen2: i32 = sqlx::query_scalar("SELECT generation FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(gen2, gen1 + 1);
    // The old run can't write any more.
    assert!(!dumbmail::features::shopify::shop::still_mine(&h.state.db, &lease).await.unwrap());
    assert!(!dumbmail::features::shopify::shop::renew(&h.state, &lease).await.unwrap());
    // A delivery stored for the first install (it was in flight) is never processed for this one.
    sqlx::query("INSERT INTO shopify_webhooks (account_id, webhook_id, topic, payload, generation, shop_domain) VALUES ($1, 'old-gen', 'orders/create', $2, $3, $4)")
        .bind(acct)
        .bind(order_body(7001, 1, "old@example.com", chrono::Utc::now()))
        .bind(gen1)
        .bind(SHOP)
        .execute(&h.state.db)
        .await
        .unwrap();
    hook(&h, "orders/create", "new-gen", &order_body(7002, 1, "new@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:7001").await.is_none());
    assert!(event(&h, "Placed Order", "order:7002").await.is_some());

    // A delayed app/uninstalled from the first install (triggered before this install began) doesn't
    // disconnect the fresh one…
    let before = (connected_at + chrono::Duration::milliseconds(10)).to_rfc3339();
    hook_with(&h, "app/uninstalled", "u-old", &json!({}), &[("x-shopify-triggered-at", &before)]).await;
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], true);
    // …a real one does, once, however often it's delivered.
    let now = chrono::Utc::now().to_rfc3339();
    hook_with(&h, "app/uninstalled", "u-new", &json!({}), &[("x-shopify-triggered-at", &now)]).await;
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], false);
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_webhooks WHERE webhook_id = 'u-new'").await, 1);
    connect(&mut h).await;
    hook_with(&h, "app/uninstalled", "u-new", &json!({}), &[("x-shopify-triggered-at", &chrono::Utc::now().to_rfc3339())]).await;
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], true, "a redelivered uninstall does nothing");

    // A refused webhook subscription: connected, but shown as not receiving it, and setup retries.
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    mock.lock().unwrap().refuse_topics.insert("REFUNDS_CREATE".into());
    connect(&mut h).await;
    let v = h.ok("GET", "/api/shopify", None).await;
    assert_eq!(v["health"]["state"], "incomplete", "{v}");
    assert!(v["health"]["message"].as_str().unwrap().contains("refunds/create"));
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_imports WHERE generation = (SELECT generation FROM shopify_shops) AND kind = 'history'").await, 0, "no import until setup is complete");
    mock.lock().unwrap().refuse_topics.clear();
    sqlx::query("UPDATE shopify_shops SET setup_after = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.fast_forward().await;
    settle_imports(&h).await;
    let v = h.ok("GET", "/api/shopify", None).await;
    assert_ne!(v["health"]["state"], "incomplete", "{v}");
    assert_eq!(v["connection"]["missing_topics"], json!([]));
    assert!(mock.lock().unwrap().webhooks_created.iter().filter(|t| *t == "REFUNDS_CREATE").count() >= 1);
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_imports WHERE generation = (SELECT generation FROM shopify_shops) AND kind = 'history'").await, 1);
    h.cleanup().await;
}

/// Item 10: disconnect clears the embed's settings and reports what Shopify didn't confirm.
#[tokio::test]
async fn disconnect_reports_unfinished_cleanup() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    mock.lock().unwrap().pixel_delete_fails = true;
    let v = h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    assert_eq!(v["cleaned_up"], false);
    assert_eq!(v["not_removed"], json!(["the checkout pixel"]));
    assert_eq!(mock.lock().unwrap().metafield_deletes, 1);
    let s = h.ok("GET", "/api/shopify", None).await;
    assert!(s["connection"]["cleanup_error"].as_str().unwrap().contains("the checkout pixel"));
    h.cleanup().await;
}

/// Item 12: a result file must answer the byte range asked for; one that ignores ranges is read
/// once by skipping; a full product scan unpublishes products the store no longer has, and an
/// older snapshot never brings back one a webhook updated or deleted since.
#[tokio::test]
async fn bulk_downloads_and_product_scans() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let old = "2025-01-01T00:00:00Z";
    let prod = |p: i64, at: &str, price: &str| vec![
        json!({"id": format!("gid://shopify/Product/{p}"), "updatedAt": at, "title": format!("P{p}"), "handle": format!("p{p}"), "status": "ACTIVE", "publishedAt": old, "tags": []}),
        json!({"id": format!("gid://shopify/ProductVariant/{p}1"), "title": "Default Title", "price": price, "inventoryQuantity": 1, "availableForSale": true, "inventoryItem": {"id": format!("gid://shopify/InventoryItem/{p}9"), "tracked": true}, "__parentId": format!("gid://shopify/Product/{p}")}),
    ];
    // A first scan with three products.
    mock.lock().unwrap().files.insert("products".into(), jsonl(&[prod(1, old, "1.00"), prod(2, old, "2.00"), prod(3, old, "3.00")].concat()));
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/3", None).await["published"], true);
    // Since then: product 1 was updated (webhook read, newer), product 2 deleted, product 3 removed.
    let mut p1 = product(1, "P1", 1, "9.00");
    p1["updatedAt"] = json!("2026-06-01T00:00:00Z");
    mock.lock().unwrap().products.insert(1, p1);
    hook(&h, "products/update", "pu1", &json!({"id": 1})).await;
    hook(&h, "products/delete", "pd2", &json!({"id": 2})).await;
    h.drain().await;
    // A scan whose snapshot is older than those changes, without product 3.
    mock.lock().unwrap().files.insert("products".into(), jsonl(&[prod(1, old, "1.00"), prod(2, old, "2.00")].concat()));
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/1", None).await["price"], 9.0, "the older snapshot didn't overwrite the webhook's");
    assert_eq!(h.ok("GET", "/api/catalog/products/2", None).await["published"], false, "deleted stays deleted");
    assert_eq!(h.ok("GET", "/api/catalog/products/3", None).await["published"], false, "gone from the store: unpublished");

    // A server that ignores Range: read once, by skipping.
    {
        let mut m = mock.lock().unwrap();
        m.files.insert("products".into(), jsonl(&[prod(4, "2026-07-01T00:00:00Z", "4.00"), prod(5, "2026-07-01T00:00:00Z", "5.00")].concat()));
        m.ignore_range = true;
    }
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    assert_eq!(h.ok("GET", "/api/shopify/import", None).await["products_sync"]["status"], "done");
    assert_eq!(h.ok("GET", "/api/catalog/products/5", None).await["price"], 5.0);
    // A partial answer that doesn't start where asked: the import fails with a plain error.
    {
        let mut m = mock.lock().unwrap();
        m.ignore_range = false;
        m.wrong_range = true;
        m.max_chunk = 150;
    }
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    let v = h.ok("GET", "/api/shopify/import", None).await;
    assert_eq!(v["products_sync"]["status"], "failed", "{v}");
    assert!(v["products_sync"]["error"].as_str().unwrap().contains("wrong byte range"));
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
    mock.lock().unwrap().token = "shpat_rotated_elsewhere".into();
    hook(&h, "customers/update", "c2", &json!({"id": 1})).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().refreshes, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn one_worker_per_account_on_the_background_lane() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    let reads = mock.lock().unwrap().node_reads.len();
    // Item 6: every Shopify job goes to the background lane; the lane sending uses never runs them.
    hook(&h, "customers/create", "c1", &json!({"id": 1})).await;
    hook(&h, "customers/data_request", "d1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 9}, "orders_requested": [], "data_request": {"id": 1}})).await;
    tick(&h).await;
    let lanes: Vec<(String, String)> = sqlx::query_as("SELECT DISTINCT kind, lane FROM jobs WHERE kind LIKE 'shopify%' ORDER BY 1").fetch_all(&h.state.db).await.unwrap();
    assert!(!lanes.is_empty());
    assert!(lanes.iter().all(|(_, l)| l == dumbmail::jobs::BACKGROUND), "{lanes:?}");
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::MAIN).await.unwrap();
    assert_eq!(mock.lock().unwrap().node_reads.len(), reads, "the main lane doesn't run Shopify work");
    let lanes_used: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_lanes WHERE account_id IS NOT NULL)").fetch_one(&h.state.db).await.unwrap();
    assert!(!lanes_used, "the app's own lane table is no longer used");
    // The account's own lease held by another worker: this one exits without doing anything.
    sqlx::query("UPDATE shopify_shops SET lease_until = now() + interval '1 minute', lease_token = gen_random_uuid()").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    assert_eq!(mock.lock().unwrap().node_reads.len(), reads);
    // The other worker died: once its lease runs out, the scheduler's safety net picks the work up.
    sqlx::query("UPDATE shopify_shops SET lease_until = now() - interval '1 second', work_queued_at = now() - interval '5 minutes'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert_eq!(mock.lock().unwrap().node_reads.len(), reads + 1);
    h.cleanup().await;
}

// ---------- The storefront extensions (run with Node, as Shopify would run them in a browser) ----------

fn node_available() -> bool {
    std::process::Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn run_node(script: &str) -> String {
    let dir = std::env::temp_dir().join(format!("dm-shopify-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.mjs");
    std::fs::write(&file, script).unwrap();
    let out = std::process::Command::new("node").arg(&file).output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(out.status.success(), "node failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Item 14: the pixel sends nothing it can't match to Shopify's own copy (no order id or token).
#[test]
fn pixel_skips_events_it_cannot_dedupe() {
    if !node_available() {
        eprintln!("node not installed; skipping");
        return;
    }
    let src = include_str!("../connectors/shopify/extensions/dumbmail-pixel/src/index.js").replace("import { register } from \"@shopify/web-pixels-extension\";", "");
    let script = format!(
        r#"
const posts = [];
globalThis.fetch = (url, opts) => {{ posts.push(JSON.parse(opts.body)); return Promise.resolve({{}}); }};
const store = () => {{ const m = new Map(); return {{ getItem: async (k) => m.has(k) ? m.get(k) : null, setItem: async (k, v) => m.set(k, v), removeItem: async (k) => m.delete(k) }}; }};
const subs = {{}};
function register(fn) {{ fn({{ analytics: {{ subscribe: (n, cb) => subs[n] = cb }}, browser: {{ localStorage: store(), sessionStorage: store() }}, settings: {{ base: "https://dm.test", key: "pk" }},
  init: {{ customerPrivacy: {{ analyticsProcessingAllowed: true, marketingAllowed: true }} }}, customerPrivacy: {{ subscribe: () => {{}} }} }}); }}
{src}
const checkout = (extra) => ({{ data: {{ checkout: Object.assign({{ email: "ann@example.com", lineItems: [], totalPrice: {{ amount: 10, currencyCode: "USD" }} }}, extra) }}, context: {{ document: {{ location: {{ href: "https://shop.test/c", origin: "https://shop.test" }} }} }} }});
await subs.checkout_completed(checkout({{ order: {{}} }}));
await subs.checkout_started(checkout({{}}));
await new Promise((r) => setTimeout(r, 10));
const before = posts.length;
await subs.checkout_completed(checkout({{ order: {{ id: "gid://shopify/Order/5" }} }}));
await new Promise((r) => setTimeout(r, 10));
console.log(JSON.stringify({{ before, after: posts.map((p) => p.unique_id) }}));
"#
    );
    let out: Value = serde_json::from_str(&run_node(&script)).unwrap();
    assert_eq!(out["before"], 0, "no order id, no checkout token: nothing sent");
    assert_eq!(out["after"], json!(["order:5"]));
}

/// Item 10: the theme embed loads the snippet only once Shopify's Customer Privacy API allows
/// analytics, with email capture off unless marketing is allowed too, and loads it when consent
/// arrives later.
#[test]
fn theme_embed_waits_for_consent() {
    if !node_available() {
        eprintln!("node not installed; skipping");
        return;
    }
    let liquid = include_str!("../connectors/shopify/extensions/dumbmail-embed/blocks/dumbmail.liquid");
    let js = liquid.split("<script>").nth(1).unwrap().split("</script>").next().unwrap().replace("{{ dumbmail_src | json }}", "\"https://dm.test/onsite.js?k=pk\"");
    let script = format!(
        r#"
function run(consent, apiLoads) {{
  const added = [];
  const listeners = {{}};
  globalThis.document = {{
    createElement: () => ({{ attrs: {{}}, setAttribute(k, v) {{ this.attrs[k] = v; }} }}),
    head: {{ appendChild: (s) => added.push(s) }},
    addEventListener: (n, cb) => listeners[n] = cb,
  }};
  globalThis.window = {{ Shopify: apiLoads ? {{
    loadFeatures: (f, cb) => {{ window.Shopify.customerPrivacy = {{ analyticsProcessingAllowed: () => consent.analytics, marketingAllowed: () => consent.marketing }}; cb(false); }},
  }} : {{}} }};
  {js}
  return {{ added, listeners }};
}}
const none = run({{ analytics: false, marketing: false }}, true);
const before = none.added.length;
none.listeners.visitorConsentCollected({{ detail: {{ analyticsAllowed: true, marketingAllowed: false }} }});
const analyticsOnly = run({{ analytics: true, marketing: false }}, true);
const both = run({{ analytics: true, marketing: true }}, true);
const noApi = run({{ analytics: true, marketing: true }}, false);
console.log(JSON.stringify({{
  before, later: none.added.map((s) => s.attrs["data-capture-emails"] || "on"),
  analyticsOnly: analyticsOnly.added.map((s) => s.attrs["data-capture-emails"] || "on"),
  both: both.added.map((s) => s.attrs["data-capture-emails"] || "on"),
  noApi: noApi.added.length,
}}));
"#
    );
    let out: Value = serde_json::from_str(&run_node(&script)).unwrap();
    assert_eq!(out["before"], 0, "no analytics consent: not loaded");
    assert_eq!(out["later"], json!(["false"]), "loaded when consent arrives, without email capture");
    assert_eq!(out["analyticsOnly"], json!(["false"]));
    assert_eq!(out["both"], json!(["on"]));
    assert_eq!(out["noApi"], 0, "without the Customer Privacy API, nothing loads");
}

// ---------- Wiring round ----------

/// A browser event through the public endpoint; its status.
async fn browser(h: &Harness, pk: &str, metric: &str) -> StatusCode {
    let body = json!({"metric": metric, "profile": {"email": "ann@example.com"}, "value": 5}).to_string();
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &body).await.0
}

/// Item 3: connecting makes the app's metrics server-only for that account only, and they stay so
/// after a disconnect; other accounts keep sending the same names from their browsers.
#[tokio::test]
async fn shopify_metrics_are_server_only_per_account() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts").fetch_one(&h.state.db).await.unwrap();
    // Before connecting, a custom browser metric with one of these names is accepted, as today.
    assert_eq!(browser(&h, &pk, "Refunded Order").await, StatusCode::OK);
    connect(&mut h).await;
    for m in ["Placed Order", "Started Checkout", "Refunded Order", "Fulfilled Order", "Cancelled Order"] {
        assert_eq!(browser(&h, &pk, m).await, StatusCode::BAD_REQUEST, "{m}");
    }
    let flagged = count(&h, "SELECT count(*) FROM metrics WHERE server_only AND name IN ('Placed Order', 'Started Checkout', 'Ordered Product', 'Fulfilled Order', 'Fulfilled Partial Order', 'Cancelled Order', 'Refunded Order')").await;
    assert_eq!(flagged, 7);
    // Disconnecting leaves them server-only.
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    assert_eq!(browser(&h, &pk, "Placed Order").await, StatusCode::BAD_REQUEST);
    // Another account without the app keeps sending them from its site.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "B"}))).await;
    let other: String = sqlx::query_scalar("SELECT public_key FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(browser(&h, &other, "Placed Order").await, StatusCode::OK);
    assert_eq!(browser(&h, &other, "Refunded Order").await, StatusCode::OK);
    h.cleanup().await;
}

/// Wiring item 4 and item 12: segment entries caused by history never start flows, whenever the
/// history was written; real entries start flows while an import runs, is paused, or just finished.
#[tokio::test]
async fn history_entries_never_start_flows_and_real_ones_always_do() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Buyers", "definition": {"match": "all", "conditions": [{"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1}]}}))).await;
    let seg_id: Uuid = seg["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", "/api/flows", Some(json!({"name": "Welcome buyers", "trigger": {"type": "segment", "segment_id": seg_id.to_string()}, "definition": {"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    sqlx::query("UPDATE flows SET status = 'live' WHERE name NOT IN ('Order confirmed', 'Order shipped')").execute(&h.state.db).await.unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    // History: an order the import brings in. Its buyer enters the segment, quietly, whenever the
    // refresh runs (here: after the import finished).
    mock.lock().unwrap().files.insert("orders".into(), jsonl(&[bulk_order(5001, "old@example.com", json!([])), bulk_line(5001, 1)]));
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    assert_eq!(flow_runs(&h).await, 0, "history starts no flow");
    // A real order right after the import finished starts the flow (nothing is absorbed).
    hook(&h, "orders/create", "o1", &order_body(5002, 7, "live@example.com", chrono::Utc::now())).await;
    h.drain().await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    assert_eq!(flow_runs(&h).await, 1, "a real entry after the import starts the flow");
    // An import started and paused: a real order meanwhile still starts the flow.
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    h.ok("POST", "/api/shopify/import/pause", None).await;
    hook(&h, "orders/create", "o2", &order_body(5003, 8, "during@example.com", chrono::Utc::now())).await;
    h.drain().await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    assert_eq!(flow_runs(&h).await, 2, "a real entry during a paused import starts the flow");
    // The race the old flag missed: a refresh that read "nothing to absorb" before a history page
    // committed, and saw that page's buyer as a new member. Here: the history is in, nothing is
    // flagged to absorb any more, and the buyer reaches a live refresh as a new member.
    h.ok("POST", "/api/shopify/import/cancel", None).await;
    mock.lock().unwrap().files.insert("orders".into(), jsonl(&[bulk_order(5004, "late@example.com", json!([])), bulk_line(5004, 4)]));
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    let late = profile_id_of(&h, "late@example.com").await.unwrap();
    sqlx::query("DELETE FROM segment_members WHERE profile_id = $1").bind(late).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE segments SET absorb_entries = false").execute(&h.state.db).await.unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    assert_eq!(flow_runs(&h).await, 2, "history's buyer starts no flow, however the refresh and the import interleaved");
    h.cleanup().await;
}

/// Item 5: people the store brings in without marketing consent are held; existing profiles never
/// are; only service-message flows reach a held person's store-supplied address.
#[tokio::test]
async fn people_without_marketing_consent_are_held() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    let acct = account_id(&h).await;
    sqlx::query("INSERT INTO profiles (id, account_id, email) VALUES ($1, $2, 'existing@example.com')").bind(Uuid::now_v7()).bind(acct).execute(&h.state.db).await.unwrap();
    connect(&mut h).await;
    {
        let mut m = mock.lock().unwrap();
        m.customers.insert(1, customer(1, Some("sub@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
        m.customers.insert(2, customer(2, Some("existing@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    }
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    hook(&h, "customers/update", "c2", &json!({"id": 2})).await;
    hook(&h, "orders/create", "o1", &order_body(5001, 3, "buyer@example.com", chrono::Utc::now())).await;
    h.drain().await;
    let held = |email: &'static str| {
        let db = h.state.db.clone();
        async move { sqlx::query_as::<_, (bool, Option<String>)>("SELECT marketing_hold, email_source FROM profiles WHERE email = $1").bind(email).fetch_one(&db).await.unwrap() }
    };
    let (hold, source) = held("buyer@example.com").await;
    assert!(hold, "made from an order, no opt-in: held");
    assert_eq!(source.as_deref(), Some("integration:shopify:acme-goods.myshopify.com"));
    assert!(!held("sub@example.com").await.0, "subscribed in Shopify: not held");
    assert!(!held("existing@example.com").await.0, "a profile that already existed is never held");
    // A service-message flow may email the held buyer at the store's address; a marketing one may not.
    let mk = |service: bool| json!({"name": if service { "Shipping" } else { "Winback" }, "trigger": {"type": "metric", "metric": "Fulfilled Order"},
        "definition": {"entry": "d", "settings": {"service_messages": service}, "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]}});
    let service: Uuid = h.ok("POST", "/api/flows", Some(mk(true))).await["id"].as_str().unwrap().parse().unwrap();
    let marketing: Uuid = h.ok("POST", "/api/flows", Some(mk(false))).await["id"].as_str().unwrap().parse().unwrap();
    let buyer = profile_id_of(&h, "buyer@example.com").await.unwrap();
    assert!(!dumbmail::marketing_hold::blocks_email(&h.state.db, buyer, Some(service)).await.unwrap());
    assert!(dumbmail::marketing_hold::blocks_email(&h.state.db, buyer, Some(marketing)).await.unwrap());
    assert!(dumbmail::marketing_hold::blocks_email(&h.state.db, buyer, None).await.unwrap(), "campaigns skip them");
    h.cleanup().await;
}

/// Item 7: a coupon linked to the store gives each person their own Shopify code through the
/// coupons module's one path.
#[tokio::test]
async fn coupons_get_codes_from_the_store() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    h.credit(5.0).await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    let c = h.ok("POST", "/api/coupons", Some(json!({"name": "WELCOME", "description": "10% off"}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s, _) = h.req("PUT", &format!("/api/shopify/coupons/{cid}"), Some(json!({"title": "Welcome 10%", "value": {"percentage": 150}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the template is checked");
    h.ok("PUT", &format!("/api/shopify/coupons/{cid}"), Some(json!({"title": "Welcome 10%", "value": {"percentage": 10}, "prefix": "HI"}))).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let lid: Uuid = list["id"].as_str().unwrap().parse().unwrap();
    for e in ["ann@x.test", "bob@x.test"] {
        let p = Uuid::now_v7();
        sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, $3, 'subscribed')").bind(p).bind(acct).bind(e).execute(&h.state.db).await.unwrap();
        sqlx::query("INSERT INTO list_members (list_id, profile_id) VALUES ($1, $2)").bind(lid).bind(p).execute(&h.state.db).await.unwrap();
    }
    let content = json!({"subject": "A code", "blocks": [{"type": "text", "html": "<p>CODE[{% coupon_code 'WELCOME' %}]</p>"}]});
    let camp = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "email", "audience": {"include": [{"type": "list", "id": lid}]}, "content": content}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", camp["id"].as_str().unwrap()), None).await;
    for _ in 0..5 {
        h.fast_forward().await;
    }
    let code_for = |to: &str| {
        let m = h.state.email.sent().into_iter().rfind(|m| m.to == to).expect("sent");
        let i = m.html.find("CODE[").unwrap() + 5;
        m.html[i..].split(']').next().unwrap().to_string()
    };
    let (a, b) = (code_for("ann@x.test"), code_for("bob@x.test"));
    assert!(a.starts_with("HI") && b.starts_with("HI") && a != b, "{a} {b}");
    let made: HashSet<String> = mock.lock().unwrap().discounts.keys().cloned().collect();
    assert!(made.contains(&a) && made.contains(&b), "real Shopify codes");
    let via: Vec<String> = sqlx::query_scalar("SELECT assigned_via FROM coupon_codes WHERE coupon_id = $1::uuid").bind(&cid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(via, vec!["store", "store"], "saved as each person's code in the coupon");
    let shop_rows = count(&h, "SELECT count(*) FROM shopify_discount_codes WHERE grant_key = 'coupon' AND shop_domain = 'acme-goods.myshopify.com'").await;
    assert_eq!(shop_rows, 2, "keyed by store, template, person and grant");
    h.cleanup().await;
}

/// Item 8: permissions as code, and the audit log.
#[tokio::test]
async fn shopify_permissions_and_audit() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    hook(&h, "customers/data_request", "d1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 9}, "orders_requested": [], "data_request": {"id": 1}})).await;
    h.drain().await;
    let req_id: Uuid = sqlx::query_scalar("SELECT id FROM shopify_data_requests").fetch_one(&h.state.db).await.unwrap();
    // A manager sees the connection but can't change it or read an export.
    let (_, mgr) = teammate(&h, "manager").await;
    let mine = h.cookie.replace(mgr);
    assert_eq!(h.req("GET", "/api/shopify", None).await.0, StatusCode::OK);
    assert_eq!(h.req("GET", "/api/shopify/import", None).await.0, StatusCode::OK);
    for (m, p, b) in [
        ("POST", "/api/shopify/connect".to_string(), Some(json!({"shop": SHOP}))),
        ("POST", "/api/shopify/disconnect".to_string(), Some(json!({"confirm": true}))),
        ("POST", "/api/shopify/import".to_string(), Some(json!({"confirm": true}))),
        ("POST", "/api/shopify/import/cancel".to_string(), None),
        ("POST", "/api/shopify/products/resync".to_string(), None),
        ("GET", format!("/api/shopify/data-requests/{req_id}"), None),
    ] {
        assert_eq!(h.req(m, &p, b).await.0, StatusCode::FORBIDDEN, "{m} {p}");
    }
    let me: Value = h.ok("GET", "/api/me", None).await;
    assert!(!me.to_string().contains("shopify.manage"), "the dashboard is told the manager can't");
    h.cookie = mine;
    let me: Value = h.ok("GET", "/api/me", None).await;
    assert!(me.to_string().contains("shopify.manage"));
    // The owner's actions are recorded, without tokens or customer data.
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    h.ok("POST", "/api/shopify/import/cancel", None).await;
    h.ok("GET", &format!("/api/shopify/data-requests/{req_id}"), None).await;
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'shopify.%' ORDER BY created_at").fetch_all(&h.state.db).await.unwrap();
    for a in ["shopify.connect", "shopify.import_start", "shopify.import_cancel", "shopify.privacy_export", "shopify.disconnect"] {
        assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
    }
    let details: Vec<Value> = sqlx::query_scalar("SELECT details FROM audit_log WHERE action LIKE 'shopify.%'").fetch_all(&h.state.db).await.unwrap();
    assert!(!format!("{details:?}").contains("shpat_"));
    h.cleanup().await;
}

/// Item 10: a server that ignores byte ranges is skipped through only up to the limit.
#[tokio::test]
async fn bulk_download_skip_limit() {
    let (mock, base) = mock_shopify().await;
    let h = shopify_harness!(base);
    {
        let mut m = mock.lock().unwrap();
        m.files.insert("products".into(), "0123456789\n".repeat(10));
        m.ignore_range = true;
    }
    let url = format!("{base}/files/products.jsonl");
    let (bytes, _) = dumbmail::features::shopify::client::result_chunk(&h.state, &url, 22, 11).await.unwrap().unwrap();
    assert_eq!(bytes, b"0123456789\n", "skipped to the offset");
    let past = dumbmail::features::shopify::client::MAX_SKIP + 1;
    let err = dumbmail::features::shopify::client::result_chunk(&h.state, &url, past, 11).await.unwrap_err();
    assert!(err.to_string().contains("ignored byte ranges"), "{err}");
    h.cleanup().await;
}

/// Item 10: withdrawing consent after the snippet loaded stops it: nothing more is sent, and events
/// waiting to be sent are deleted.
#[tokio::test]
async fn snippet_stops_when_consent_is_withdrawn() {
    if !node_available() {
        eprintln!("node not installed; skipping");
        return;
    }
    let mut h = harness!();
    h.signup().await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts").fetch_one(&h.state.db).await.unwrap();
    // The snippet exactly as the server serves it.
    let (s, _, onsite) = h.raw("GET", &format!("/onsite.js?k={pk}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    h.cleanup().await;
    let liquid = include_str!("../connectors/shopify/extensions/dumbmail-embed/blocks/dumbmail.liquid");
    let embed = liquid.split("<script>").nth(1).unwrap().split("</script>").next().unwrap().replace("{{ dumbmail_src | json }}", "\"https://dm.test/onsite.js?k=pk\"");
    let script = format!(
        r#"
const posts = [];
const storage = () => {{ const m = new Map(); return {{ getItem: (k) => m.has(k) ? m.get(k) : null, setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k), _m: m }}; }};
const ls = storage(), ss = storage();
const listeners = {{}};
const el = () => new Proxy({{
  attrs: {{}}, style: {{}}, dataset: {{}}, children: [], childNodes: [], textContent: "", innerHTML: "", className: "", value: "",
  classList: {{ contains: () => false, add: () => {{}}, remove: () => {{}}, toggle: () => {{}} }},
  setAttribute(k, v) {{ this.attrs[k] = v; }}, getAttribute(k) {{ return this.attrs[k] ?? null; }}, hasAttribute(k) {{ return k in this.attrs; }},
  querySelector: () => null, querySelectorAll: () => [], getElementsByTagName: () => [], closest: () => null, matches: () => false,
  appendChild: (c) => c, insertBefore: (c) => c, removeChild: (c) => c, remove: () => {{}}, addEventListener: () => {{}}, removeEventListener: () => {{}},
  attachShadow: () => el(), getBoundingClientRect: () => ({{ top: 0, left: 0, width: 0, height: 0 }}),
}}, {{ get: (t, k) => k in t ? t[k] : (typeof k === "symbol" ? undefined : (() => null)) }});
const script = el();
script.attrs.src = "https://dm.test/onsite.js?k=pk";
script.src = "https://dm.test/onsite.js?k=pk";
globalThis.localStorage = ls; globalThis.sessionStorage = ss;
globalThis.location = {{ href: "https://shop.test/products/mug", origin: "https://shop.test", pathname: "/products/mug", search: "", hostname: "shop.test", protocol: "https:" }};
Object.defineProperty(globalThis, "navigator", {{ value: {{ userAgent: "node", sendBeacon: () => true }}, configurable: true }});
globalThis.fetch = (url, opts) => {{ if (opts && opts.method === "POST") posts.push(String(url)); return Promise.resolve({{ ok: true, json: () => Promise.resolve([]) }}); }};
globalThis.document = new Proxy({{
  currentScript: script, readyState: "complete", head: {{ appendChild: (s) => {{ added.push(s); }} }}, body: el(), documentElement: el(),
  createElement: () => el(), querySelector: () => null, querySelectorAll: () => [], getElementById: () => null, getElementsByTagName: () => [],
  removeEventListener: () => {{}}, visibilityState: "visible", hidden: false,
  addEventListener: (n, cb) => {{ listeners[n] = cb; }}, cookie: "", title: "Mug", referrer: "",
}}, {{ get: (t, k) => k in t ? t[k] : undefined }});
const added = [];
globalThis.window = globalThis;
window.addEventListener = () => {{}}; window.removeEventListener = () => {{}};
globalThis.history = {{ pushState: () => {{}}, replaceState: () => {{}} }};
globalThis.MutationObserver = class {{ observe() {{}} disconnect() {{}} }};
globalThis.IntersectionObserver = class {{ observe() {{}} disconnect() {{}} }};
globalThis.requestAnimationFrame = (f) => setTimeout(f, 0);
globalThis.getComputedStyle = () => ({{}}); globalThis.innerWidth = 1200; globalThis.innerHeight = 800; globalThis.scrollY = 0;
globalThis.Intl = Intl;
window.setTimeout = setTimeout; window.setInterval = () => 0;
window.Shopify = {{ loadFeatures: (f, cb) => {{ window.Shopify.customerPrivacy = {{ analyticsProcessingAllowed: () => true, marketingAllowed: () => true }}; cb(false); }} }};
{embed}
// The embed added the snippet: run it as the browser would.
(new Function({onsite_json}))();
window.dumbmail.identify({{ email: "ann@example.com" }});
window.dumbmail.track("Viewed Thing", {{}});
const before = posts.length;
ls.setItem("pl_buffer", JSON.stringify([{{ metric: "Held", time: Date.now() }}]));
listeners.visitorConsentCollected({{ detail: {{ analyticsAllowed: false, marketingAllowed: false }} }});
window.dumbmail.track("After Stop", {{}});
window.dumbmail.identify({{ email: "bob@example.com" }});
setTimeout(() => console.log(JSON.stringify({{ added: added.length, before, after: posts.length - before, buffer: ls.getItem("pl_buffer"), profile: ls.getItem("pl_profile") }})), 20);
"#,
        onsite_json = serde_json::to_string(&onsite).unwrap()
    );
    let out: Value = serde_json::from_str(&run_node(&script)).unwrap();
    assert_eq!(out["added"], 1, "the embed loaded the snippet");
    assert!(out["before"].as_i64().unwrap() >= 1, "it was sending before: {out}");
    assert_eq!(out["after"], 0, "nothing is sent after consent is withdrawn: {out}");
    assert_eq!(out["buffer"], Value::Null, "queued events are deleted");
    assert_eq!(out["profile"], Value::Null, "and who it thought the visitor was");
}

/// Item 9: a closed account gets only privacy requests; no other Shopify work runs for it.
#[tokio::test]
async fn closed_accounts_only_get_privacy_requests() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    // Work already queued, then the account is closed.
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    sqlx::query("UPDATE accounts SET closed_at = now() WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    let queries = mock.lock().unwrap().queries;
    h.fast_forward().await;
    assert_eq!(mock.lock().unwrap().queries, queries, "no Shopify work runs for a closed account");
    // New deliveries are acknowledged and dropped…
    assert_eq!(hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now())).await, StatusCode::OK);
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_webhooks WHERE webhook_id = 'o1'").await, 0);
    // …except privacy requests, which are still honoured.
    assert_eq!(hook(&h, "customers/data_request", "d1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 1}, "orders_requested": [], "data_request": {"id": 5}})).await, StatusCode::OK);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_data_requests WHERE shopify_request_id = 5").await, 1);
    // An unsubscribe made before closing is still written back to the store.
    sqlx::query("INSERT INTO shopify_customers (account_id, customer_id, email, email_state, shop_domain) VALUES ($1, 2, 'bob@example.com', 'SUBSCRIBED', $2)").bind(acct).bind(SHOP).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, 'bob@example.com', 'unsubscribed')").bind(Uuid::now_v7()).bind(acct).execute(&h.state.db).await.unwrap();
    tick(&h).await;
    h.drain().await;
    assert!(mock.lock().unwrap().consent_updates.iter().any(|u| u["customerId"] == "gid://shopify/Customer/2"), "opt-outs still reach the store");
    // Uninstalling the app still forgets the token and releases the store.
    uninstall_in_shopify(&mock);
    hook(&h, "app/uninstalled", "u1", &json!({})).await;
    let (status, tok): (String, Option<String>) = sqlx::query_as("SELECT status, access_token_enc FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok), ("uninstalled", None));
    h.cleanup().await;
}

// ---------- Before switch-on ----------

/// Item 1: the event's own email and phone come first; the nested customer's contact only when the
/// event has none, and then nothing is sent because of the event.
#[tokio::test]
async fn own_contact_before_the_customer_record() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    live_flow(&mut h, "Placed Order").await;
    mock.lock().unwrap().customers.insert(1, customer(1, Some("ann@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    h.drain().await;
    let ann = profile_id_of(&h, "ann@example.com").await.unwrap();
    // A new phone on the order, and only the stale email on the customer record: the phone wins.
    let mut o = order_body(5001, 1, "", chrono::Utc::now());
    o["email"] = Value::Null;
    o["phone"] = json!("+15035550123");
    o["customer"] = json!({"id": 1, "email": "ann@example.com"});
    hook(&h, "orders/create", "o1", &o).await;
    h.drain().await;
    let (_, props, _, owner) = event(&h, "Placed Order", "order:5001").await.unwrap();
    let by_phone: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = '+15035550123'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(owner, by_phone, "the order's own phone, not the customer record's email");
    assert_ne!(owner, ann);
    assert!(props.get("$no_contact").is_none());
    assert_eq!(flow_runs(&h).await, 1);
    // A checkout with only a shipping phone of its own: that phone.
    let checkout = json!({"token": "tok1", "email": null, "shipping_address": {"phone": "+15035550124"}, "completed_at": null, "created_at": chrono::Utc::now().to_rfc3339(),
        "currency": "USD", "total_price": "10.00", "customer": {"id": 1, "email": "ann@example.com"}, "line_items": []});
    hook(&h, "checkouts/create", "k1", &checkout).await;
    h.drain().await;
    let by_ship: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = '+15035550124'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(event(&h, "Started Checkout", "checkout:tok1").await.unwrap().3, by_ship);
    // No contact on the order itself, only on the customer record: recorded, marked, starts nothing.
    let mut bare = order_body(5002, 2, "", chrono::Utc::now());
    bare["email"] = Value::Null;
    bare["customer"] = json!({"id": 2, "email": "cust-only@example.com"});
    hook(&h, "orders/create", "o2", &bare).await;
    h.drain().await;
    let (_, props, _, _) = event(&h, "Placed Order", "order:5002").await.unwrap();
    assert_eq!(props["$no_contact"], true);
    assert_eq!(flow_runs(&h).await, 1, "an order without its own contact starts nothing");
    h.cleanup().await;
}

/// Item 2: an opt-out survives a crash between recording Shopify's consent time and changing the
/// profile; an order's re-link isn't undone by a snapshot no newer than the order.
#[tokio::test]
async fn opt_out_survives_a_crash_and_relinks_stick() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    mock.lock().unwrap().customers.insert(1, customer_at(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED", "2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z"));
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    h.drain().await;
    assert_eq!(consent(&h, "ann@example.com").await.0, "subscribed");
    // The next write that unsubscribes a profile fails once (the crash in the gap).
    sqlx::query("CREATE TABLE crash_once (n int)").execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO crash_once VALUES (1)").execute(&h.state.db).await.unwrap();
    sqlx::query(
        "CREATE FUNCTION crash_unsub() RETURNS trigger AS $$ BEGIN
           IF NEW.email_consent = 'unsubscribed' AND OLD.email_consent <> 'unsubscribed' AND EXISTS (SELECT 1 FROM crash_once) THEN
             DELETE FROM crash_once; RAISE EXCEPTION 'crash'; END IF; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER crash_unsub BEFORE UPDATE ON profiles FOR EACH ROW EXECUTE FUNCTION crash_unsub()").execute(&h.state.db).await.unwrap();
    mock.lock().unwrap().customers.insert(1, customer_at(1, Some("ann@example.com"), "UNSUBSCRIBED", None, "NOT_SUBSCRIBED", "2025-02-01T00:00:00Z", "2025-02-01T00:00:00Z"));
    hook(&h, "customers/update", "c2", &json!({"id": 1})).await;
    h.drain().await;
    // The crash rolled back the delete of crash_once too, so drop the guard row now and retry.
    sqlx::query("DELETE FROM crash_once").execute(&h.state.db).await.unwrap();
    for _ in 0..3 {
        h.fast_forward().await;
    }
    assert_eq!(consent(&h, "ann@example.com").await.0, "unsubscribed", "the replay applies the opt-out");

    // Re-linking by an order sticks against a snapshot no newer than the order.
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann.new@example.com", chrono::Utc::now())).await;
    h.drain().await;
    let new = profile_id_of(&h, "ann.new@example.com").await.unwrap();
    let snapshot = customer_at(1, Some("ann@example.com"), "UNSUBSCRIBED", None, "NOT_SUBSCRIBED", "2025-02-01T00:00:00Z", "2025-02-01T00:00:00Z");
    mock.lock().unwrap().files.insert("customers".into(), jsonl(&[snapshot]));
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    let (linked, email): (Uuid, String) = sqlx::query_as("SELECT profile_id, email FROM shopify_customers WHERE customer_id = 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(linked, new, "the older snapshot didn't move the link back");
    assert_eq!(email, "ann.new@example.com", "the link's contact moved with it");
    h.cleanup().await;
}

/// Item 3: a data request exports only the named customer's orders, a profile another customer of
/// the store still uses isn't deleted, and an old install's shop redaction leaves the current
/// install's data.
#[tokio::test]
async fn privacy_requests_touch_exactly_the_named_customer() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    // Two customers of the store used the same address: one profile.
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "shared@example.com", chrono::Utc::now())).await;
    h.drain().await;
    mock.lock().unwrap().customers.insert(2, customer(2, Some("shared@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c2", &json!({"id": 2})).await;
    hook(&h, "orders/create", "o2", &order_body(5002, 2, "shared@example.com", chrono::Utc::now())).await;
    h.drain().await;
    let shared = profile_id_of(&h, "shared@example.com").await.unwrap();
    // Customer 1's export has customer 1's order only.
    hook(&h, "customers/data_request", "d1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 1}, "orders_requested": [5001], "data_request": {"id": 1}})).await;
    h.drain().await;
    let export: Value = sqlx::query_scalar("SELECT export FROM shopify_data_requests").fetch_one(&h.state.db).await.unwrap();
    let orders: HashSet<String> = export["events"].as_array().unwrap().iter().filter_map(|e| e["properties"]["order_id"].as_str().map(String::from)).collect();
    assert_eq!(orders, HashSet::from(["5001".to_string()]), "{export}");
    // Erasing customer 1 keeps the profile customer 2 still uses (even once 5002 is gone too).
    hook(&h, "customers/redact", "r2", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 2}, "orders_to_redact": [5002]})).await;
    h.drain().await;
    // Customer 2's link was removed; now customer 1 is the only one left. Re-link 2 without events.
    mock.lock().unwrap().customers.insert(3, customer(3, Some("shared@example.com"), "NOT_SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c3", &json!({"id": 3})).await;
    h.drain().await;
    hook(&h, "customers/redact", "r1", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 1}, "orders_to_redact": [5001]})).await;
    h.drain().await;
    assert_eq!(profile_id_of(&h, "shared@example.com").await, Some(shared), "customer 3 of the same store still uses it");

    // An old install's shop redaction after the store connected again: only the old install's data goes.
    hook(&h, "orders/create", "o4", &order_body(5004, 4, "old@example.com", chrono::Utc::now())).await;
    h.drain().await;
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    connect(&mut h).await;
    hook(&h, "orders/create", "o5", &order_body(5005, 5, "new@example.com", chrono::Utc::now())).await;
    h.drain().await;
    hook(&h, "shop/redact", "s1", &json!({"shop_id": 1, "shop_domain": SHOP})).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5004").await.is_none(), "the old install's order is erased");
    assert!(event(&h, "Placed Order", "order:5005").await.is_some(), "the current install's order stays");
    // And the current install keeps working.
    hook(&h, "orders/create", "o6", &order_body(5006, 6, "newer@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5006").await.is_some());
    h.cleanup().await;
}

/// Item 3: the erasure marker and an import's write can't interleave: a privacy request waits for
/// a write already inside its fence, then erases what it wrote.
#[tokio::test]
async fn erasure_waits_for_a_write_in_flight() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    let generation: i32 = sqlx::query_scalar("SELECT generation FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    // An importer is between its erasure check and its write for customer 7.
    let fence = dumbmail::features::shopify::fence::open(&h.state.db, acct, SHOP, generation, Some(7), None).await.unwrap().expect("not erased yet");
    // The privacy request arrives now.
    hook(&h, "customers/redact", "r7", &json!({"shop_id": 1, "shop_domain": SHOP, "customer": {"id": 7}, "orders_to_redact": []})).await;
    let state = h.state.clone();
    let privacy = tokio::spawn(async move { dumbmail::features::shopify::privacy::run(&state, acct).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(!privacy.is_finished(), "the request waits for the write in flight");
    // The importer writes customer 7's link, then leaves its fence.
    sqlx::query("INSERT INTO shopify_customers (account_id, customer_id, shop_domain, email) VALUES ($1, 7, $2, 'seven@example.com')").bind(acct).bind(SHOP).execute(&h.state.db).await.unwrap();
    fence.done().await.unwrap();
    privacy.await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM shopify_customers WHERE customer_id = 7").await, 0, "erased after the write landed");
    // A later write for that customer gets no fence.
    assert!(dumbmail::features::shopify::fence::open(&h.state.db, acct, SHOP, generation, Some(7), None).await.unwrap().is_none());
    h.cleanup().await;
}

/// Item 8: old work can't write into a new install; an uninstall counts as handled only once the
/// disconnect committed; one without a trigger time is checked against the store.
#[tokio::test]
async fn old_work_cannot_write_into_a_new_install() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    let gen1: i32 = sqlx::query_scalar("SELECT generation FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    // An Admin API answer read for install 1 lands after the store was reinstalled.
    let answer = dumbmail::features::shopify::customers::Customer::from_graphql(&customer(9, Some("inflight@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED")).unwrap();
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    connect(&mut h).await;
    let written = dumbmail::features::shopify::customers::apply(&h.state.db, acct, SHOP, gen1, &answer, false).await.unwrap();
    assert!(written.is_none() && profile_id_of(&h, "inflight@example.com").await.is_none(), "install 1's answer wrote nothing");
    // A worker job queued for install 1 does nothing for install 2.
    mock.lock().unwrap().customers.insert(9, customer(9, Some("inflight@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED"));
    sqlx::query("INSERT INTO shopify_refresh (account_id, kind, shopify_id) VALUES ($1, 'customer', 9)").bind(acct).execute(&h.state.db).await.unwrap();
    let reads = mock.lock().unwrap().node_reads.len();
    dumbmail::jobs::enqueue_background(&h.state.db, "shopify_work", json!({"account_id": acct, "generation": gen1}), None).await.unwrap();
    dumbmail::features::run_job(&h.state, "shopify_work", &json!({"account_id": acct, "generation": gen1})).await.unwrap().unwrap();
    assert_eq!(mock.lock().unwrap().node_reads.len(), reads, "an old job can't pick up the new install");
    sqlx::query("DELETE FROM shopify_refresh").execute(&h.state.db).await.unwrap();

    // An uninstall whose disconnect fails is tried again when Shopify delivers it again.
    sqlx::query("CREATE TABLE fail_forget (n int)").execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO fail_forget VALUES (1)").execute(&h.state.db).await.unwrap();
    sqlx::query(
        "CREATE FUNCTION fail_forget() RETURNS trigger AS $$ BEGIN
           IF NEW.status = 'uninstalled' AND EXISTS (SELECT 1 FROM fail_forget) THEN RAISE EXCEPTION 'disconnect failed'; END IF; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_forget BEFORE UPDATE ON shopify_shops FOR EACH ROW EXECUTE FUNCTION fail_forget()").execute(&h.state.db).await.unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    assert_eq!(hook_with(&h, "app/uninstalled", "u1", &json!({}), &[("x-shopify-triggered-at", &now)]).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], true);
    sqlx::query("DELETE FROM fail_forget").execute(&h.state.db).await.unwrap();
    assert_eq!(hook_with(&h, "app/uninstalled", "u1", &json!({}), &[("x-shopify-triggered-at", &now)]).await, StatusCode::OK);
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], false, "the redelivery disconnected it");
    connect(&mut h).await;

    // No trigger time: while the store still accepts this install's token, it's an earlier install's.
    assert_eq!(hook(&h, "app/uninstalled", "u2", &json!({})).await, StatusCode::OK);
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], true, "a stale uninstall without a time is ignored");
    uninstall_in_shopify(&mock);
    assert_eq!(hook(&h, "app/uninstalled", "u3", &json!({})).await, StatusCode::OK);
    assert_eq!(h.ok("GET", "/api/shopify", None).await["connected"], false, "the store refused the token: really uninstalled");
    h.cleanup().await;
}

fn bulk_order(id: i64, email: &str, refunds: Value) -> Value {
    let now = chrono::Utc::now().to_rfc3339();
    json!({"id": format!("gid://shopify/Order/{id}"), "name": format!("#{}", id - 4000), "createdAt": now, "processedAt": now, "updatedAt": now, "cancelledAt": null,
        "cancelReason": null, "test": false, "email": email, "phone": null, "currencyCode": "USD", "displayFinancialStatus": "PAID", "displayFulfillmentStatus": "UNFULFILLED",
        "discountCodes": [], "totalPriceSet": {"shopMoney": {"amount": "50.00", "currencyCode": "USD"}}, "subtotalPriceSet": {"shopMoney": {"amount": "50.00"}},
        "totalDiscountsSet": {"shopMoney": {"amount": "0"}}, "totalShippingPriceSet": {"shopMoney": {"amount": "0"}}, "totalTaxSet": {"shopMoney": {"amount": "0"}},
        "customer": null, "refunds": refunds, "fulfillments": []})
}

fn bulk_line(order: i64, line: i64) -> Value {
    json!({"id": format!("gid://shopify/LineItem/{line}"), "title": "Mug", "variantTitle": null, "sku": "SKU", "vendor": "Acme", "quantity": 1,
        "originalUnitPriceSet": {"shopMoney": {"amount": "50.00"}}, "totalDiscountSet": {"shopMoney": {"amount": "0.00"}}, "product": {"id": "gid://shopify/Product/101"},
        "variant": {"id": "gid://shopify/ProductVariant/1011"}, "image": null, "__parentId": format!("gid://shopify/Order/{order}")})
}

/// Item 4: a child row whose parent was saved from an earlier read isn't dropped: the parent is
/// read again in full.
#[tokio::test]
async fn bulk_import_never_drops_a_line() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let prod = |p: i64| json!({"id": format!("gid://shopify/Product/{p}"), "title": format!("P{p}"), "handle": format!("p{p}"), "status": "ACTIVE", "publishedAt": "2025-01-01T00:00:00Z", "tags": [], "descriptionHtml": "x".repeat(120)});
    let variant = json!({"id": "gid://shopify/ProductVariant/11", "title": "Default Title", "price": "11.00", "inventoryQuantity": 1, "availableForSale": true,
        "inventoryItem": {"id": "gid://shopify/InventoryItem/19", "tracked": true}, "__parentId": "gid://shopify/Product/1"});
    let mut full = product(1, "P1", 1, "11.00");
    full["id"] = json!("gid://shopify/Product/1");
    {
        let mut m = mock.lock().unwrap();
        // Ungrouped, and small reads: product 1 is saved before its variant arrives.
        m.ignore_grouping = true;
        m.max_chunk = 160;
        m.files.insert("products".into(), jsonl(&[prod(1), prod(2), prod(3), variant]));
        m.products.insert(1, full);
        let o5001 = bulk_order(5001, "ann@example.com", json!([]));
        let mut o = o5001.clone();
        o["lineItems"] = json!({"nodes": [{"id": "gid://shopify/LineItem/71", "title": "Mug", "variantTitle": null, "sku": "SKU", "vendor": "Acme", "quantity": 1,
            "originalUnitPriceSet": {"shopMoney": {"amount": "50.00"}}, "totalDiscountSet": {"shopMoney": {"amount": "0.00"}}, "product": {"id": "gid://shopify/Product/101"},
            "variant": {"id": "gid://shopify/ProductVariant/1011"}, "image": null}]});
        m.orders.insert(5001, o);
        m.files.insert("orders".into(), jsonl(&[o5001, bulk_order(5002, "bob@example.com", json!([])), bulk_order(5003, "cat@example.com", json!([])), bulk_line(5001, 71)]));
    }
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    for _ in 0..3 {
        h.fast_forward().await;
    }
    assert_eq!(h.ok("GET", "/api/catalog/products/1", None).await["price"], 11.0, "the variant that came after its product was saved isn't lost");
    assert!(event(&h, "Ordered Product", "order:5001:71").await.is_some(), "the line that came after its order was saved isn't lost");
    h.cleanup().await;
}

/// Item 5: refunds the history import finds are history: no live refund flow starts.
#[tokio::test]
async fn history_refunds_are_quiet() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    live_flow(&mut h, "Refunded Order").await;
    let now = chrono::Utc::now().to_rfc3339();
    {
        let mut m = mock.lock().unwrap();
        m.files.insert("orders".into(), jsonl(&[bulk_order(5001, "ann@example.com", json!([{"id": "gid://shopify/Refund/9"}])), bulk_line(5001, 1)]));
        // A recent refund, discovered only by the import.
        m.orders.insert(5001, gql_order(5001, &now, vec![refund(9, &now, &[("SUCCESS", "20.00")], &[])]));
    }
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    settle_imports(&h).await;
    for _ in 0..3 {
        h.fast_forward().await;
    }
    assert_eq!(value(&h, "Refunded Order", "refund:9").await, Some(20.0));
    assert_eq!(flow_runs(&h).await, 0, "the import's refund starts no flow");
    h.cleanup().await;
}

/// Item 6: every refund transaction is read (not just the first page); one too large to read in
/// full is left as it was and marked, rather than counted wrong; a new order and its lines are
/// recorded together.
#[tokio::test]
async fn refunds_are_read_in_full() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let now = chrono::Utc::now().to_rfc3339();
    hook(&h, "orders/create", "o1", &order_body(5001, 1, "ann@example.com", chrono::Utc::now() - chrono::Duration::minutes(5))).await;
    h.drain().await;
    let tx = |n: usize| (0..n).map(|_| json!({"kind": "REFUND", "status": "SUCCESS", "amountSet": {"shopMoney": {"amount": "2.00", "currencyCode": "USD"}}})).collect::<Vec<_>>();
    // 25 transactions of 2.00: 20 on the first page, 5 on the next.
    let mut r = refund(900, &now, &[], &[]);
    r["transactions"] = json!({"nodes": tx(20), "pageInfo": {"hasNextPage": true, "endCursor": "1"}});
    r["refundLineItems"]["pageInfo"] = json!({"hasNextPage": false, "endCursor": null});
    {
        let mut m = mock.lock().unwrap();
        m.orders.insert(5001, gql_order(5001, &now, vec![r.clone()]));
        m.refund_pages.insert(("gid://shopify/Refund/900".into(), "transactions".into()), vec![json!([]), json!(tx(5))]);
    }
    hook(&h, "refunds/create", "r1", &json!({"id": 900, "order_id": 5001})).await;
    h.drain().await;
    assert_eq!(value(&h, "Refunded Order", "refund:900").await, Some(50.0), "all 25 transactions");
    assert_eq!(value(&h, "Placed Order", "order:5001").await, Some(50.0));
    // A refund whose transactions never end: not reconciled, value left as it was.
    let mut endless = refund(901, &now, &[], &[]);
    endless["transactions"] = json!({"nodes": tx(20), "pageInfo": {"hasNextPage": true, "endCursor": "1"}});
    {
        let mut m = mock.lock().unwrap();
        let later = (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339();
        m.orders.insert(5001, gql_order(5001, &later, vec![r, endless]));
        m.refund_pages.insert(("gid://shopify/Refund/901".into(), "transactions".into()), (0..40).map(|_| json!(tx(1))).collect());
    }
    hook(&h, "refunds/create", "r2", &json!({"id": 901, "order_id": 5001})).await;
    h.drain().await;
    let (v, props, _, _) = event(&h, "Placed Order", "order:5001").await.unwrap();
    assert_eq!(v, Some(50.0), "not changed to a wrong total");
    assert_eq!(props["refunds_not_reconciled"], true);
    // A new order and its lines go in together: a failure on a line leaves no order either.
    sqlx::query(
        "CREATE FUNCTION fail_line() RETURNS trigger AS $$ BEGIN IF NEW.unique_id = 'order:5002:12' THEN RAISE EXCEPTION 'line failed'; END IF; RETURN NEW; END $$ LANGUAGE plpgsql",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_line BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION fail_line()").execute(&h.state.db).await.unwrap();
    hook(&h, "orders/create", "o2", &order_body(5002, 2, "bob@example.com", chrono::Utc::now())).await;
    h.drain().await;
    assert!(event(&h, "Placed Order", "order:5002").await.is_none(), "no order without its lines");
    assert!(event(&h, "Ordered Product", "order:5002:11").await.is_none());
    h.cleanup().await;
}

/// Item 7: a store-made code is reused only for the store (and install) that made it.
#[tokio::test]
async fn store_codes_belong_to_the_connected_store() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    h.credit(5.0).await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    let c = h.ok("POST", "/api/coupons", Some(json!({"name": "WELCOME", "description": "10% off"}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("PUT", &format!("/api/shopify/coupons/{cid}"), Some(json!({"title": "Welcome 10%", "value": {"percentage": 10}, "prefix": "HI"}))).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let lid: Uuid = list["id"].as_str().unwrap().parse().unwrap();
    let p = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, 'ann@x.test', 'subscribed')").bind(p).bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO list_members (list_id, profile_id) VALUES ($1, $2)").bind(lid).bind(p).execute(&h.state.db).await.unwrap();
    let content = json!({"subject": "A code", "blocks": [{"type": "text", "html": "<p>CODE[{% coupon_code 'WELCOME' %}]</p>"}]});
    let send = |h: &mut Harness| {
        let content = content.clone();
        let h = h as *mut Harness;
        async move {
            let h = unsafe { &mut *h };
            let camp = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "email", "audience": {"include": [{"type": "list", "id": lid}]}, "content": content}))).await;
            h.ok("POST", &format!("/api/campaigns/{}/send", camp["id"].as_str().unwrap()), None).await;
            for _ in 0..5 {
                h.fast_forward().await;
            }
            let m = h.state.email.sent().into_iter().rfind(|m| m.to == "ann@x.test").expect("sent");
            let i = m.html.find("CODE[").unwrap() + 5;
            m.html[i..].split(']').next().unwrap().to_string()
        }
    };
    let a = send(&mut h).await;
    assert!(mock.lock().unwrap().discounts.contains_key(&a));
    // Store A is disconnected and store B connected: Ann's next message gets a code B made.
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    connect_shop(&mut h, "second-store.myshopify.com").await;
    let b = send(&mut h).await;
    assert_ne!(b, a, "store A's code isn't handed out once store B is connected");
    let shop: String = sqlx::query_scalar("SELECT shop_domain FROM shopify_discount_codes WHERE code = $1").bind(&b).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(shop, "second-store.myshopify.com");
    // With the same store still connected, the person keeps their code.
    assert_eq!(send(&mut h).await, b);
    h.cleanup().await;
}

/// Item 9: a steady backlog of reads can't starve the import: each kind's budget is fixed once
/// per run, and the import always gets its share.
#[tokio::test]
async fn reads_cannot_starve_the_import() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    let generation: i32 = sqlx::query_scalar("SELECT generation FROM shopify_shops").fetch_one(&h.state.db).await.unwrap();
    // A slow store and a long backlog of customers to read again (more than a slice can do).
    mock.lock().unwrap().nodes_delay_ms = 400;
    sqlx::query("INSERT INTO shopify_refresh (account_id, kind, shopify_id) SELECT $1, 'customer', g FROM generate_series(1, 2500) g").bind(acct).execute(&h.state.db).await.unwrap();
    mock.lock().unwrap().files.insert("customers".into(), jsonl(&[customer(1, Some("ann@example.com"), "SUBSCRIBED", None, "NOT_SUBSCRIBED")]));
    h.ok("POST", "/api/shopify/import", Some(json!({"confirm": true}))).await;
    sqlx::query("DELETE FROM jobs WHERE kind = 'shopify_work'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE shopify_shops SET work_queued_at = NULL").execute(&h.state.db).await.unwrap();
    let ops = mock.lock().unwrap().bulk_ops.len();
    // One worker run.
    dumbmail::features::run_job(&h.state, "shopify_work", &json!({"account_id": acct, "generation": generation})).await.unwrap().unwrap();
    assert!(count(&h, "SELECT count(*) FROM shopify_refresh").await > 0, "the backlog isn't done in one run");
    assert_eq!(mock.lock().unwrap().bulk_ops.len(), ops + 1, "the import still got its turn");
    // A lease that expired is never renewed: the run must take it afresh.
    let lease = dumbmail::features::shopify::shop::acquire(&h.state, acct, generation).await.unwrap().unwrap();
    sqlx::query("UPDATE shopify_shops SET lease_until = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    assert!(!dumbmail::features::shopify::shop::renew(&h.state, &lease).await.unwrap(), "an expired lease isn't renewed");
    h.cleanup().await;
}

/// The browser stand-in the storefront tests run the snippet in (Node, no DOM library).
const BROWSER: &str = r#"
const posts = [];
const storage = () => { const m = new Map(); return { getItem: (k) => m.has(k) ? m.get(k) : null, setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k), _m: m }; };
const ls = storage(), ss = storage();
const listeners = {};
const el = () => new Proxy({
  attrs: {}, style: {}, dataset: {}, children: [], childNodes: [], textContent: "", innerHTML: "", className: "", value: "",
  classList: { contains: () => false, add: () => {}, remove: () => {}, toggle: () => {} },
  setAttribute(k, v) { this.attrs[k] = v; }, getAttribute(k) { return this.attrs[k] ?? null; }, hasAttribute(k) { return k in this.attrs; },
  querySelector: () => null, querySelectorAll: () => [], getElementsByTagName: () => [], closest: () => null, matches: () => false,
  appendChild: (c) => c, insertBefore: (c) => c, removeChild: (c) => c, remove: () => {}, addEventListener: () => {}, removeEventListener: () => {},
  attachShadow: () => el(), getBoundingClientRect: () => ({ top: 0, left: 0, width: 0, height: 0 }),
}, { get: (t, k) => k in t ? t[k] : (typeof k === "symbol" ? undefined : (() => null)) });
const script = el();
script.attrs.src = "https://dm.test/onsite.js?k=pk";
script.src = "https://dm.test/onsite.js?k=pk";
globalThis.localStorage = ls; globalThis.sessionStorage = ss;
globalThis.location = { href: "https://shop.test/products/mug", origin: "https://shop.test", pathname: "/products/mug", search: "", hostname: "shop.test", protocol: "https:" };
Object.defineProperty(globalThis, "navigator", { value: { userAgent: "node", sendBeacon: () => true }, configurable: true });
globalThis.fetch = (url, opts) => { if (opts && opts.method === "POST") posts.push(String(url)); return Promise.resolve({ ok: true, json: () => Promise.resolve([]) }); };
globalThis.document = new Proxy({
  currentScript: script, readyState: "complete", head: { appendChild: (s) => { added.push(s); } }, body: el(), documentElement: el(),
  createElement: () => el(), querySelector: () => null, querySelectorAll: () => [], getElementById: () => null, getElementsByTagName: () => [],
  removeEventListener: () => {}, visibilityState: "visible", hidden: false,
  addEventListener: (n, cb) => { listeners[n] = cb; }, cookie: "", title: "Mug", referrer: "",
}, { get: (t, k) => k in t ? t[k] : undefined });
const added = [];
globalThis.window = globalThis;
window.addEventListener = () => {}; window.removeEventListener = () => {};
globalThis.history = { pushState: () => {}, replaceState: () => {} };
globalThis.MutationObserver = class { observe() {} disconnect() {} };
globalThis.IntersectionObserver = class { observe() {} disconnect() {} };
globalThis.requestAnimationFrame = (f) => setTimeout(f, 0);
globalThis.getComputedStyle = () => ({}); globalThis.innerWidth = 1200; globalThis.innerHeight = 800; globalThis.scrollY = 0;
globalThis.Intl = Intl;
window.setTimeout = setTimeout; window.setInterval = () => 0;
window.Shopify = { loadFeatures: (f, cb) => { window.Shopify.customerPrivacy = { analyticsProcessingAllowed: () => true, marketingAllowed: () => true }; cb(false); } };
"#;

async fn served_snippet() -> Option<String> {
    let mut h = Harness::new().await?;
    h.signup().await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let (_, _, onsite) = h.raw("GET", &format!("/onsite.js?k={pk}"), &[], "").await;
    h.cleanup().await;
    Some(onsite)
}

fn embed_js() -> String {
    let liquid = include_str!("../connectors/shopify/extensions/dumbmail-embed/blocks/dumbmail.liquid");
    liquid.split("<script>").nth(1).unwrap().split("</script>").next().unwrap().replace("{{ dumbmail_src | json }}", "\"https://dm.test/onsite.js?k=pk\"")
}

/// Item 10: marketing withdrawn alone stops the snippet; a withdrawal between the snippet being
/// added and it starting is honoured when it starts; without marketing consent the snippet never
/// recognises anyone; the pixel passes nothing on without consent.
#[tokio::test]
async fn storefront_consent_is_respected() {
    if !node_available() {
        eprintln!("node not installed; skipping");
        return;
    }
    let Some(onsite) = served_snippet().await else { return };
    let run = |consent: &str, steps: &str| {
        let script = format!(
            "{BROWSER}\nwindow.Shopify = {{ loadFeatures: (f, cb) => {{ window.Shopify.customerPrivacy = {consent}; cb(false); }} }};\n{embed}\nconst start = () => (new Function({onsite}))();\n{steps}\nsetTimeout(() => console.log(JSON.stringify({{ posts, profile: ls.getItem('pl_profile'), added: added.length }})), 20);",
            embed = embed_js(),
            onsite = serde_json::to_string(&onsite).unwrap()
        );
        serde_json::from_str::<Value>(&run_node(&script)).unwrap()
    };
    let both = "{ analyticsProcessingAllowed: () => true, marketingAllowed: () => true }";
    // A: marketing withdrawn alone, after the snippet started.
    let a = run(both, "start(); window.dumbmail.identify({ email: 'ann@example.com' }); const before = posts.length;
        listeners.visitorConsentCollected({ detail: { analyticsAllowed: true, marketingAllowed: false } }); posts.length = 0;
        window.dumbmail.track('After', {}); window.dumbmail.identify({ email: 'bob@example.com' });");
    assert_eq!(a["posts"], json!([]), "marketing withdrawn: nothing more is sent: {a}");
    assert_eq!(a["profile"], Value::Null);
    // B: analytics withdrawn after the snippet was added but before it started.
    let b = run(both, "ls.setItem('pl_buffer', JSON.stringify([{ metric: 'Held', time: Date.now() }])); listeners.visitorConsentCollected({ detail: { analyticsAllowed: false, marketingAllowed: false } }); start();
        if (ls.getItem('pl_buffer') !== null) posts.push('buffer kept');
        window.dumbmail.identify({ email: 'ann@example.com' }); window.dumbmail.track('Viewed', {});");
    assert_eq!(b["added"], 1);
    assert_eq!(b["posts"], json!([]), "the withdrawal before start-up is honoured: {b}");
    // C: no marketing consent: a stored profile isn't used, and identify does nothing.
    let c = run("{ analyticsProcessingAllowed: () => true, marketingAllowed: () => false }", "ls.setItem('pl_profile', JSON.stringify({ email: 'old@example.com' })); start();
        window.dumbmail.track('Viewed', {}); window.dumbmail.identify({ email: 'new@example.com' });");
    let posted: Vec<String> = serde_json::from_value(c["posts"].clone()).unwrap();
    assert!(!posted.iter().any(|p| p.contains("/client/events")), "nothing tracked against the stored profile: {c}");
    assert_eq!(c["profile"], json!("{\"email\":\"old@example.com\"}"), "identify changed nothing");

    // D: the pixel without marketing consent passes nothing on.
    let src = include_str!("../connectors/shopify/extensions/dumbmail-pixel/src/index.js").replace("import { register } from \"@shopify/web-pixels-extension\";", "");
    let pixel = format!(
        r#"
const posts = [];
globalThis.fetch = (url, opts) => {{ posts.push(JSON.parse(opts.body)); return Promise.resolve({{}}); }};
const local = (() => {{ const m = new Map(); return {{ getItem: async (k) => m.has(k) ? m.get(k) : null, setItem: async (k, v) => m.set(k, v), removeItem: async (k) => m.delete(k), m }}; }})();
const session = (() => {{ const m = new Map(); return {{ getItem: async (k) => m.has(k) ? m.get(k) : null, setItem: async (k, v) => m.set(k, v), removeItem: async (k) => m.delete(k) }}; }})();
const subs = {{}};
function register(fn) {{ fn({{ analytics: {{ subscribe: (n, cb) => subs[n] = cb }}, browser: {{ localStorage: local, sessionStorage: session }}, settings: {{ base: "https://dm.test", key: "pk" }},
  init: {{ customerPrivacy: {{ analyticsProcessingAllowed: true, marketingAllowed: false }} }}, customerPrivacy: {{ subscribe: () => {{}} }} }}); }}
{src}
await local.setItem("pl_buffer", JSON.stringify([{{ metric: "Viewed", time: Date.now() }}]));
await subs.checkout_completed({{ data: {{ checkout: {{ email: "ann@example.com", lineItems: [], order: {{ id: "gid://shopify/Order/5" }} }} }}, context: {{ document: {{ location: {{ href: "x", origin: "https://shop.test" }} }} }} }});
await new Promise((r) => setTimeout(r, 10));
console.log(JSON.stringify({{ posts: posts.length, profile: await local.getItem("pl_profile"), buffer: await local.getItem("pl_buffer") }}));
"#
    );
    let d: Value = serde_json::from_str(&run_node(&pixel)).unwrap();
    assert_eq!(d["posts"], 0, "{d}");
    assert_eq!(d["profile"], Value::Null, "no identity passed to the storefront script");
    assert!(d["buffer"].is_string(), "held browsing activity isn't flushed");
}

/// Item 11: a complete product scan of a store with no products left unpublishes them all.
#[tokio::test]
async fn an_emptied_store_empties_the_catalog() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let prod = jsonl(&[
        json!({"id": "gid://shopify/Product/1", "updatedAt": "2025-01-01T00:00:00Z", "title": "P1", "handle": "p1", "status": "ACTIVE", "publishedAt": "2025-01-01T00:00:00Z", "tags": []}),
        json!({"id": "gid://shopify/ProductVariant/11", "title": "Default Title", "price": "1.00", "inventoryQuantity": 1, "availableForSale": true, "inventoryItem": {"id": "gid://shopify/InventoryItem/19", "tracked": true}, "__parentId": "gid://shopify/Product/1"}),
    ]);
    mock.lock().unwrap().files.insert("products".into(), prod);
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/1", None).await["published"], true);
    // The store removed its last product: the bulk operation completes with no result file.
    mock.lock().unwrap().files.insert("products".into(), String::new());
    h.ok("POST", "/api/shopify/products/resync", None).await;
    settle_imports(&h).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/1", None).await["published"], false, "gone from the store: unpublished");
    h.cleanup().await;
}

/// Item 13: an address the store supplies later to an existing held profile is marked as the
/// store's (so service messages reach it there); connecting ships two draft service-message flows.
#[tokio::test]
async fn store_addresses_and_order_starters() {
    let (mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let flows: Vec<(String, String, Value)> = sqlx::query_as("SELECT name, status, definition FROM flows ORDER BY name").fetch_all(&h.state.db).await.unwrap();
    let names: Vec<&str> = flows.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(names, ["Order confirmed", "Order shipped"]);
    assert!(flows.iter().all(|f| f.1 == "draft" && f.2["settings"]["service_messages"] == true), "{flows:?}");
    // Reconnecting doesn't add them twice.
    h.ok("POST", "/api/shopify/disconnect", Some(json!({"confirm": true}))).await;
    connect(&mut h).await;
    assert_eq!(count(&h, "SELECT count(*) FROM flows").await, 2);

    // A phone-first customer, held; later Shopify has their email too.
    mock.lock().unwrap().customers.insert(1, customer(1, None, "NOT_SUBSCRIBED", Some("+15035550111"), "NOT_SUBSCRIBED"));
    hook(&h, "customers/update", "c1", &json!({"id": 1})).await;
    h.drain().await;
    mock.lock().unwrap().customers.insert(1, customer_at(1, Some("late@example.com"), "NOT_SUBSCRIBED", Some("+15035550111"), "NOT_SUBSCRIBED", "2025-03-01T00:00:00Z", "2025-03-01T00:00:00Z"));
    hook(&h, "customers/update", "c2", &json!({"id": 1})).await;
    h.drain().await;
    let (pid, held, source): (Uuid, bool, Option<String>) = sqlx::query_as("SELECT id, marketing_hold, email_source FROM profiles WHERE email = 'late@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(held);
    assert_eq!(source.as_deref(), Some("integration:shopify:acme-goods.myshopify.com"));
    let confirmed: Uuid = sqlx::query_scalar("SELECT id FROM flows WHERE name = 'Order confirmed'").fetch_one(&h.state.db).await.unwrap();
    assert!(!dumbmail::marketing_hold::blocks_email(&h.state.db, pid, Some(confirmed)).await.unwrap(), "the order confirmation reaches the store's address");
    h.cleanup().await;
}

/// Item 14: a closed account gets no catalog sync, and one left running is cancelled.
#[tokio::test]
async fn closed_accounts_get_no_catalog_sync() {
    let (_mock, base) = mock_shopify().await;
    let mut h = shopify_harness!(base);
    h.signup().await;
    connect(&mut h).await;
    let acct = account_id(&h).await;
    h.ok("POST", "/api/shopify/products/resync", None).await;
    sqlx::query("UPDATE accounts SET closed_at = now() WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE shopify_shops SET reconcile_after = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    tick(&h).await;
    let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM shopify_imports WHERE kind = 'products'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(statuses, ["cancelled"], "the running sync is cancelled and none is scheduled");
    h.cleanup().await;
}

//! The ready-made integrations (recipes) end to end, through the real webhook endpoint and the
//! real script sandbox: each vendor's signature accepted and refused, every documented event turned
//! into the expected events, resent and out-of-order deliveries, sensitive fields never stored,
//! people held back from marketing, and a run through a flow. What each script returns for each
//! fixture is checked literally by the unit test `recipe_scripts_match_their_fixtures`.

#[macro_use]
mod common;
use common::*;

use axum::http::StatusCode;
use base64::Engine;
use hmac::Mac;
use serde_json::{json, Value};

macro_rules! integ_harness {
    () => {{
        dumbmail::features::integrations::sandbox::set_program(env!("CARGO_BIN_EXE_dumbmail"));
        dumbmail::features::integrations::sandbox::set_global_cap(64);
        harness!()
    }};
}

/// How a vendor signs its webhooks, for the test to sign like it.
#[derive(Clone, Copy)]
enum Sign {
    /// HMAC-SHA256 of the body, base64, in `header` (WooCommerce, AfterShip, Acuity).
    Base64(&'static str),
    /// HMAC-SHA256 of the body, base64 after a prefix (Typeform's `sha256=`).
    Base64Prefixed(&'static str, &'static str),
    /// HMAC-SHA1 of the body, hex (Segment).
    Sha1Hex(&'static str),
    /// `t=…,v1=…` over `t.body` (Stripe style: Recharge).
    Stripe(&'static str),
    /// `ts=…;h1=…` over `ts:body` (Paddle).
    Paddle,
    /// base64 HMAC-SHA256 of the webhook URL + body (Square).
    SquareUrl,
    /// hex HMAC-SHA256 of `<ms>:body`, with the timestamp in a header (Webflow).
    Webflow,
    /// base64 HMAC-SHA256 of `<ISO time>body` (Zendesk).
    Zendesk,
    /// The secret itself in a header (BigCommerce custom headers, Gorgias).
    Header(&'static str),
}

fn mac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).unwrap();
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn sign(how: Sign, secret: &str, url: &str, body: &str) -> Vec<(String, String)> {
    let now = chrono::Utc::now();
    let t = now.timestamp();
    match how {
        Sign::Base64(h) => vec![(h.into(), b64(&mac256(secret.as_bytes(), body.as_bytes())))],
        Sign::Base64Prefixed(h, p) => vec![(h.into(), format!("{p}{}", b64(&mac256(secret.as_bytes(), body.as_bytes()))))],
        Sign::Sha1Hex(h) => {
            let mut m = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes()).unwrap();
            m.update(body.as_bytes());
            vec![(h.into(), hex::encode(m.finalize().into_bytes()))]
        }
        Sign::Stripe(h) => vec![(h.into(), format!("t={t},v1={}", hex::encode(mac256(secret.as_bytes(), format!("{t}.{body}").as_bytes()))))],
        Sign::Paddle => vec![("Paddle-Signature".into(), format!("ts={t};h1={}", hex::encode(mac256(secret.as_bytes(), format!("{t}:{body}").as_bytes()))))],
        Sign::SquareUrl => vec![("x-square-hmacsha256-signature".into(), b64(&mac256(secret.as_bytes(), format!("{url}{body}").as_bytes())))],
        Sign::Webflow => {
            let ms = now.timestamp_millis();
            vec![("x-webflow-timestamp".into(), ms.to_string()), ("x-webflow-signature".into(), hex::encode(mac256(secret.as_bytes(), format!("{ms}:{body}").as_bytes())))]
        }
        Sign::Zendesk => {
            let ts = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            vec![("X-Zendesk-Webhook-Signature-Timestamp".into(), ts.clone()), ("X-Zendesk-Webhook-Signature".into(), b64(&mac256(secret.as_bytes(), format!("{ts}{body}").as_bytes())))]
        }
        Sign::Header(h) => vec![(h.into(), secret.into())],
    }
}

fn fixture(recipe: &str, name: &str) -> Value {
    let p = format!("{}/recipes/{recipe}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))).unwrap()
}

/// An installed recipe and how to post to it.
struct Installed {
    recipe: &'static str,
    id: String,
    url: String,
    sign: Sign,
    secret: String,
    flows: Vec<Value>,
}

async fn install(h: &mut Harness, recipe: &'static str, sign: Sign, secret: &str, create_flows: bool) -> Installed {
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": recipe, "create_flows": create_flows}))).await;
    let id = created["integration"]["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"signature_secret": secret}))).await;
    for f in created["flows"].as_array().unwrap() {
        assert_eq!(f["status"], "draft", "{recipe}: suggested flows start as drafts");
    }
    Installed { recipe, id, url: created["integration"]["inbound_url"].as_str().unwrap().to_string(), sign, secret: secret.into(), flows: created["flows"].as_array().unwrap().clone() }
}

impl Installed {
    fn path(&self) -> String {
        self.url[self.url.find("/webhooks/in/").unwrap()..].to_string()
    }

    /// Post a payload as the vendor would (signed with `secret`), with the fixture's headers and query.
    async fn post_as(&self, h: &Harness, body: &str, headers: &Value, query: &Value, secret: &str) -> (StatusCode, String) {
        let qs: Vec<String> = query.as_object().into_iter().flatten().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect();
        let (path, url) = if qs.is_empty() { (self.path(), self.url.clone()) } else { (format!("{}?{}", self.path(), qs.join("&")), format!("{}?{}", self.url, qs.join("&"))) };
        let mut hdrs: Vec<(String, String)> = headers.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect();
        if !hdrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            hdrs.push(("content-type".into(), "application/json".into()));
        }
        hdrs.extend(sign(self.sign, secret, &url, body));
        let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (s, _, r) = h.raw("POST", &path, &hdrs, body).await;
        h.drain().await;
        (s, r)
    }

    /// Post a fixture, signed; it must be accepted.
    async fn post(&self, h: &Harness, name: &str) -> String {
        let f = fixture(self.recipe, name);
        let body = body_of(&f["input"]["payload"]);
        let (s, r) = self.post_as(h, &body, &f["input"]["headers"], &f["input"]["query"], &self.secret).await;
        assert_eq!(s, StatusCode::OK, "{}/{name}: {r}", self.recipe);
        r
    }

    /// The same fixture signed with the wrong secret is refused and writes nothing.
    async fn refused(&self, h: &Harness, name: &str) {
        let f = fixture(self.recipe, name);
        let body = body_of(&f["input"]["payload"]);
        let before = count(h, "SELECT count(*) FROM events").await;
        let (s, _) = self.post_as(h, &body, &f["input"]["headers"], &f["input"]["query"], "not-the-secret-0000").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{}/{name} with a wrong signature", self.recipe);
        assert_eq!(count(h, "SELECT count(*) FROM events").await, before);
    }

    async fn deliveries(&self, h: &mut Harness) -> Vec<Value> {
        h.ok("GET", &format!("/api/integrations/{}/deliveries", self.id), None).await["data"].as_array().unwrap().clone()
    }

    fn flow(&self, contains: &str) -> String {
        self.flows.iter().find(|f| f["name"].as_str().unwrap().contains(contains)).unwrap_or_else(|| panic!("no flow named like {contains}"))["id"].as_str().unwrap().to_string()
    }
}

fn body_of(payload: &Value) -> String {
    match payload {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

async fn events(h: &Harness, metric: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = $1").bind(metric).fetch_one(&h.state.db).await.unwrap()
}

/// Values of `metric` on `email`'s profile, in order.
async fn values(h: &Harness, metric: &str, email: &str) -> Vec<f64> {
    sqlx::query_scalar("SELECT e.value FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = $1 AND p.email = $2 ORDER BY e.value")
        .bind(metric)
        .bind(email)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

async fn prop(h: &Harness, metric: &str, key: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT e.properties->>$2 FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = $1 ORDER BY e.occurred_at, e.id")
        .bind(metric)
        .bind(key)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

/// (marketing_hold, email_consent) of a profile.
async fn consent(h: &Harness, email: &str) -> (bool, String) {
    sqlx::query_as("SELECT marketing_hold, email_consent FROM profiles WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap_or_else(|e| panic!("{email}: {e}"))
}

/// Whether an email campaign to everyone would reach `email` (marketing).
async fn campaign_reaches(h: &mut Harness, email: &str) -> bool {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": format!("All {email}")}))).await;
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap();
    let account: uuid::Uuid = sqlx::query_scalar("SELECT account_id FROM profiles WHERE id = $1").bind(pid).fetch_one(&h.state.db).await.unwrap();
    dumbmail::audience::add_to_list(&h.state.db, account, list["id"].as_str().unwrap().parse().unwrap(), &[pid]).await.unwrap();
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}}))).await;
    est["count"].as_i64().unwrap() > 0
}

/// None of `needles` appears in anything stored or shown: delivery payloads, headers, requests,
/// results and action lists, the delivery API (with payloads), events, profiles, or a dry run of
/// the same payloads.
async fn never_stored(h: &mut Harness, inst: &Installed, needles: &[String], fixtures: &[&str]) {
    if needles.is_empty() {
        return;
    }
    let stored: String = sqlx::query_scalar(
        "SELECT COALESCE(string_agg(concat_ws(' ', body, headers::text, query::text, request::text, result::text, actions::text, error), ' '), '') FROM integration_deliveries",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    let written: String = sqlx::query_scalar(
        "SELECT concat((SELECT COALESCE(string_agg(properties::text, ' '), '') FROM events), ' ', (SELECT COALESCE(string_agg(concat_ws(' ', properties::text, integration_keys::text), ' '), '') FROM profiles))",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    let mut shown = String::new();
    for d in inst.deliveries(h).await {
        let one = h.ok("GET", &format!("/api/integrations/{}/deliveries/{}", inst.id, d["id"].as_str().unwrap()), None).await;
        shown.push_str(&one.to_string());
    }
    for name in fixtures {
        let f = fixture(inst.recipe, name);
        let mut body = json!({"payload": f["input"]["payload"]});
        for k in ["headers", "query", "source"] {
            if f["input"][k].is_object() || f["input"][k].is_string() {
                body[k] = f["input"][k].clone();
            }
        }
        let out = h.ok("POST", &format!("/api/integrations/{}/test", inst.id), Some(body)).await;
        assert_eq!(out["ok"], true, "{name}: {out}");
        shown.push_str(&out.to_string());
    }
    for n in needles {
        assert!(!stored.contains(n.as_str()), "{}: {n:?} was stored with a delivery", inst.recipe);
        assert!(!written.contains(n.as_str()), "{}: {n:?} reached an event or profile", inst.recipe);
        assert!(!shown.contains(n.as_str()), "{}: {n:?} was shown by the API or a dry run", inst.recipe);
    }
}

/// The `sensitive` values every fixture of a recipe lists.
fn sensitive(recipe: &str, names: &[&str]) -> Vec<String> {
    names.iter().flat_map(|n| fixture(recipe, n)["sensitive"].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_str().map(String::from)).collect()
}

/// A live flow on `metric` sending one email (`service` marks it as service messages).
async fn live_flow(h: &mut Harness, metric: &str, subject: &str, service: bool) -> String {
    let def = json!({"entry": "e1", "settings": {"service_messages": service}, "nodes": [
        {"id": "e1", "type": "email", "name": "Note", "next": null, "content": {"subject": subject, "blocks": [{"type": "text", "html": "<p>Thanks.</p>"}]}}]});
    let f = h.ok("POST", "/api/flows", Some(json!({"name": subject, "trigger": {"type": "metric", "metric": metric}, "definition": def}))).await;
    let id = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
    id
}

fn sent_to(h: &Harness, subject_starts: &str) -> Vec<String> {
    let mut v: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject.starts_with(subject_starts)).map(|m| m.to).collect();
    v.sort();
    v
}

/// Make deliveries waiting to retry due now, and run them.
async fn retry_now(h: &Harness) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind LIKE 'integrations_%'").execute(&h.state.db).await.unwrap();
    h.drain().await;
}

// ---------- WooCommerce ----------

#[tokio::test]
async fn woocommerce_orders_refunds_and_customers() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let w = install(&mut h, "woocommerce", Sign::Base64("X-WC-Webhook-Signature"), "wc-webhook-secret-0001", true).await;
    let (api, base) = mock_woo().await;
    let i = h.ok("GET", &format!("/api/integrations/{}", w.id), None).await;
    let mut pull = i["pull"].clone();
    pull["url"] = json!(format!("{base}/wp-json/wc/v3/orders"));
    h.ok("PATCH", &format!("/api/integrations/{}", w.id), Some(json!({"pull": pull, "credentials": {"consumer_key": "ck_test", "consumer_secret": "cs_test"}}))).await;
    let thanks = live_flow(&mut h, "Placed Order", "Thanks for your order", true).await;
    // Riley was already here, subscribed to texts on another number.
    h.ok("POST", "/api/profiles", Some(json!({"email": "riley.example@example.com", "phone_number": "+15035550100", "sms_consent": "subscribed"}))).await;
    w.refused(&h, "order-created-paid.json").await;
    // Out of order: the completion arrives before the order was created; both count once.
    for name in ["ping.json", "order-updated-completed.json", "order-created-paid.json", "order-created-pending.json", "order-created-free.json", "customer-created.json"] {
        w.post(&h, name).await;
    }
    assert_eq!(events(&h, "Placed Order").await, 1, "the pending and free orders aren't orders; the paid one counts once");
    assert_eq!(events(&h, "Fulfilled Order").await, 1);
    // A resend with the same delivery id is acknowledged and ignored.
    let f = fixture("woocommerce", "order-updated-completed.json");
    let mut headers = f["input"]["headers"].clone();
    headers["x-wc-webhook-delivery-id"] = json!("d-1001");
    let body = f["input"]["payload"].to_string();
    w.post_as(&h, &body, &headers, &json!({}), &w.secret).await;
    let (_, r) = w.post_as(&h, &body, &headers, &json!({}), &w.secret).await;
    assert!(r.contains("duplicate"), "{r}");
    // Refunds come from the refund records (fetched): once per refund id, each with its own amount
    // and date, however often the order is resent; the merchant-only refund carries gateway: false.
    w.post(&h, "order-updated-refunded.json").await;
    w.post(&h, "order-updated-refunded.json").await;
    assert!(api.seen.lock().unwrap().iter().any(|p| p == "refunds:727"));
    assert_eq!(values(&h, "Refunded Order", "riley.example@example.com").await, vec![9.0, 10.0]);
    assert_eq!(prop(&h, "Refunded Order", "currency").await, vec!["USD", "USD"]);
    assert_eq!(prop(&h, "Refunded Order", "gateway").await, vec!["true", "false"], "724 went through the gateway; 726 didn't");
    let when: Vec<String> = sqlx::query_scalar("SELECT to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Refunded Order' ORDER BY 1").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(when, vec!["2017-03-21 19:55:37", "2017-03-21 20:07:11"], "each refund's own date");
    w.post(&h, "order-updated-cancelled.json").await;
    assert_eq!(events(&h, "Cancelled Order").await, 1);
    // An order that was never paid, cancelled: recorded apart from orders.
    w.post(&h, "order-updated-cancelled-unpaid.json").await;
    assert_eq!(events(&h, "Unpaid Order Cancelled").await, 1);
    assert_eq!(events(&h, "Cancelled Order").await, 1);
    // Buyers and customers are held back from marketing; the service flow still reached the buyer.
    // The order brought a different number: the SMS subscription belonged to the old one.
    let (phone, sms): (Option<String>, String) = sqlx::query_as("SELECT phone_number, sms_consent FROM profiles WHERE email = 'riley.example@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((phone.as_deref(), sms.as_str()), (Some("+14155550142"), "never_subscribed"));
    // Riley existed before (never held); Morgan was created by the integration and is held.
    assert_eq!(consent(&h, "morgan.example@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "morgan.example@example.com").await);
    assert_eq!(sent_to(&h, "Thanks for your order"), vec!["riley.example@example.com"]);
    let keys: Value = sqlx::query_scalar("SELECT integration_keys FROM profiles WHERE email = 'morgan.example@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(keys["woocommerce_customer"], json!(["25"]), "{keys}");
    let _ = thanks;
    let names = ["order-created-paid.json", "order-updated-refunded.json"];
    never_stored(&mut h, &w, &sensitive("woocommerce", &names), &names).await;
    h.cleanup().await;
}

#[derive(Default)]
struct MockWoo {
    orders: std::sync::Mutex<Vec<Value>>,
    /// The query of each orders-list request, and each refunds path.
    seen: std::sync::Mutex<Vec<String>>,
}

/// A stand-in for a WooCommerce store's REST API (HTTP Basic with the consumer key and secret):
/// `GET /wp-json/wc/v3/orders` (modified_after and modified_before, both exclusive; orderby=modified,
/// order=asc; per_page and page) and `GET /wp-json/wc/v3/orders/{id}/refunds` (the documented example).
async fn mock_woo() -> (std::sync::Arc<MockWoo>, String) {
    use axum::extract::{Path, Query, State};
    use axum::routing::get;
    use std::collections::HashMap;
    use std::sync::Arc;
    let m: Arc<MockWoo> = Default::default();
    let authed = |h: &axum::http::HeaderMap| h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Basic {}", b64(b"ck_test:cs_test")));
    let app = axum::Router::new()
        .route(
            "/wp-json/wc/v3/orders",
            get(move |State(m): State<Arc<MockWoo>>, h: axum::http::HeaderMap, uri: axum::http::Uri, Query(q): Query<HashMap<String, String>>| async move {
                assert!(authed(&h));
                assert!(!q.contains_key("offset"), "no offsets into a changing list: {q:?}");
                m.seen.lock().unwrap().push(uri.query().unwrap_or("").to_string());
                let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
                let after = q.get("modified_after").map(|s| t(s));
                let before = q.get("modified_before").map(|s| t(s));
                let per: usize = q.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(10);
                let page: usize = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
                let mut rows: Vec<Value> = m.orders.lock().unwrap().iter().filter(|o| {
                    let mt = chrono::NaiveDateTime::parse_from_str(o["date_modified_gmt"].as_str().unwrap(), "%Y-%m-%dT%H:%M:%S").unwrap().and_utc().timestamp();
                    after.is_none_or(|a| mt > a) && before.is_none_or(|b| mt < b)
                }).cloned().collect();
                rows.sort_by(|a, b| a["date_modified_gmt"].as_str().cmp(&b["date_modified_gmt"].as_str()).then(a["id"].as_i64().cmp(&b["id"].as_i64())));
                axum::Json(Value::Array(rows.into_iter().skip((page - 1) * per).take(per).collect()))
            }),
        )
        .route(
            "/wp-json/wc/v3/orders/{id}/refunds",
            get(move |State(m): State<Arc<MockWoo>>, h: axum::http::HeaderMap, Path(id): Path<u64>| async move {
                assert!(authed(&h));
                m.seen.lock().unwrap().push(format!("refunds:{id}"));
                axum::Json(fixture("woocommerce", "fetch-refunds.json")["input"]["payload"].clone())
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, format!("http://{addr}"))
}

#[tokio::test]
async fn woocommerce_history_reads_time_windows_without_offsets() {
    let mut h = integ_harness!();
    h.signup().await;
    let (api, base) = mock_woo().await;
    let w = install(&mut h, "woocommerce", Sign::Base64("X-WC-Webhook-Signature"), "wc-webhook-secret-0001", false).await;
    let i = h.ok("GET", &format!("/api/integrations/{}", w.id), None).await;
    let mut pull = i["pull"].clone();
    pull["url"] = json!(format!("{base}/wp-json/wc/v3/orders"));
    h.ok("PATCH", &format!("/api/integrations/{}", w.id), Some(json!({"pull": pull, "credentials": {"consumer_key": "ck_test", "consumer_secret": "cs_test"}}))).await;
    // 250 paid orders, many changed in the same second.
    let base_order = fixture("woocommerce", "order-created-paid.json")["input"]["payload"].clone();
    let orders: Vec<Value> = (0..250)
        .map(|k| {
            let mut o = base_order.clone();
            o["id"] = json!(2000 + k);
            o["number"] = json!((2000 + k).to_string());
            o["billing"]["email"] = json!(format!("buyer{k}@example.com"));
            o["billing"]["phone"] = json!("");
            o["date_modified_gmt"] = json!(format!("2026-03-{:02}T10:00:{:02}", 1 + k / 20, (k % 3)));
            o
        })
        .collect();
    *api.orders.lock().unwrap() = orders;
    h.ok("POST", &format!("/api/integrations/{}/pull", w.id), Some(json!({"backfill_since": "2026-01-01"}))).await;
    for _ in 0..20 {
        retry_now(&h).await;
    }
    let st = h.ok("GET", &format!("/api/integrations/{}", w.id), None).await;
    assert_eq!(st["backfill"]["status"], "done", "{}", st["backfill"]);
    assert_eq!(events(&h, "Placed Order").await, 250, "every order once: {:?}", api.seen.lock().unwrap());
    assert_eq!(count(&h, "SELECT count(DISTINCT unique_id) FROM events").await, count(&h, "SELECT count(*) FROM events").await);
    h.cleanup().await;
}

// ---------- Recharge ----------

#[tokio::test]
async fn recharge_subscriptions_wait_for_their_customer_and_remind_before_charges() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let r = install(&mut h, "recharge", Sign::Stripe("X-Recharge-Webhook-Signature"), "rc-client-secret-0001", true).await;
    let remind = r.flow("upcoming charge");
    let f = h.ok("GET", &format!("/api/flows/{remind}"), None).await;
    assert_eq!(f["definition"]["settings"]["service_messages"], true);
    h.ok("POST", &format!("/api/flows/{remind}/status"), Some(json!({"status": "live"}))).await;
    r.refused(&h, "subscription-created.json").await;
    // The subscription arrives before its customer: it waits, then finds them.
    r.post(&h, "subscription-created.json").await;
    let d = r.deliveries(&mut h).await;
    assert_eq!(d[0]["status"], "error", "{:?}", d[0]);
    assert!(d[0]["next_attempt_at"].is_string(), "retried, not dropped");
    r.post(&h, "customer-created.json").await;
    retry_now(&h).await;
    assert_eq!(events(&h, "Subscription Started").await, 1);
    // Cancelled, then reactivated, delivered newest first: both kept with Recharge's own times.
    r.post(&h, "subscription-activated.json").await;
    r.post(&h, "subscription-cancelled.json").await;
    r.post(&h, "subscription-cancelled.json").await;
    assert_eq!(events(&h, "Subscription Cancelled").await, 1, "a resend counts once");
    let order: Vec<String> = sqlx::query_scalar("SELECT m.name FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name IN ('Subscription Cancelled', 'Subscription Reactivated') ORDER BY e.occurred_at, e.id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(order, vec!["Subscription Cancelled", "Subscription Reactivated"]);
    // Recharge's documented window is 48 hours: a delivery signed 30 hours ago is accepted.
    let late = fixture("recharge", "customer-created.json")["input"]["payload"].to_string();
    let t = chrono::Utc::now().timestamp() - 30 * 3600;
    let sig = format!("t={t},v1={}", hex::encode(mac256(r.secret.as_bytes(), format!("{t}.{late}").as_bytes())));
    let (st, _, _) = h.raw("POST", &format!("{}?topic=customer/created", r.path()), &[("X-Recharge-Webhook-Signature", &sig), ("content-type", "application/json")], &late).await;
    assert_eq!(st, StatusCode::OK, "within Recharge's 48 hours");
    let t = chrono::Utc::now().timestamp() - 49 * 3600;
    let sig = format!("t={t},v1={}", hex::encode(mac256(r.secret.as_bytes(), format!("{t}.{late}").as_bytes())));
    let (st, _, _) = h.raw("POST", &format!("{}?topic=customer/created", r.path()), &[("X-Recharge-Webhook-Signature", &sig), ("content-type", "application/json")], &late).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "older than 48 hours");
    // The topic in the URL is only a hint: relabelled bodies record only what they show.
    for name in ["subscription-relabelled.json", "subscription-created-relabelled-activated.json", "charge-queued-relabelled-paid.json"] {
        r.post(&h, name).await;
    }
    assert_eq!(events(&h, "Subscription Cancelled").await, 1);
    assert_eq!(events(&h, "Subscription Reactivated").await, 1);
    assert_eq!(events(&h, "Charge Paid").await, 0);
    // The charge is paid before its (late) upcoming notice: the reminder stands down.
    for name in ["subscription-skipped.json", "charge-paid.json", "charge-upcoming.json", "charge-paid.json", "charge-failed.json", "no-topic.json"] {
        r.post(&h, name).await;
    }
    assert_eq!(values(&h, "Charge Paid", "jamie.example@example.com").await, vec![13.14]);
    assert_eq!(prop(&h, "Charge Paid", "currency").await, vec!["USD"]);
    assert_eq!(events(&h, "Charge Failed").await, 1);
    assert!(sent_to(&h, "Your next order is coming up").is_empty(), "the charge it announces was already paid");
    // A charge that's still coming up gets its reminder; once skipped, a late notice doesn't.
    let mut next = fixture("recharge", "charge-upcoming.json")["input"]["payload"].clone();
    next["charge"]["id"] = json!(100714429);
    r.post_as(&h, &next.to_string(), &json!({}), &json!({"topic": "charge/upcoming"}), &r.secret).await;
    r.post(&h, "charge-updated-skipped.json").await;
    assert_eq!(events(&h, "Charge Skipped").await, 1);
    // A charge that went through for nothing still stops its (late) reminder, with no revenue.
    r.post(&h, "charge-paid-free.json").await;
    let mut free_upcoming = fixture("recharge", "charge-upcoming.json")["input"]["payload"].clone();
    free_upcoming["charge"]["id"] = json!(100714430);
    r.post_as(&h, &free_upcoming.to_string(), &json!({}), &json!({"topic": "charge/upcoming"}), &r.secret).await;
    assert_eq!(values(&h, "Charge Paid", "jamie.example@example.com").await, vec![13.14], "no revenue from the free charge");
    // Held back from marketing, but the upcoming-charge reminder (a service message) reached them.
    assert_eq!(consent(&h, "jamie.example@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "jamie.example@example.com").await);
    assert_eq!(sent_to(&h, "Your next order is coming up"), vec!["jamie.example@example.com"], "once, for the charge that wasn't paid yet");
    let names = ["charge-upcoming.json"];
    never_stored(&mut h, &r, &sensitive("recharge", &names), &names).await;
    h.cleanup().await;
}

// ---------- Square ----------

#[derive(Default)]
struct MockSquare {
    /// What the order says is still due (in cents).
    due: std::sync::atomic::AtomicI64,
    reads: std::sync::atomic::AtomicUsize,
}

/// A stand-in for Square's `GET /v2/orders/{order_id}` (`{"order": {...}}`, Bearer token), with the
/// documented example order paid in two parts.
async fn mock_square() -> (std::sync::Arc<MockSquare>, String) {
    use axum::extract::State;
    use axum::routing::get;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let m: Arc<MockSquare> = Default::default();
    let app = axum::Router::new()
        .route(
            "/v2/orders/{id}",
            get(|State(m): State<Arc<MockSquare>>, h: axum::http::HeaderMap| async move {
                assert_eq!(h.get("authorization").and_then(|v| v.to_str().ok()), Some("Bearer sq-access-token-0001"));
                m.reads.fetch_add(1, Ordering::SeqCst);
                let mut o = fixture("square", "fetch-order-paid.json")["input"]["payload"].clone();
                let due = m.due.load(Ordering::SeqCst);
                o["order"]["net_amount_due_money"]["amount"] = json!(due);
                if due > 0 {
                    o["order"]["state"] = json!("OPEN");
                }
                axum::Json(o)
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, format!("http://{addr}"))
}

#[tokio::test]
async fn square_signs_its_url_counts_orders_once_and_refunds_find_their_payment() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let (api, base) = mock_square().await;
    let s = install(&mut h, "square", Sign::SquareUrl, "square-signature-key-0001", false).await;
    let i = h.ok("GET", &format!("/api/integrations/{}", s.id), None).await;
    let mut pull = i["pull"].clone();
    pull["url"] = json!(format!("{base}/v2/locations"));
    h.ok("PATCH", &format!("/api/integrations/{}", s.id), Some(json!({"pull": pull, "credentials": {"access_token": "sq-access-token-0001"}}))).await;
    live_flow(&mut h, "Placed Order", "Your receipt", true).await;
    s.refused(&h, "payment-updated-completed.json").await;
    // The signature covers the URL: the right body and secret on another URL is refused.
    let f = fixture("square", "payment-updated-completed.json");
    let body = f["input"]["payload"].to_string();
    let wrong = sign(Sign::SquareUrl, &s.secret, "https://pl.test/webhooks/in/other", &body);
    let mut hdrs: Vec<(&str, &str)> = wrong.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    hdrs.push(("content-type", "application/json"));
    assert_eq!(h.raw("POST", &s.path(), &hdrs, &body).await.0, StatusCode::UNAUTHORIZED);
    s.post(&h, "customer-created.json").await;
    s.post(&h, "payment-created-approved.json").await;
    // The first of two payments for one order: 1.50 is still due, so it isn't an order yet.
    api.due.store(150, std::sync::atomic::Ordering::SeqCst);
    s.post(&h, "payment-updated-completed.json").await;
    assert_eq!(events(&h, "Placed Order").await, 0, "part paid");
    let (_, again) = s.post_as(&h, &body, &json!({}), &json!({}), &s.secret).await;
    assert!(again.contains("duplicate"), "Square's resend is recognized by event_id: {again}");
    // The second payment names only the customer; once it completes, nothing is due: one order, in full.
    api.due.store(0, std::sync::atomic::Ordering::SeqCst);
    s.post(&h, "payment-updated-customer-only.json").await;
    assert_eq!(values(&h, "Placed Order", "avery.example@example.com").await, vec![2.5], "the whole order once, not one per payment");
    assert_eq!(prop(&h, "Placed Order", "order_id").await, vec!["03O3USaPaAaFnI6kkwB1JxGgBsUZY"]);
    // A refund of the customer-only payment finds the person through the link that payment saved.
    let mut rf = fixture("square", "refund-updated-completed.json")["input"]["payload"].clone();
    rf["event_id"] = json!("refund-for-customer-only");
    rf["data"]["object"]["refund"]["id"] = json!("R2Refund0001");
    rf["data"]["object"]["refund"]["payment_id"] = json!("R2NoEmailPayment0000000000000");
    s.post_as(&h, &rf.to_string(), &json!({}), &json!({}), &s.secret).await;
    s.post(&h, "refund-created-pending.json").await;
    s.post(&h, "refund-updated-completed.json").await;
    assert_eq!(values(&h, "Refunded Order", "avery.example@example.com").await, vec![0.6, 0.6], "both refunds found their buyer; the pending one isn't counted");
    s.post(&h, "customer-updated-unsubscribed.json").await;
    assert_eq!(consent(&h, "avery.example@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "avery.example@example.com").await);
    let reason: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE email = 'quinn.example@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(reason.is_some(), "unsubscribed from Square's marketing: never sent marketing here");
    assert_eq!(sent_to(&h, "Your receipt"), vec!["avery.example@example.com"]);
    let mut needles = sensitive("square", &["payment-updated-completed.json", "customer-created.json"]);
    needles.push("sq-access-token-0001".into());
    let names = ["payment-updated-completed.json", "customer-created.json"];
    never_stored(&mut h, &s, &needles, &names).await;
    h.cleanup().await;
}

// ---------- Paddle ----------

#[tokio::test]
async fn paddle_orders_refunds_and_its_marketing_consent_box() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let p = install(&mut h, "paddle", Sign::Paddle, "pdl_ntfset_test_0001", true).await;
    let winback = p.flow("win-back");
    h.ok("POST", &format!("/api/flows/{winback}/status"), Some(json!({"status": "live"}))).await;
    p.refused(&h, "transaction-completed.json").await;
    // The transaction comes before its customer: it waits, then counts once.
    p.post(&h, "transaction-completed.json").await;
    p.post(&h, "adjustment-created-pending.json").await;
    p.post(&h, "adjustment-updated-approved.json").await;
    p.post(&h, "customer-created.json").await;
    retry_now(&h).await;
    let (_, again) = p.post_as(&h, &fixture("paddle", "transaction-completed.json")["input"]["payload"].to_string(), &json!({}), &json!({}), &p.secret).await;
    assert!(again.contains("duplicate"), "{again}");
    assert_eq!(values(&h, "Placed Order", "jo.example@example.com").await, vec![652.15]);
    assert_eq!(values(&h, "Refunded Order", "jo.example@example.com").await, vec![1.0], "the pending refund isn't counted; the approved one once");
    for name in ["subscription-created.json", "subscription-activated.json", "subscription-canceled.json"] {
        p.post(&h, name).await;
    }
    assert_eq!(events(&h, "Subscription Started").await, 1, "created and activated are one start");
    assert_eq!(events(&h, "Subscription Cancelled").await, 1);
    // Only the marketing box subscribes; the held customer gets no win-back (a marketing flow).
    p.post(&h, "customer-updated-consent.json").await;
    assert_eq!(consent(&h, "jo.example@example.com").await, (true, "never_subscribed".into()));
    assert_eq!(consent(&h, "sam.example@example.com").await, (false, "subscribed".into()));
    assert!(!campaign_reaches(&mut h, "jo.example@example.com").await);
    assert!(campaign_reaches(&mut h, "sam.example@example.com").await);
    // Sam's subscription is cancelled too: the win-back reaches Sam, not Jo.
    let mut sams = fixture("paddle", "subscription-canceled.json")["input"]["payload"].clone();
    sams["event_id"] = json!("evt_sam_cancel");
    sams["data"]["id"] = json!("sub_sam");
    sams["data"]["customer_id"] = json!("ctm_01hv6y1consent00000000000");
    p.post_as(&h, &sams.to_string(), &json!({}), &json!({}), &p.secret).await;
    h.fast_forward().await;
    h.fast_forward().await;
    assert_eq!(sent_to(&h, "We'd like you back"), vec!["sam.example@example.com"]);
    // Sam turns marketing off in Paddle: the subscription Paddle made is withdrawn.
    p.post(&h, "customer-updated-withdrawn.json").await;
    assert_eq!(consent(&h, "sam.example@example.com").await.1, "unsubscribed");
    // Lee subscribed through a sign-up form, not Paddle: Paddle's "off" leaves that consent alone.
    h.ok("POST", "/api/profiles", Some(json!({"email": "lee.example@example.com", "email_consent": "subscribed"}))).await;
    let mut lee = fixture("paddle", "customer-updated-withdrawn.json")["input"]["payload"].clone();
    lee["event_id"] = json!("evt_lee_off");
    lee["data"]["id"] = json!("ctm_lee");
    lee["data"]["email"] = json!("lee.example@example.com");
    p.post_as(&h, &lee.to_string(), &json!({}), &json!({}), &p.secret).await;
    assert_eq!(consent(&h, "lee.example@example.com").await.1, "subscribed", "consent from elsewhere isn't Paddle's to withdraw");
    // Kim: Paddle's newer "on" arrives first, then an older "off": Paddle's order wins.
    let mut kim_on = fixture("paddle", "customer-updated-consent.json")["input"]["payload"].clone();
    kim_on["event_id"] = json!("evt_kim_on");
    kim_on["data"]["id"] = json!("ctm_kim");
    kim_on["data"]["email"] = json!("kim.example@example.com");
    kim_on["data"]["updated_at"] = json!("2024-05-02T10:00:00.000Z");
    let mut kim_off = kim_on.clone();
    kim_off["event_id"] = json!("evt_kim_off_old");
    kim_off["data"]["marketing_consent"] = json!(false);
    kim_off["data"]["updated_at"] = json!("2024-05-01T10:00:00.000Z");
    for b in [kim_on, kim_off] {
        p.post_as(&h, &b.to_string(), &json!({}), &json!({}), &p.secret).await;
    }
    assert_eq!(consent(&h, "kim.example@example.com").await, (false, "subscribed".into()), "an older 'off' doesn't undo a newer 'on'");
    // Ivy: Paddle's newer "off" arrives first and changes nothing; an older "on" after it doesn't subscribe.
    let mut ivy_off = fixture("paddle", "customer-updated-withdrawn.json")["input"]["payload"].clone();
    ivy_off["event_id"] = json!("evt_ivy_off");
    ivy_off["data"]["id"] = json!("ctm_ivy");
    ivy_off["data"]["email"] = json!("ivy.example@example.com");
    ivy_off["data"]["updated_at"] = json!("2024-06-02T10:00:00.000Z");
    let mut ivy_on = ivy_off.clone();
    ivy_on["event_id"] = json!("evt_ivy_on_old");
    ivy_on["data"]["marketing_consent"] = json!(true);
    ivy_on["data"]["updated_at"] = json!("2024-06-01T10:00:00.000Z");
    for b in [ivy_off, ivy_on] {
        p.post_as(&h, &b.to_string(), &json!({}), &json!({}), &p.secret).await;
    }
    assert_eq!(consent(&h, "ivy.example@example.com").await.1, "never_subscribed", "an older 'on' can't follow a newer 'off'");
    // Ann: Paddle subscribes her; she unsubscribes; later she opts in again herself (a form). Paddle's
    // "off" after that isn't Paddle's to withdraw.
    let mut ann_on = fixture("paddle", "customer-updated-consent.json")["input"]["payload"].clone();
    ann_on["event_id"] = json!("evt_ann_on");
    ann_on["data"]["id"] = json!("ctm_ann");
    ann_on["data"]["email"] = json!("ann.example@example.com");
    ann_on["data"]["updated_at"] = json!("2024-07-01T10:00:00.000Z");
    p.post_as(&h, &ann_on.to_string(), &json!({}), &json!({}), &p.secret).await;
    assert_eq!(consent(&h, "ann.example@example.com").await.1, "subscribed");
    let (ann, account): (uuid::Uuid, uuid::Uuid) = sqlx::query_as("SELECT id, account_id FROM profiles WHERE email = 'ann.example@example.com'").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE id = $1").bind(ann).execute(&h.state.db).await.unwrap();
    dumbmail::audience::record_event(&h.state.db, dumbmail::audience::NewEvent { account_id: account, profile_id: ann, metric: "Unsubscribed", properties: json!({"source": "link"}), value: None, occurred_at: None, unique_id: None, message_id: None }).await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'subscribed' WHERE id = $1").bind(ann).execute(&h.state.db).await.unwrap();
    let mut ann_off = ann_on.clone();
    ann_off["event_id"] = json!("evt_ann_off");
    ann_off["data"]["marketing_consent"] = json!(false);
    ann_off["data"]["updated_at"] = json!("2024-07-05T10:00:00.000Z");
    p.post_as(&h, &ann_off.to_string(), &json!({}), &json!({}), &p.secret).await;
    assert_eq!(consent(&h, "ann.example@example.com").await.1, "subscribed", "her own later opt-in stays");
    // A script can't write (or look people up by) DumbMail's consent bookkeeping keys.
    for bad in [
        json!([{"type": "upsert_profile", "email": "ann.example@example.com", "keys": {"consent_by:email": "x"}}]),
        json!([{"type": "add_keys", "profile": {"email": "ann.example@example.com"}, "keys": {"consent_as_of:x:email": "y"}}]),
        json!([{"type": "track_event", "metric": "Placed Order", "profile": {"property": "consent_by:email", "value": "x"}}]),
    ] {
        let out = h.ok("POST", &format!("/api/integrations/{}/test", p.id), Some(json!({"payload": {}, "script": format!("function handle() {{ return {bad}; }}")}))).await;
        assert_eq!(out["ok"], false, "{out}");
        assert!(out["error"].as_str().unwrap().contains("DumbMail's own"), "{out}");
    }
    let names = ["transaction-completed.json"];
    never_stored(&mut h, &p, &sensitive("paddle", &names), &names).await;
    h.cleanup().await;
}

// ---------- BigCommerce (webhooks that only carry an id; DumbMail fetches the record) ----------

#[derive(Default)]
struct MockStore {
    /// (X-Auth-Token, path with query) of each request.
    seen: std::sync::Mutex<Vec<(String, String)>>,
    /// Answer 404 to this many order reads first (the order isn't readable yet).
    not_yet: std::sync::atomic::AtomicUsize,
    /// Answer 429 with BigCommerce's rate-limit headers to this many order reads first.
    limited: std::sync::atomic::AtomicUsize,
}

/// A stand-in for BigCommerce's store API, answering with the documented example bodies kept in
/// the recipe's fixtures: `GET /stores/{hash}/v2/orders/{id}`,
/// `GET /stores/{hash}/v3/orders/{id}/payment_actions/refunds/{refund_id}` and
/// `GET /stores/{hash}/v3/customers?id:in=…`, all behind `X-Auth-Token`.
async fn mock_bigcommerce() -> (std::sync::Arc<MockStore>, String) {
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let m: Arc<MockStore> = Default::default();
    let note = |m: &MockStore, h: &axum::http::HeaderMap, uri: &axum::http::Uri| -> bool {
        let tok = h.get("x-auth-token").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        m.seen.lock().unwrap().push((tok.clone(), uri.path_and_query().map(|p| p.to_string()).unwrap_or_default()));
        tok == "bc-access-token-0001"
    };
    let app = axum::Router::new()
        .route(
            "/stores/abc123/v2/orders/{id}",
            get(move |State(m): State<Arc<MockStore>>, h: axum::http::HeaderMap, uri: axum::http::Uri, Path(id): Path<u64>| async move {
                if !note(&m, &h, &uri) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"status": 401}))).into_response();
                }
                if m.limited.load(Ordering::SeqCst) > 0 {
                    m.limited.fetch_sub(1, Ordering::SeqCst);
                    let headers = [("X-Rate-Limit-Time-Reset-Ms", "15000"), ("X-Rate-Limit-Time-Window-Ms", "30000"), ("X-Rate-Limit-Requests-Quota", "150"), ("X-Rate-Limit-Requests-Left", "0")];
                    return (StatusCode::TOO_MANY_REQUESTS, headers, axum::Json(json!({"status": 429, "title": "Too many requests"}))).into_response();
                }
                let none = [("X-Rate-Limit-Requests-Left", "100"), ("X-Rate-Limit-Time-Reset-Ms", "15000"), ("X-Rate-Limit-Time-Window-Ms", "30000"), ("X-Rate-Limit-Requests-Quota", "150")];
                if m.not_yet.load(Ordering::SeqCst) > 0 {
                    m.not_yet.fetch_sub(1, Ordering::SeqCst);
                    return (StatusCode::NOT_FOUND, none, axum::Json(json!({"status": 404, "title": "The requested resource was not found."}))).into_response();
                }
                let mut o = fixture("bigcommerce", "fetch-order-awaiting-fulfillment.json")["input"]["payload"].clone();
                o["id"] = json!(id);
                (StatusCode::OK, none, axum::Json(o)).into_response()
            }),
        )
        .route(
            "/stores/abc123/v3/orders/{id}/payment_actions/refunds/{rid}",
            get(move |State(m): State<Arc<MockStore>>, h: axum::http::HeaderMap, uri: axum::http::Uri| async move {
                if !note(&m, &h, &uri) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"status": 401})));
                }
                (StatusCode::OK, axum::Json(fixture("bigcommerce", "fetch-refund.json")["input"]["payload"].clone()))
            }),
        )
        .route(
            "/stores/abc123/v3/customers",
            get(move |State(m): State<Arc<MockStore>>, h: axum::http::HeaderMap, uri: axum::http::Uri| async move {
                if !note(&m, &h, &uri) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"status": 401})));
                }
                (StatusCode::OK, axum::Json(fixture("bigcommerce", "fetch-customer.json")["input"]["payload"].clone()))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, format!("http://{addr}"))
}

#[tokio::test]
async fn bigcommerce_webhooks_fetch_what_they_name() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let (store, base) = mock_bigcommerce().await;
    let b = install(&mut h, "bigcommerce", Sign::Header("X-Webhook-Token"), "bc-webhook-token-0001", false).await;
    let i = h.ok("GET", &format!("/api/integrations/{}", b.id), None).await;
    let mut pull = i["pull"].clone();
    pull["url"] = json!(format!("{base}/stores/{{{{settings.store_hash}}}}/v2/orders"));
    h.ok("PATCH", &format!("/api/integrations/{}", b.id), Some(json!({"pull": pull, "settings": {"store_hash": "abc123"}, "credentials": {"access_token": "bc-access-token-0001"}}))).await;
    live_flow(&mut h, "Placed Order", "Order confirmed", true).await;
    b.refused(&h, "webhook-order-created.json").await;
    // The order isn't readable yet: the fetch is tried again, then the order is recorded once.
    store.not_yet.store(1, std::sync::atomic::Ordering::SeqCst);
    b.post(&h, "webhook-order-created.json").await;
    assert_eq!(events(&h, "Placed Order").await, 0);
    let failed = b.deliveries(&mut h).await.into_iter().find(|d| d["source"] == "fetch").unwrap();
    assert!(failed["error"].as_str().is_some_and(|e| e.contains("404")), "{failed}");
    retry_now(&h).await;
    assert_eq!(events(&h, "Placed Order").await, 1);
    b.post(&h, "webhook-order-status-updated.json").await;
    assert_eq!(events(&h, "Placed Order").await, 1, "a second webhook for the same order reads it again; it still counts once");
    // A refund: the refund is fetched, then its order; recorded once, in the order's currency.
    b.post(&h, "webhook-refund-created.json").await;
    // BigCommerce's own resend of a callback (same hash, minutes later) is recognized.
    let dup = b.post(&h, "webhook-refund-created.json").await;
    assert!(dup.contains("duplicate"), "{dup}");
    assert_eq!(values(&h, "Refunded Order", "casey.example@example.com").await, vec![50.0]);
    assert_eq!(prop(&h, "Refunded Order", "currency").await, vec!["USD"]);
    b.post(&h, "webhook-customer-created.json").await;
    b.post(&h, "webhook-other-store.json").await;
    let seen = store.seen.lock().unwrap().clone();
    assert!(seen.iter().all(|(tok, _)| tok == "bc-access-token-0001"), "{seen:?}");
    let paths: Vec<&str> = seen.iter().map(|(_, p)| p.as_str()).collect();
    assert!(paths.contains(&"/stores/abc123/v3/orders/218/payment_actions/refunds/12"), "{paths:?}");
    assert!(paths.contains(&"/stores/abc123/v3/customers?id%3Ain=32"), "{paths:?}");
    assert_eq!(paths.iter().filter(|p| p.starts_with("/stores/abc123/v2/orders/218")).count(), 4, "404, then the order, the status update and once for the refund: {paths:?}");
    // Buyers and customers are held back from marketing; the service flow still reached the buyer.
    for email in ["casey.example@example.com", "drew.example@example.com"] {
        assert_eq!(consent(&h, email).await, (true, "never_subscribed".into()), "{email}");
    }
    assert!(!campaign_reaches(&mut h, "casey.example@example.com").await);
    assert_eq!(sent_to(&h, "Order confirmed"), vec!["casey.example@example.com"]);
    // The access token, the gate code a shopper typed and IP addresses are never stored or shown.
    let mut needles = sensitive("bigcommerce", &["fetch-order-awaiting-fulfillment.json", "fetch-customer.json"]);
    needles.push("bc-access-token-0001".into());
    let names = ["webhook-order-created.json", "webhook-refund-created.json"];
    never_stored(&mut h, &b, &needles, &names).await;
    h.cleanup().await;
}

// ---------- Hostaway (a pull, with a paid-in-full pre-arrival flow) ----------

#[derive(Default)]
struct MockHostaway {
    tokens: std::sync::atomic::AtomicUsize,
    /// The reservations the list returns, newest change first.
    rows: std::sync::Mutex<Vec<Value>>,
    /// afterId of each list request ("" for the first page).
    pages: std::sync::Mutex<Vec<String>>,
}

/// A stand-in for Hostaway's API: `POST /v1/accessTokens` (client credentials, scope general) and
/// `GET /v1/reservations` (`{"status": "success", "result": [...]}`, newest change first, paged with
/// `afterId`), as documented.
async fn mock_hostaway() -> (std::sync::Arc<MockHostaway>, String) {
    use axum::extract::{Query, State};
    use axum::routing::{get, post};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let m: Arc<MockHostaway> = Default::default();
    let app = axum::Router::new()
        .route(
            "/v1/accessTokens",
            post(|State(m): State<Arc<MockHostaway>>, axum::Form(f): axum::Form<HashMap<String, String>>| async move {
                assert_eq!(f.get("grant_type").map(String::as_str), Some("client_credentials"));
                assert_eq!(f.get("scope").map(String::as_str), Some("general"));
                assert_eq!(f.get("client_id").map(String::as_str), Some("10450"));
                let n = m.tokens.fetch_add(1, Ordering::SeqCst) + 1;
                axum::Json(json!({"token_type": "Bearer", "expires_in": 15897600, "access_token": format!("ha-token-{n}")}))
            }),
        )
        .route(
            "/v1/reservations",
            get(|State(m): State<Arc<MockHostaway>>, headers: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                assert!(headers.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|a| a.starts_with("Bearer ha-token-")));
                assert_eq!(q.get("includePayments").map(String::as_str), Some("1"), "remainingBalance only comes with includePayments=1");
                let after = q.get("afterId").cloned().unwrap_or_default();
                m.pages.lock().unwrap().push(after.clone());
                let rows = m.rows.lock().unwrap().clone();
                let start = if after.is_empty() { 0 } else { rows.iter().position(|r| r["id"].as_u64().map(|n| n.to_string()).as_deref() == Some(after.as_str())).map(|i| i + 1).unwrap_or(rows.len()) };
                let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100);
                let page: Vec<Value> = rows.into_iter().skip(start).take(limit).collect();
                axum::Json(json!({"status": "success", "result": page, "count": null, "limit": limit, "offset": null}))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, format!("http://{addr}"))
}

#[tokio::test]
async fn hostaway_pulls_reservations_and_reminds_only_paid_stays() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let (api, base) = mock_hostaway().await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "hostaway"}))).await;
    let id = created["integration"]["id"].as_str().unwrap().to_string();
    let mut pull = created["integration"]["pull"].clone();
    pull["url"] = json!(format!("{base}/v1/reservations"));
    pull["auth"]["token_url"] = json!(format!("{base}/v1/accessTokens"));
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": pull, "credentials": {"account_id": "10450", "api_key": "ha-api-key-secret-0001"}}))).await;
    let pre = created["flows"][0]["id"].as_str().unwrap().to_string();
    let f = h.ok("GET", &format!("/api/flows/{pre}"), None).await;
    assert_eq!(f["status"], "draft");
    assert_eq!(f["definition"]["settings"]["service_messages"], true);
    let mut trigger = f["trigger"].clone();
    trigger["hour"] = json!(0);
    h.ok("PATCH", &format!("/api/flows/{pre}"), Some(json!({"trigger": trigger}))).await;
    h.ok("POST", &format!("/api/flows/{pre}/status"), Some(json!({"status": "live"}))).await;

    let first = fixture("hostaway", "pull-first-page.json")["input"]["payload"]["result"].as_array().unwrap().clone();
    let now = chrono::Utc::now();
    let stamp = |mins: i64| (now - chrono::Duration::minutes(mins)).format("%Y-%m-%d %H:%M:%S").to_string();
    let today = now.with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    // Arriving in three days: the reminder (two days before) waits until tomorrow.
    let in_two = (today + chrono::Duration::days(3)).format("%Y-%m-%d").to_string();
    // Robin: paid in full. Guest 20: a balance is still due. Guest 21: cancelled. Pay 40: isPaid but
    // no remaining balance. Pay 41: a zero balance but isPaid false. Sam: paid in full. All arriving in two days.
    let signals = fixture("hostaway", "pull-payment-signals.json")["input"]["payload"]["result"].as_array().unwrap().clone();
    let mut sam = first[0].clone();
    sam["id"] = json!(50);
    sam["guestEmail"] = json!("sam.example@example.com");
    sam["phone"] = Value::Null;
    // Kit: paid. Lee: a paid and an unpaid version in the same second (unpaid wins). Dee: a 0.004 balance.
    let mut kit = first[0].clone();
    kit["id"] = json!(51);
    kit["guestEmail"] = json!("kit.example@example.com");
    kit["phone"] = Value::Null;
    let mut lee_unpaid = kit.clone();
    lee_unpaid["id"] = json!(52);
    lee_unpaid["guestEmail"] = json!("lee.example@example.com");
    lee_unpaid["remainingBalance"] = json!(80);
    let mut lee_paid = lee_unpaid.clone();
    lee_paid["remainingBalance"] = json!(0);
    let mut dee = kit.clone();
    dee["id"] = json!(53);
    dee["guestEmail"] = json!("dee.example@example.com");
    dee["remainingBalance"] = json!(0.004);
    let mut rows: Vec<Value> = first[..3].to_vec();
    rows.extend([signals[0].clone(), signals[1].clone(), sam, kit, lee_paid, lee_unpaid, dee]);
    for (i, r) in rows.iter_mut().enumerate() {
        // Lee's two versions (indexes 7 and 8) share one second.
        let slot = if i == 8 { 7 } else { i } as i64;
        r["updatedOn"] = json!(stamp(30 + slot * 10));
        r["arrivalDate"] = json!(in_two);
    }
    *api.rows.lock().unwrap() = rows.clone();
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    assert_eq!(events(&h, "Reservation Booked").await, 8);
    assert_eq!(values(&h, "Reservation Total", "robin.example@example.com").await, vec![867.0]);
    assert_eq!(events(&h, "Reservation Cancelled").await, 1);
    // The next run reads back only to the change the last one saw: nothing new, nothing twice.
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    assert_eq!(events(&h, "Reservation Confirmed").await, 9, "Lee's paid and unpaid versions are two events");
    let waiting = "SELECT count(*) FROM flow_event_date_fires WHERE outcome = 'pending'";
    assert_eq!(count(&h, waiting).await, 9, "each confirmed version waits; the paid-in-full filter is checked when it fires");
    // Newer versions, newest first: guest 20 pays; Robin's booking now shows a balance due again;
    // Sam's goes back to awaiting payment.
    let mut paid = rows[1].clone();
    paid["remainingBalance"] = json!(0);
    paid["updatedOn"] = json!(stamp(1));
    let mut unpaid = rows[0].clone();
    unpaid["remainingBalance"] = json!(50);
    unpaid["updatedOn"] = json!(stamp(2));
    let mut waiting = rows[5].clone();
    waiting["status"] = json!("awaitingPayment");
    waiting["updatedOn"] = json!(stamp(3));
    // Kit's booking goes back to awaiting payment, in a version with no guest email.
    let mut kit_waiting = rows[6].clone();
    kit_waiting["status"] = json!("awaitingPayment");
    kit_waiting["updatedOn"] = json!(stamp(4));
    kit_waiting.as_object_mut().unwrap().remove("guestEmail");
    let rest: Vec<Value> = vec![rows[2].clone(), rows[3].clone(), rows[4].clone()];
    let newest: Vec<Value> = [paid, unpaid, waiting, kit_waiting].into_iter().chain(rest).collect();
    *api.rows.lock().unwrap() = newest;
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    assert_eq!(events(&h, "Reservation Confirmed").await, 11);
    assert_eq!(events(&h, "Reservation Unconfirmed").await, 2, "Sam's, and Kit's (found by the reservation id)");
    // Tomorrow comes.
    sqlx::query("UPDATE flow_event_date_fires SET due_at = now() - interval '1 minute' WHERE outcome = 'pending'").execute(&h.state.db).await.unwrap();
    assert_eq!(api.tokens.load(std::sync::atomic::Ordering::SeqCst), 1, "the token is reused");
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(
        sent_to(&h, "See you soon"),
        vec!["guest20@example.com"],
        "only the stay that's now paid in full: not cancelled, missing, conflicting or nearly-zero payment details, not a same-second unpaid version, and not the bookings whose newest version isn't paid (Kit's had no email)"
    );
    // Held back from marketing.
    assert_eq!(consent(&h, "robin.example@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "robin.example@example.com").await);
    // Door codes, lockbox instructions and guest notes never stored or shown, and no access detail in the email.
    let needles = sensitive("hostaway", &["pull-first-page.json"]);
    let mail: String = h.state.email.sent().into_iter().map(|m| format!("{} {}", m.subject, m.html)).collect();
    for n in &needles {
        assert!(!mail.contains(n.as_str()), "{n:?} reached an email");
    }
    let inst = Installed { recipe: "hostaway", id, url: created["integration"]["inbound_url"].as_str().unwrap().into(), sign: Sign::Header("x"), secret: String::new(), flows: vec![] };
    never_stored(&mut h, &inst, &needles, &[]).await;
    h.cleanup().await;
}

// ---------- Forms, support, shipping and Segment ----------

#[tokio::test]
async fn form_submissions_never_subscribe_and_never_rename_anyone() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    // Someone who was already here: a forged form submission with their email mustn't rename them.
    h.ok("POST", "/api/profiles", Some(json!({"email": "lane.example@example.com", "first_name": "Lane"}))).await;
    let t = install(&mut h, "typeform", Sign::Base64Prefixed("Typeform-Signature", "sha256="), "typeform-secret-0001", false).await;
    let w = install(&mut h, "webflow", Sign::Webflow, "webflow-secret-0001", false).await;
    live_flow(&mut h, "Form Submitted", "We got your form", false).await;
    t.refused(&h, "form-response.json").await;
    w.refused(&h, "form-submission.json").await;
    // A Webflow signature from more than 5 minutes ago is refused.
    let f = fixture("webflow", "form-submission.json");
    let body = f["input"]["payload"].to_string();
    let old = chrono::Utc::now().timestamp_millis() - 400_000;
    let sig = hex::encode(mac256(b"webflow-secret-0001", format!("{old}:{body}").as_bytes()));
    let old_s = old.to_string();
    assert_eq!(h.raw("POST", &w.path(), &[("x-webflow-timestamp", &old_s), ("x-webflow-signature", &sig), ("content-type", "application/json")], &body).await.0, StatusCode::UNAUTHORIZED);
    h.ok("PATCH", &format!("/api/integrations/{}", t.id), Some(json!({"settings": {"email_field": "", "consent_field": "readable_ref_yes_no"}}))).await;
    t.post(&h, "form-response.json").await;
    let (_, again) = t.post_as(&h, &fixture("typeform", "form-response.json")["input"]["payload"].to_string(), &json!({}), &json!({}), &t.secret).await;
    assert!(again.contains("duplicate"), "{again}");
    t.post(&h, "form-response-no-email.json").await;
    h.ok("PATCH", &format!("/api/integrations/{}", w.id), Some(json!({"settings": {"email_field": "Email", "consent_field": "Newsletter", "copy_fields": ["Interest"]}}))).await;
    w.post(&h, "form-submission.json").await;
    assert_eq!(events(&h, "Form Submitted").await, 2);
    assert_eq!(prop(&h, "Form Submitted", "marketing_consent").await, vec!["true", "true"], "the consent box is recorded…");
    // …but nobody is subscribed, the existing profile kept its name, and the new one is held.
    let (name, cons): (Option<String>, String) = sqlx::query_as("SELECT first_name, email_consent FROM profiles WHERE email = 'lane.example@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((name.as_deref(), cons.as_str()), (Some("Lane"), "never_subscribed"));
    assert_eq!(consent(&h, "ari.example@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "ari.example@example.com").await);
    // The marketing flow on Form Submitted reached the existing (unheld) person, not the held one.
    assert_eq!(sent_to(&h, "We got your form"), vec!["lane.example@example.com"]);
    never_stored(&mut h, &t, &sensitive("typeform", &["form-response.json"]), &["form-response.json"]).await;
    // Webflow: only the mapped fields and the ids are kept; the visitor's message and name aren't
    // in the stored body, the delivery log or a dry run.
    never_stored(&mut h, &w, &sensitive("webflow", &["form-submission.json"]), &["form-submission.json"]).await;
    h.cleanup().await;
}

#[tokio::test]
async fn zendesk_tickets_keep_only_subject_and_status() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let z = install(&mut h, "zendesk", Sign::Zendesk, "zendesk-secret-0001", false).await;
    live_flow(&mut h, "Ticket Solved", "How did we do", true).await;
    z.refused(&h, "ticket-created.json").await;
    for name in ["ticket-solved.json", "ticket-created.json", "ticket-satisfaction.json", "ticket-solved.json"] {
        z.post(&h, name).await;
    }
    assert_eq!(events(&h, "Ticket Created").await, 1);
    assert_eq!(events(&h, "Ticket Solved").await, 1, "the resent solve (same invocation) counts once");
    z.post(&h, "ticket-solved-again.json").await;
    assert_eq!(events(&h, "Ticket Solved").await, 2, "solved again in the same minute: a new invocation, a second event");
    assert_eq!(prop(&h, "Satisfaction Rated", "rating").await, vec!["Good, I'm satisfied"]);
    assert_eq!(consent(&h, "kai.example@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "kai.example@example.com").await);
    assert_eq!(sent_to(&h, "How did we do"), vec!["kai.example@example.com"]);
    never_stored(&mut h, &z, &sensitive("zendesk", &["ticket-satisfaction.json"]), &["ticket-satisfaction.json"]).await;
    h.cleanup().await;
}

#[tokio::test]
async fn aftership_updates_reach_the_buyer_once_per_status() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let a = install(&mut h, "aftership", Sign::Base64("aftership-hmac-sha256"), "aftership-secret-0001", true).await;
    let ofd = a.flow("out for delivery");
    h.ok("POST", &format!("/api/flows/{ofd}/status"), Some(json!({"status": "live"}))).await;
    a.refused(&h, "in-transit.json").await;
    // Out of order: delivered first, then older updates write nothing (not even the buyer's phone);
    // a newer exception still counts.
    h.ok("POST", "/api/profiles", Some(json!({"email": "sky.example@example.com", "phone_number": "+15035550177", "sms_consent": "subscribed"}))).await;
    a.post(&h, "delivered.json").await;
    h.ok("PATCH", &format!("/api/profiles/{}", sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM profiles WHERE email = 'sky.example@example.com'").fetch_one(&h.state.db).await.unwrap()), Some(json!({"phone_number": "+15035550177", "sms_consent": "subscribed"}))).await;
    for name in ["in-transit.json", "out-for-delivery.json", "out-for-delivery.json", "stale-out-for-delivery.json", "pending.json"] {
        a.post(&h, name).await;
    }
    let (phone, sms): (Option<String>, String) = sqlx::query_as("SELECT phone_number, sms_consent FROM profiles WHERE email = 'sky.example@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((phone.as_deref(), sms.as_str()), (Some("+15035550177"), "subscribed"), "a stale update never rewrote the phone or its consent");
    a.post(&h, "exception.json").await;
    for (m, n) in [("Shipment In Transit", 0), ("Out For Delivery", 0), ("Delivered", 1), ("Shipment Exception", 1)] {
        assert_eq!(events(&h, m).await, n, "{m}");
    }
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'shop@example.com'").await, 0, "the sender isn't the customer");
    assert!(sent_to(&h, "Your order arrives today").is_empty(), "a late 'out for delivery' after 'delivered' starts nothing");
    // A newer "in transit" (say the van turned back) stands an older "out for delivery" down even
    // when the stored state no longer has the shipment.
    let mut back = fixture("aftership", "in-transit.json")["input"]["payload"].clone();
    back["event_id"] = json!("back-in-transit");
    back["msg"]["id"] = json!("22222222222222222222222222222222");
    back["msg"]["updated_at"] = json!("2021-01-15T09:00:00+00:00");
    back["msg"]["customers"][0]["email"] = json!("new.buyer@example.com");
    back["msg"]["customers"][0]["phone_number"] = Value::Null;
    a.post_as(&h, &back.to_string(), &json!({}), &json!({}), &a.secret).await;
    sqlx::query("UPDATE integrations SET state = '{}' WHERE id = $1::uuid").bind(&a.id).execute(&h.state.db).await.unwrap();
    let mut late = fixture("aftership", "out-for-delivery.json")["input"]["payload"].clone();
    late["event_id"] = json!("late-ofd");
    late["msg"]["id"] = json!("22222222222222222222222222222222");
    a.post_as(&h, &late.to_string(), &json!({}), &json!({}), &a.secret).await;
    assert!(sent_to(&h, "Your order arrives today").is_empty(), "back in transit after it: no 'arrives today'");
    // Another shipment, in order: out for delivery reaches the buyer, once.
    let mut other = fixture("aftership", "out-for-delivery.json")["input"]["payload"].clone();
    other["event_id"] = json!("f4dadd60-0000-46d0-aa52-3ced925a50ff");
    other["msg"]["id"] = json!("11111111111111111111111111111111");
    a.post_as(&h, &other.to_string(), &json!({}), &json!({}), &a.secret).await;
    assert_eq!(sent_to(&h, "Your order arrives today"), vec!["sky.example@example.com"], "a service message, once");
    // Only the configured role: a customer without it gets nothing (no "first non-sender").
    let before = events(&h, "Out For Delivery").await;
    a.post(&h, "no-buyer.json").await;
    assert_eq!(events(&h, "Out For Delivery").await, before);
    // A buyer the integration added is held back from marketing.
    assert_eq!(consent(&h, "new.buyer@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "new.buyer@example.com").await);
    never_stored(&mut h, &a, &sensitive("aftership", &["in-transit.json"]), &["in-transit.json"]).await;
    h.cleanup().await;
}

#[tokio::test]
async fn segment_identify_then_track() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let s = install(&mut h, "segment", Sign::Sha1Hex("X-Signature"), "segment-secret-0001", false).await;
    live_flow(&mut h, "Order Completed", "Thanks from Segment", true).await;
    s.refused(&h, "identify.json").await;
    // The track arrives before the identify that says who the userId is: it waits.
    s.post(&h, "track-order-completed.json").await;
    s.post(&h, "identify.json").await;
    retry_now(&h).await;
    s.post(&h, "track-unlisted.json").await;
    s.post(&h, "track-from-browser.json").await;
    // The same userId with a new email: the same profile's address changes; the userId is never on two profiles.
    s.post(&h, "identify-new-email.json").await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE integration_keys->'segment_user_id' ? '019mr8mf4r'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'achilles.new@example.com' AND external_id = 'segment:019mr8mf4r'").await, 1);
    s.post(&h, "page.json").await;
    let (_, again) = s.post_as(&h, &fixture("segment", "track-order-completed.json")["input"]["payload"].to_string(), &json!({}), &json!({}), &s.secret).await;
    assert!(again.contains("duplicate"), "{again}");
    assert_eq!(values(&h, "Order Completed", "achilles.new@example.com").await, vec![39.95], "the order stays with the person (now at the new address)");
    assert_eq!(count(&h, "SELECT count(*) FROM metrics WHERE name = 'Purchased an Item'").await, 0);
    assert_eq!(consent(&h, "achilles.new@example.com").await, (true, "never_subscribed".into()));
    assert!(!campaign_reaches(&mut h, "achilles.new@example.com").await);
    assert_eq!(sent_to(&h, "Thanks from Segment"), vec!["achilles.example@example.com"]);
    never_stored(&mut h, &s, &sensitive("segment", &["identify.json"]), &["identify.json"]).await;
    h.cleanup().await;
}

// ---------- Framework: signature settings fail closed ----------

#[tokio::test]
async fn unreadable_signature_settings_and_empty_keys_refuse_every_webhook() {
    let mut h = integ_harness!();
    h.signup().await;
    let script = "function handle(i) { return [{type: 'upsert_profile', email: i.payload.email}] }";
    let i = h.ok("POST", "/api/integrations", Some(json!({"name": "Signed app", "script": script, "signature": {"type": "hmac", "header": "X-Sig"}, "signature_secret": "a-long-enough-secret"}))).await;
    let id = i["id"].as_str().unwrap().to_string();
    let url = i["inbound_url"].as_str().unwrap();
    let path = &url[url.find("/webhooks/in/").unwrap()..];
    // Stored settings that no longer parse (an older or hand-edited row) are not "no check".
    sqlx::query("UPDATE integrations SET signature = '{\"type\": \"hmac\"}' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    let (s, _, _) = h.raw("POST", path, &[("content-type", "application/json")], r#"{"email": "anyone@example.com"}"#).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'anyone@example.com'").await, 0);
    let v = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(v["signature_ready"], false, "{v}");
    assert!(v["signature_error"].is_string(), "{v}");
    let list = h.ok("GET", "/api/integrations", None).await;
    assert_eq!(list["data"][0]["signature_ready"], false);
    let log = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await;
    assert!(log["data"][0]["error"].as_str().unwrap().contains("can't be read"), "{log}");
    // A whsec_ secret that decodes to an empty key never verifies, even with a matching empty-key HMAC.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"signature": {"type": "hmac", "header": "svix-signature", "format": "svix"}, "signature_secret": "whsec_"}))).await;
    let body = r#"{"email": "anyone@example.com"}"#;
    let ts = chrono::Utc::now().timestamp().to_string();
    let mac = b64(&mac256(b"", format!("msg_1.{ts}.{body}").as_bytes()));
    let sig = format!("v1,{mac}");
    let (s, _, _) = h.raw("POST", path, &[("svix-id", "msg_1"), ("svix-timestamp", &ts), ("svix-signature", &sig), ("content-type", "application/json")], body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.cleanup().await;
}

/// BigCommerce wired to the stand-in store; returns the installed recipe.
async fn bigcommerce_with_mock(h: &mut Harness, base: &str) -> Installed {
    let b = install(h, "bigcommerce", Sign::Header("X-Webhook-Token"), "bc-webhook-token-0001", false).await;
    let i = h.ok("GET", &format!("/api/integrations/{}", b.id), None).await;
    let mut pull = i["pull"].clone();
    pull["url"] = json!(format!("{base}/stores/{{{{settings.store_hash}}}}/v2/orders"));
    h.ok("PATCH", &format!("/api/integrations/{}", b.id), Some(json!({"pull": pull, "settings": {"store_hash": "abc123"}, "credentials": {"access_token": "bc-access-token-0001"}}))).await;
    b
}

fn order_reads(store: &MockStore, id: &str) -> usize {
    store.seen.lock().unwrap().iter().filter(|(_, p)| p == &format!("/stores/abc123/v2/orders/{id}")).count()
}

#[tokio::test]
async fn a_fetch_is_reserved_once_and_waits_for_room_and_the_vendors_cooldown() {
    let mut h = integ_harness!();
    h.signup().await;
    let (store, base) = mock_bigcommerce().await;
    let b = bigcommerce_with_mock(&mut h, &base).await;
    let account: uuid::Uuid = sqlx::query_scalar("SELECT account_id FROM integrations WHERE id = $1::uuid").bind(&b.id).fetch_one(&h.state.db).await.unwrap();
    // No room in the payload budget: the fetch is reserved and counted, but no request is made.
    dumbmail::features::integrations::engine::set_payload_budget_for(account, 1000);
    b.post(&h, "webhook-order-created.json").await;
    assert_eq!(order_reads(&store, "218"), 0, "no request while there's no room for the answer");
    let open = "SELECT count(*) FROM integration_deliveries WHERE source = 'fetch' AND status = 'pending'";
    assert_eq!(count(&h, open).await, 1, "the intent is outstanding work");
    // A second webhook for the same order (another status change) reuses the waiting intent.
    b.post(&h, "webhook-order-status-updated.json").await;
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE source = 'fetch'").await, 1);
    // Room again, and the job queued twice: one request.
    dumbmail::features::integrations::engine::set_payload_budget_for(account, 256 * 1024 * 1024);
    let intent: uuid::Uuid = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE source = 'fetch'").fetch_one(&h.state.db).await.unwrap();
    dumbmail::jobs::enqueue_background(&h.state.db, "integrations_fetch", json!({"delivery_id": intent}), None).await.unwrap();
    retry_now(&h).await;
    assert_eq!(order_reads(&store, "218"), 1, "{:?}", store.seen.lock().unwrap());
    assert_eq!(events(&h, "Placed Order").await, 1);
    // Rate limited: the next try waits for BigCommerce's reset time, not the usual minute.
    store.limited.store(1, std::sync::atomic::Ordering::SeqCst);
    let mut body = fixture("bigcommerce", "webhook-order-created.json")["input"]["payload"].clone();
    body["data"]["id"] = json!(219);
    body["hash"] = json!("0000000000000000000000000000000000000219");
    b.post_as(&h, &body.to_string(), &json!({}), &json!({}), &b.secret).await;
    let wait: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM run_at - now())::float8 FROM jobs WHERE kind = 'integrations_fetch' ORDER BY run_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert!((10.0..20.0).contains(&wait), "rescheduled after the 15-second reset: {wait}");
    retry_now(&h).await;
    assert_eq!(order_reads(&store, "219"), 2);
    assert_eq!(events(&h, "Placed Order").await, 2);
    // The context a webhook carries isn't visible in the delivery's request record.
    b.post(&h, "webhook-refund-created.json").await;
    let requests: String = sqlx::query_scalar("SELECT string_agg(request::text, ' ') FROM integration_deliveries WHERE source = 'fetch'").fetch_one(&h.state.db).await.unwrap();
    assert!(!requests.contains("webhook_order_id") && !requests.contains("Customer requires a refund"), "{requests}");
    h.cleanup().await;
}

#[tokio::test]
async fn webhook_fetches_are_read_only_lookups_of_declared_paths() {
    let mut h = integ_harness!();
    h.signup().await;
    let (store, base) = mock_bigcommerce().await;
    let script = "function handle(i) { if (i.source === 'fetch') return [{type: 'log', message: 'got ' + i.request.url}]; return [Object.assign({type: 'next_request'}, i.payload.next)] }";
    let pull = json!({"url": format!("{base}/stores/{{{{settings.store_hash}}}}/v2/orders"), "auth": {"type": "header", "name": "X-Auth-Token", "value": "{{secret.access_token}}"},
        "fetch": {"paths": ["/stores/{{settings.store_hash}}/v2/orders/{id}", "/stores/{{settings.store_hash}}/v3/customers"], "query": ["id:in"], "context": ["kept"]}});
    let i = h.ok("POST", "/api/integrations", Some(json!({"name": "Lookups", "script": script, "pull": pull, "settings": {"store_hash": "abc123"}, "credentials": {"access_token": "bc-access-token-0001"}}))).await;
    let url = i["inbound_url"].as_str().unwrap();
    let path = url[url.find("/webhooks/in/").unwrap()..].to_string();
    let send = |next: Value| {
        let path = path.clone();
        let body = json!({"next": next}).to_string();
        async move { (path, body) }
    };
    let refused = [
        json!({"url": "/stores/abc123/v2/orders/218", "method": "POST"}),
        json!({"url": "/stores/abc123/v2/orders/218", "body": {"status_id": 5}}),
        json!({"url": "/stores/abc123/v2/orders/218", "headers": {"X-Other": "1"}}),
        json!({"url": "/stores/abc123/v2/orders/218/coupons"}),
        json!({"url": "/stores/abc123/v2/orders/abc"}),
        json!({"url": "/stores/abc123/v2/orders/../customers"}),
        json!({"url": "/stores/zzz999/v2/orders/218"}),
        json!({"url": "https://elsewhere.example/stores/abc123/v2/orders/218"}),
        json!({"url": "/stores/abc123/v3/customers", "query": {"email:in": "a@example.com"}}),
        json!({"url": "/stores/abc123/v3/customers", "query": {"id:in": "1 OR 1=1"}}),
    ];
    for next in refused.iter() {
        let (p, body) = send(next.clone()).await;
        let (s, _, _) = h.raw("POST", &p, &[("content-type", "application/json")], &body).await;
        assert_eq!(s, StatusCode::OK);
        h.drain().await;
    }
    assert_eq!(store.seen.lock().unwrap().len(), 0, "nothing was requested: {:?}", store.seen.lock().unwrap());
    let errors: Vec<Option<String>> = sqlx::query_scalar("SELECT error FROM integration_deliveries WHERE source = 'webhook' ORDER BY created_at").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(errors.len(), refused.len());
    assert!(errors.iter().all(|e| e.as_deref().is_some_and(|e| e.contains("fetch"))), "{errors:?}");
    // An allowed lookup goes out as a GET, and only declared context keys survive.
    let (p, body) = send(json!({"url": "/stores/abc123/v3/customers", "query": {"id:in": "32"}, "context": {"kept": 1, "dropped": "x"}})).await;
    h.raw("POST", &p, &[("content-type", "application/json")], &body).await;
    h.drain().await;
    assert_eq!(store.seen.lock().unwrap().iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(), vec!["/stores/abc123/v3/customers?id%3Ain=32"]);
    h.cleanup().await;
}

// ---------- Shared: a new phone number hasn't agreed to texts ----------

#[tokio::test]
async fn patching_a_profiles_phone_leaves_the_sms_subscription_with_the_old_number() {
    let mut h = integ_harness!();
    h.signup().await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "pat@example.com", "phone_number": "+15035550100", "sms_consent": "subscribed"}))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let same = h.ok("PATCH", &format!("/api/profiles/{id}"), Some(json!({"phone_number": "+1 (503) 555-0100"}))).await;
    assert_eq!(same["sms_consent"], "subscribed", "the same number, written differently: unchanged");
    let moved = h.ok("PATCH", &format!("/api/profiles/{id}"), Some(json!({"phone_number": "+15035550199"}))).await;
    assert_eq!((moved["phone_number"].as_str(), moved["sms_consent"].as_str()), (Some("+15035550199"), Some("never_subscribed")));
    // Setting the consent in the same request applies it to the new number.
    let opted = h.ok("PATCH", &format!("/api/profiles/{id}"), Some(json!({"phone_number": "+15035550142", "sms_consent": "subscribed"}))).await;
    assert_eq!(opted["sms_consent"], "subscribed");
    h.cleanup().await;
}

// ---------- Allow-lists keep only named leaves; dry runs see what would be stored ----------

#[tokio::test]
async fn keep_lists_keep_only_named_leaves_everywhere() {
    let mut h = integ_harness!();
    h.signup().await;
    // Webflow: a mapped field whose value is an object keeps nothing nested in it.
    let w = install(&mut h, "webflow", Sign::Webflow, "webflow-secret-0001", false).await;
    h.ok("PATCH", &format!("/api/integrations/{}", w.id), Some(json!({"settings": {"email_field": "Email", "copy_fields": ["Interest"]}}))).await;
    let mut body = fixture("webflow", "form-submission.json")["input"]["payload"].clone();
    body["payload"]["data"]["Interest"] = json!({"label": "Weekend stays", "note": "NESTED-SECRET-1"});
    let (s, _) = w.post_as(&h, &body.to_string(), &json!({}), &json!({}), &w.secret).await;
    assert_eq!(s, StatusCode::OK);
    // Typeform: a selected answer with something nested beside the named leaves.
    let t = install(&mut h, "typeform", Sign::Base64Prefixed("Typeform-Signature", "sha256="), "typeform-secret-0001", false).await;
    h.ok("PATCH", &format!("/api/integrations/{}", t.id), Some(json!({"settings": {"copy_fields": ["readable_ref_choice"]}}))).await;
    let mut tf = fixture("typeform", "form-response.json")["input"]["payload"].clone();
    tf["form_response"]["answers"][2]["choice"] = json!({"label": "London", "extra": {"deep": "NESTED-SECRET-2"}});
    let (s, _) = t.post_as(&h, &tf.to_string(), &json!({}), &json!({}), &t.secret).await;
    assert_eq!(s, StatusCode::OK);
    // A custom keep list whose only selector is unset keeps nothing (it never turns off).
    let script = "function handle(i) { return [{type: 'log', message: 'saw ' + JSON.stringify(i.payload)}] }";
    let c = h.ok("POST", "/api/integrations", Some(json!({"name": "Keep nothing", "script": script, "redact": ["keep:data.{setting:field}"]}))).await;
    let url = c["inbound_url"].as_str().unwrap();
    let path = &url[url.find("/webhooks/in/").unwrap()..];
    let (s, _, _) = h.raw("POST", path, &[("content-type", "application/json")], r#"{"data": {"secret": "UNSET-SECRET-3"}}"#).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let everything: String = sqlx::query_scalar("SELECT COALESCE(string_agg(concat_ws(' ', body, result::text, actions::text), ' '), '') FROM integration_deliveries").fetch_one(&h.state.db).await.unwrap();
    let written: String = sqlx::query_scalar("SELECT COALESCE(string_agg(properties::text, ' '), '') FROM events").fetch_one(&h.state.db).await.unwrap();
    for n in ["NESTED-SECRET-1", "NESTED-SECRET-2", "UNSET-SECRET-3"] {
        assert!(!everything.contains(n), "{n} was stored: {everything}");
        assert!(!written.contains(n), "{n} reached an event");
    }
    assert!(everything.contains("saw {}"), "the script ran on the kept (empty) body: {everything}");
    // A dry run gives the script the same projected input, so its logs can't show more.
    let peek = "function handle(i) { return [{type: 'log', message: JSON.stringify(i.payload)}] }";
    let out = h.ok("POST", &format!("/api/integrations/{}/test", w.id), Some(json!({"payload": body, "script": peek}))).await;
    assert_eq!(out["ok"], true, "{out}");
    let shown = out.to_string();
    assert!(!shown.contains("NESTED-SECRET-1") && !shown.contains("503 555 0177"), "the dry run saw what would be stored only: {shown}");
    assert!(!shown.contains("Weekend stays"), "an object value keeps none of its leaves unless they're named: {shown}");
    assert!(shown.contains("Email"), "{shown}");
    h.cleanup().await;
}

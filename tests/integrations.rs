//! Custom integrations end to end: inbound webhooks, scripts, the delivery log, retries and
//! replays, consent and relay-address suppression, pulls with OAuth token reuse, backfills,
//! SSRF guards, recipes, event-date flow triggers and the MCP tools.

#[macro_use]
mod common;
use common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use base64::Engine;
use hmac::Mac;
use serde_json::{json, Value};

fn hmac_hex(secret: &str, body: &str) -> String {
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    m.update(body.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

fn path_of(integration: &Value) -> String {
    let url = integration["inbound_url"].as_str().unwrap();
    url[url.find("/webhooks/in/").unwrap()..].to_string()
}

async fn account_id(h: &Harness) -> uuid::Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

const SHIP_SCRIPT: &str = r#"
function handle(input) {
  var p = input.payload;
  if (p.kind === 'skip') return [];
  if (p.bad) throw new Error('bad order ' + p.id);
  return [
    {type: 'upsert_profile', email: p.email, first_name: p.first, properties: {plan: p.plan}},
    {type: 'track_event', metric: 'Order Shipped', profile: {email: p.email}, value: Number(p.total), unique_id: 'ship:' + p.id,
     properties: {order_id: p.id, carrier: input.headers['x-carrier'] || null}},
    {type: 'add_to_list', list: 'Customers', profile: {email: p.email}},
    {type: 'set_state', values: {last_id: p.id}},
    {type: 'log', message: 'shipped ' + p.id}
  ];
}"#;

async fn ship_integration(h: &mut Harness) -> Value {
    h.ok("POST", "/api/lists", Some(json!({"name": "Customers"}))).await;
    h.ok(
        "POST",
        "/api/integrations",
        Some(json!({"name": "Shipping", "script": SHIP_SCRIPT, "signature": {"type": "hmac", "header": "X-Sig", "prefix": "sha256="}, "signature_secret": "ship-secret-123456"})),
    )
    .await
}

#[tokio::test]
async fn webhooks_scripts_and_delivery_log() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let integ = ship_integration(&mut h).await;
    let id = integ["id"].as_str().unwrap().to_string();
    let path = path_of(&integ);
    assert!(path.len() > 50, "the token is long: {path}");
    // The secret is never returned, only a hint.
    assert_eq!(integ["signature_secret"], json!({"saved": true, "hint": "…3456"}));
    assert!(!integ.to_string().contains("ship-secret-123456"));

    let body = json!({"id": "o-1", "email": "Ann@Example.com", "first": "Ann", "plan": "gold", "total": 42.5}).to_string();
    let sig = format!("sha256={}", hmac_hex("ship-secret-123456", &body));
    let (s, _, r) = h.raw("POST", &path, &[("x-sig", &sig), ("content-type", "application/json"), ("x-carrier", "UPS"), ("authorization", "Bearer leak")], &body).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    h.drain().await;

    let (source, consent, props): (Option<String>, String, Value) = sqlx::query_as("SELECT source, email_consent, properties FROM profiles WHERE email = 'ann@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(source.as_deref(), Some(format!("integration:{id}").as_str()));
    assert_eq!(consent, "never_subscribed", "no consent block: consent untouched");
    assert_eq!(props["plan"], "gold");
    let (value, ev_props): (Option<f64>, Value) = sqlx::query_as("SELECT e.value, e.properties FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Order Shipped'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(value, Some(42.5));
    assert_eq!(ev_props["carrier"], "UPS", "headers reach the script");
    assert_eq!(count(&h, "SELECT count(*) FROM list_members lm JOIN lists l ON l.id = lm.list_id WHERE l.name = 'Customers'").await, 1);

    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["state"], json!({"last_id": "o-1"}));
    assert_eq!(got["deliveries_ok"], 1);
    let log = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await;
    assert_eq!(log["total"], 1);
    assert_eq!(log["data"][0]["status"], "ok");
    assert_eq!(log["data"][0]["counts"]["events"], 1);
    let did = log["data"][0]["id"].as_str().unwrap();
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries/{did}"), None).await;
    assert_eq!(d["payload"]["id"], "o-1");
    assert!(d["headers"].get("authorization").is_none(), "credentials aren't stored: {}", d["headers"]);
    assert!(d["result"]["logs"][0].as_str().unwrap().contains("shipped o-1"));

    // A resent webhook doesn't double-count the event.
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", &sig), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Order Shipped'").await, 1);

    // Form-encoded bodies work too.
    let form = "id=o-2&email=bo%40example.com&first=Bo&plan=silver&total=10";
    let fsig = format!("sha256={}", hmac_hex("ship-secret-123456", form));
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", &fsig), ("content-type", "application/x-www-form-urlencoded")], form).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'bo@example.com'").await, 1);

    // Nothing to do: ignored.
    let skip = json!({"kind": "skip"}).to_string();
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", &format!("sha256={}", hmac_hex("ship-secret-123456", &skip)))], &skip).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let ignored = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=ignored"), None).await;
    assert_eq!(ignored["total"], 1);

    // Bad or missing signatures are refused and logged without the body.
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", "sha256=00ff")], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = h.raw("POST", &path, &[], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let bad = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=error"), None).await;
    assert_eq!(bad["total"], 2);
    assert_eq!(bad["data"][0]["has_payload"], false);
    // Unknown URLs and oversized bodies.
    let (s, _, _) = h.raw("POST", "/webhooks/in/in_nope", &[], "{}").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let big = format!("{{\"x\":\"{}\"}}", "a".repeat(1024 * 1024 + 10));
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", "sha256=00")], &big).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);

    // Rotating the URL retires the old one.
    let rotated = h.ok("POST", &format!("/api/integrations/{id}/rotate-url"), None).await;
    assert_ne!(path_of(&rotated), path);
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", &sig)], &body).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM profiles WHERE account_id = '{acct}'")).await, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn failures_retry_then_replay() {
    let mut h = harness!();
    h.signup().await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Orders", "script": SHIP_SCRIPT}))).await;
    h.ok("POST", "/api/lists", Some(json!({"name": "Customers"}))).await;
    let id = integ["id"].as_str().unwrap().to_string();
    let path = path_of(&integ);
    let body = json!({"id": "o-9", "email": "cy@example.com", "bad": true, "total": 5}).to_string();
    let (s, _, _) = h.raw("POST", &path, &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let d = &h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await["data"][0];
    assert_eq!(d["status"], "error");
    assert!(d["error"].as_str().unwrap().contains("bad order o-9"), "{d}");
    assert_eq!(d["attempts"], 1);
    assert!(!d["next_attempt_at"].is_null(), "it will retry");
    // Retries back off, then it stays failed.
    for _ in 0..6 {
        h.fast_forward().await;
    }
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await["data"][0].clone();
    assert_eq!(d["attempts"], 5);
    assert!(d["next_attempt_at"].is_null());
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["deliveries_failed"], 1);
    assert!(got["last_error"].as_str().unwrap().contains("bad order"));

    // Fix the script, then replay everything that failed.
    let fixed = SHIP_SCRIPT.replace("if (p.bad) throw new Error('bad order ' + p.id);", "");
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"script": fixed}))).await;
    let r = h.ok("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"all_failed": true}))).await;
    assert_eq!(r["replayed"], 1);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'cy@example.com'").await, 1);
    let replays = h.ok("GET", &format!("/api/integrations/{id}/deliveries?source=replay"), None).await;
    assert_eq!(replays["data"][0]["status"], "ok");
    // Already replayed: not picked again.
    let again = h.ok("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"all_failed": true}))).await;
    assert_eq!(again["replayed"], 0);

    // A bad action names its index and writes nothing.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"script": "function handle(i) { return [{type: 'upsert_profile', email: 'dz@example.com'}, {type: 'log', message: 'x', colour: 'red'}] }"}))).await;
    h.raw("POST", &path, &[], "{}").await;
    h.drain().await;
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=error"), None).await["data"][0].clone();
    assert!(d["error"].as_str().unwrap().contains("action 1 (log): unknown field `colour`"), "{d}");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'dz@example.com'").await, 0);

    // A runaway script is stopped, and the worker moves on.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"script": "function handle(i) { while (true) {} }"}))).await;
    h.raw("POST", &path, &[], "{}").await;
    let t = std::time::Instant::now();
    h.drain().await;
    assert!(t.elapsed() < std::time::Duration::from_secs(15));
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=error"), None).await["data"][0].clone();
    assert!(d["error"].as_str().unwrap().contains("longer than"), "{d}");

    // Turned off: webhooks are kept as ignored, and can be replayed once it's back on.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"script": fixed}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/status"), Some(json!({"status": "disabled"}))).await;
    let body = json!({"id": "o-10", "email": "eve@example.com", "total": 1}).to_string();
    let (s, _, _) = h.raw("POST", &path, &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'eve@example.com'").await, 0);
    let ign = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=ignored"), None).await["data"][0].clone();
    let (s, _) = h.req("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"delivery_ids": [ign["id"]]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "turn it on first");
    h.ok("POST", &format!("/api/integrations/{id}/status"), Some(json!({"status": "enabled"}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"delivery_ids": [ign["id"]]}))).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'eve@example.com'").await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn consent_relays_and_suppression() {
    let mut h = harness!();
    h.signup().await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": "function handle(i) { return i.payload.actions }"}))).await;
    let id = integ["id"].as_str().unwrap().to_string();
    let path = path_of(&integ);
    let send = |actions: Value| json!({"actions": actions}).to_string();

    // An existing subscriber and an unsubscriber, created outside the integration.
    let sub = h.ok("POST", "/api/profiles", Some(json!({"email": "sub@example.com", "email_consent": "subscribed"}))).await;
    let unsub = h.ok("POST", "/api/profiles", Some(json!({"email": "gone@example.com", "email_consent": "unsubscribed"}))).await;
    let api_relay = h.ok("POST", "/api/profiles", Some(json!({"email": "old-xyz@guest.airbnb.com"}))).await;

    let body = send(json!([
        {"type": "upsert_profile", "email": "guest-abc@guest.airbnb.com", "first_name": "Sam"},
        {"type": "upsert_profile", "email": "x@messages.homeaway.com"},
        {"type": "upsert_profile", "email": "old-xyz@guest.airbnb.com"},
        {"type": "upsert_profile", "email": "quiet@example.com", "marketing": false},
        {"type": "upsert_profile", "email": "new@example.com", "consent": [{"channel": "email", "subscribed": true, "source": "Booking form checkbox"}]},
        {"type": "upsert_profile", "email": "gone@example.com", "consent": [{"channel": "email", "subscribed": true, "source": "Booking form checkbox"}]},
        {"type": "upsert_profile", "email": "sub@example.com", "first_name": "Sue"},
        {"type": "upsert_profile", "email": "plain@example.com"},
        {"type": "unsubscribe", "channel": "email", "profile": {"email": "plain@example.com"}},
        {"type": "suppress", "email": "never@example.com"},
        {"type": "track_event", "metric": "Stay", "profile": {"property": "nobody", "value": "x"}}
    ]));
    let (s, _, _) = h.raw("POST", &path, &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let row = |email: &'static str| {
        let db = h.state.db.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>("SELECT email_consent, email_suppressed_reason, source FROM profiles WHERE email = $1")
                .bind(email)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    let tag = format!("integration:{id}");
    assert_eq!(row("guest-abc@guest.airbnb.com").await, ("never_subscribed".into(), Some("booking_relay".into()), Some(tag.clone())));
    assert_eq!(row("x@messages.homeaway.com").await.1.as_deref(), Some("booking_relay"));
    // A profile the API made earlier is suppressed too, but keeps its own source.
    assert_eq!(row("old-xyz@guest.airbnb.com").await, ("never_subscribed".into(), Some("booking_relay".into()), None));
    assert_eq!(row("quiet@example.com").await.1.as_deref(), Some("no_marketing"));
    assert_eq!(row("new@example.com").await.0, "subscribed");
    assert_eq!(row("gone@example.com").await.0, "unsubscribed", "an unsubscribe always wins");
    assert_eq!(row("sub@example.com").await.0, "subscribed", "no consent block leaves consent alone");
    assert_eq!(row("plain@example.com").await.0, "unsubscribed");
    assert_eq!(row("never@example.com").await.1.as_deref(), Some("no_marketing"));
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Subscribed to Email Marketing' AND e.properties->>'source' = 'Booking form checkbox'").await, 1);
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await["data"][0].clone();
    assert_eq!(d["counts"]["skipped"], 1, "the property lookup found nobody: {d}");

    // The reason and source show on the profile.
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'guest-abc@guest.airbnb.com'").fetch_one(&h.state.db).await.unwrap();
    let detail = h.ok("GET", &format!("/api/profiles/{pid}"), None).await;
    assert_eq!(detail["email_suppressed_reason"], "booking_relay");
    assert_eq!(detail["source"]["name"], "Bookings");

    // Campaigns and segments see the suppression.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Everyone"}))).await;
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM profiles").fetch_all(&h.state.db).await.unwrap();
    dumbmail::audience::add_to_list(&h.state.db, account_id(&h).await, list["id"].as_str().unwrap().parse().unwrap(), &ids).await.unwrap();
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}}))).await;
    // sub, new and the two never_subscribed non-relay profiles made by the API... only addresses with
    // no suppression and not unsubscribed: sub, new.
    assert_eq!(est["count"], 2, "{est}");
    let _ = (sub, unsub, api_relay);
    h.cleanup().await;
}

#[tokio::test]
async fn accounts_are_isolated() {
    let mut h = harness!();
    h.signup().await;
    let a = ship_integration(&mut h).await;
    let a_id = a["id"].as_str().unwrap().to_string();
    let a_cookie = h.cookie.clone();

    h.cookie = None;
    let (s, _) = h.req("GET", "/api/integrations", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "B Co"}))).await;
    let b_list = h.ok("POST", "/api/lists", Some(json!({"name": "B list"}))).await;
    assert_eq!(h.ok("GET", "/api/integrations", None).await["data"], json!([]));
    for (m, p) in [("GET", format!("/api/integrations/{a_id}")), ("GET", format!("/api/integrations/{a_id}/deliveries")), ("DELETE", format!("/api/integrations/{a_id}")), ("POST", format!("/api/integrations/{a_id}/rotate-url"))] {
        let (s, _) = h.req(m, &p, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {p}");
    }
    let (s, _) = h.req("PATCH", &format!("/api/integrations/{a_id}"), Some(json!({"name": "mine now"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A's script can't reach B's list.
    h.cookie = a_cookie;
    let script = format!("function handle(i) {{ return [{{type: 'add_to_list', list_id: '{}', profile: {{email: 'z@example.com'}}}}] }}", b_list["id"].as_str().unwrap());
    h.ok("PATCH", &format!("/api/integrations/{a_id}"), Some(json!({"script": script, "signature": {"type": "none"}}))).await;
    h.raw("POST", &path_of(&a), &[], "{}").await;
    h.drain().await;
    let d = h.ok("GET", &format!("/api/integrations/{a_id}/deliveries"), None).await["data"][0].clone();
    assert!(d["error"].as_str().unwrap().contains("no list with id"), "{d}");
    assert_eq!(count(&h, "SELECT count(*) FROM list_members").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'z@example.com'").await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn test_runs_write_nothing_and_webhooks_are_rate_limited() {
    let mut h = harness!();
    h.signup().await;
    let integ = ship_integration(&mut h).await;
    let id = integ["id"].as_str().unwrap().to_string();
    let r = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"payload": {"id": "t1", "email": "t@example.com", "total": 3}, "headers": {"x-carrier": "DHL"}}))).await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["would_write"]["profiles_created"], 1);
    assert_eq!(r["would_write"]["events"], 1);
    assert!(r["effects"][1].as_str().unwrap().contains("would record Order Shipped"), "{r}");
    assert_eq!(r["state_after"], json!({"last_id": "t1"}));
    assert_eq!(r["actions"][1]["properties"]["carrier"], "DHL");
    // A draft script can be tried without saving it.
    let r = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"payload": {}, "script": "function handle(i) { return [{type: 'nope'}] }"}))).await;
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("unknown type"));
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM events").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries").await, 0);
    assert_eq!(h.ok("GET", &format!("/api/integrations/{id}"), None).await["state"], json!({}));

    // Past the per-minute limit the sender gets 429.
    let open = h.ok("POST", "/api/integrations", Some(json!({"name": "Open", "script": "function handle(i) { return [] }"}))).await;
    let path = path_of(&open);
    for i in 0..dumbmail::features::integrations::api::RATE_PER_MINUTE {
        let (s, _, _) = h.raw("POST", &path, &[], "{}").await;
        assert_eq!(s, StatusCode::OK, "request {i}");
    }
    let (s, hdrs, _) = h.raw("POST", &path, &[], "{}").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hdrs.get("retry-after").unwrap(), "60");
    h.cleanup().await;
}

// ---------- Pulls ----------

#[derive(Default)]
struct MockGuesty {
    tokens: AtomicUsize,
    reject: Mutex<bool>,
    seen: Mutex<Vec<(String, String)>>,
}

async fn mock_guesty(total: usize) -> (Arc<MockGuesty>, String) {
    use axum::extract::{Query, State};
    use axum::routing::{get, post};
    use std::collections::HashMap;
    let m: Arc<MockGuesty> = Default::default();
    let app = axum::Router::new()
        .route(
            "/oauth2/token",
            post(|State(m): State<Arc<MockGuesty>>, axum::Form(f): axum::Form<HashMap<String, String>>| async move {
                assert_eq!(f.get("grant_type").map(String::as_str), Some("client_credentials"));
                assert_eq!(f.get("scope").map(String::as_str), Some("open-api"));
                assert_eq!(f.get("client_secret").map(String::as_str), Some("cs-secret-value"));
                let n = m.tokens.fetch_add(1, Ordering::SeqCst) + 1;
                axum::Json(json!({"token_type": "Bearer", "expires_in": 86400, "access_token": format!("tok-{n}"), "scope": "open-api"}))
            }),
        )
        .route(
            "/v1/reservations",
            get(move |State(m): State<Arc<MockGuesty>>, headers: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                m.seen.lock().unwrap().push((auth.clone(), q.get("filters").cloned().unwrap_or_default()));
                if *m.reject.lock().unwrap() || !auth.starts_with("Bearer tok-") {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid token"})));
                }
                let skip: usize = q.get("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
                let limit: usize = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(25);
                let results: Vec<Value> = (skip..total.min(skip + limit))
                    .map(|i| {
                        let email = if i == 0 { "relay-1@guest.airbnb.com".to_string() } else { format!("guest{i}@example.com") };
                        json!({"_id": format!("res{i}"), "status": "confirmed", "source": "manual", "confirmationCode": format!("C{i}"),
                               "checkInDateLocalized": "2027-01-10", "checkOutDateLocalized": "2027-01-12", "nightsCount": 2, "guestsCount": 2,
                               "listing": {"_id": "L1", "nickname": "Lake House"}, "guest": {"_id": format!("g{i}"), "fullName": format!("Guest {i}"), "email": email},
                               "money": {"totalPaid": 300, "balanceDue": 0, "currency": "USD"}, "integration": {"platform": "manual"},
                               "confirmedAt": "2026-11-17T10:00:00.000Z", "lastUpdatedAt": format!("2026-11-17T10:{:02}:00.000Z", i % 60)})
                    })
                    .collect();
                (StatusCode::OK, axum::Json(json!({"results": results, "count": total, "limit": limit, "skip": skip})))
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
async fn guesty_pull_reuses_its_token_and_backfills() {
    let (mock, base) = mock_guesty(150).await;
    let mut h = harness!();
    h.signup().await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    assert_eq!(created["flows"].as_array().unwrap().len(), 3);
    assert_eq!(created["flows"][0]["status"], "draft", "suggested flows never start on their own");
    let integ = &created["integration"];
    let id = integ["id"].as_str().unwrap().to_string();
    assert_eq!(integ["missing_credentials"], json!(["client_id", "client_secret"]));
    let (s, v) = h.req("POST", &format!("/api/integrations/{id}/pull"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("client_id"));
    // A scheduled pull waits for the credentials without logging errors.
    sqlx::query("UPDATE integrations SET pull_next_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    dumbmail::features::integrations::engine::schedule_pulls(&h.state).await.unwrap();
    h.drain().await;
    assert!(h.ok("GET", &format!("/api/integrations/{id}"), None).await["last_error"].is_null());
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 0);

    // Point the recipe's pull at the stand-in Guesty.
    let mut pull = integ["pull"].clone();
    pull["url"] = json!(format!("{base}/v1/reservations"));
    pull["auth"]["token_url"] = json!(format!("{base}/oauth2/token"));
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": pull, "credentials": {"client_id": "cid-0001", "client_secret": "cs-secret-value"}}))).await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert!(!got.to_string().contains("cs-secret-value"), "credentials are write-only");
    assert_eq!(got["missing_credentials"], json!([]));

    // A test pull fetches a page and writes nothing (except caching the token).
    let t = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(t["ok"], true, "{t}");
    assert_eq!(t["would_write"]["profiles_created"], 100);
    assert_eq!(t["next_request"]["query"]["skip"], 100);
    assert!(t["effects"][0].as_str().unwrap().contains("suppress"), "the relay guest would be suppressed: {}", t["effects"][0]);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 0);
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    assert!(!t["pull"]["request"].to_string().contains("tok-1"), "the token isn't shown");
    // Another test reuses the cached token.
    h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);

    // A real pull pages through all 150 with the same token.
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 150);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Reservation Confirmed'").await, 150);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email_suppressed_reason = 'booking_relay'").await, 1);
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["state"]["cursor"], "2026-11-17T10:59:00.000Z", "{}", got["state"]);
    assert!(got["state"].get("pending_cursor").is_none());
    let pulls = h.ok("GET", &format!("/api/integrations/{id}/deliveries?source=pull"), None).await;
    assert_eq!(pulls["total"], 2, "test pulls aren't logged; the two real pages are: {pulls}");

    // A backfill runs from its own start date, in chained jobs, and reports progress.
    mock.seen.lock().unwrap().clear();
    let b = h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"backfill_since": "2025-01-01"}))).await;
    assert_eq!(b["backfill"]["status"], "running");
    h.drain().await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["backfill"]["status"], "done", "{}", got["backfill"]);
    assert_eq!(got["backfill"]["pages"], 2);
    assert!(mock.seen.lock().unwrap()[0].1.contains("\"from\":\"2025-01-01T00:00:00.000Z\""), "{:?}", mock.seen.lock().unwrap()[0]);
    assert_eq!(got["state"]["cursor"], "2026-11-17T10:59:00.000Z", "a backfill leaves the live cursor alone");
    assert_eq!(count(&h, "SELECT count(*) FROM events").await, 150, "nothing counted twice");
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    // A backfill can be cancelled.
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"backfill_since": "2025-01-01"}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/backfill/cancel"), None).await;
    h.drain().await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["backfill"]["status"], "cancelled");
    assert_eq!(got["backfill"]["pages"], 0);

    // The daily cap: with no valid token and the cap used up, no new token is asked for.
    sqlx::query("UPDATE integrations SET oauth_token_enc = NULL, oauth_token_expires_at = NULL, oauth_token_requests = ARRAY[now(), now(), now(), now()]").execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("daily cap"));
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    // Once the cap allows it, a new token is fetched.
    sqlx::query("UPDATE integrations SET oauth_token_requests = '{}'").execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 2);
    // Re-saving the same credentials keeps the token.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"credentials": {"client_secret": "cs-secret-value"}}))).await;
    assert!(!h.ok("GET", &format!("/api/integrations/{id}"), None).await["oauth_token_cached_until"].is_null());
    // When the app rejects the token, it's dropped and the error is logged.
    *mock.reject.lock().unwrap() = true;
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert!(got["oauth_token_cached_until"].is_null());
    assert!(got["last_error"].as_str().unwrap().contains("HTTP 401"), "{}", got["last_error"]);
    let failed = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=error"), None).await;
    let did = failed["data"][0]["id"].as_str().unwrap();
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries/{did}"), None).await;
    assert!(!d.to_string().contains("tok-2") && !d.to_string().contains("cs-secret-value"), "no secrets in the log");

    // Pulls only reach public addresses.
    for url in ["http://127.0.0.1:9/x", "http://169.254.169.254/latest/meta-data", "http://10.0.0.5/api", "http://localhost/x"] {
        h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": {"url": url, "interval_minutes": 15}}))).await;
        let (s, v) = h.req("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("public internet address"), "{url}: {v}");
    }
    let (s, _) = h.req("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": {"url": "https://x.test", "interval_minutes": 2}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "five minutes is the shortest interval");
    h.cleanup().await;
}

#[tokio::test]
async fn scheduled_pulls_and_retention() {
    let (mock, base) = mock_guesty(3).await;
    let mut h = harness!();
    h.signup().await;
    let script = "function handle(i) { return i.payload.results.map(function (r) { return {type: 'upsert_profile', email: r.guest.email} }) }";
    let integ = h
        .ok("POST", "/api/integrations", Some(json!({"name": "Poll", "script": script, "credentials": {"key": "k"},
            "pull": {"url": format!("{base}/v1/reservations"), "query": {"limit": 10}, "auth": {"type": "bearer", "token": "tok-{{secret.key}}"}, "interval_minutes": 5}})))
        .await;
    let id: uuid::Uuid = integ["id"].as_str().unwrap().parse().unwrap();
    // Due: the scheduler starts it once; not again until the interval passes.
    sqlx::query("UPDATE integrations SET pull_next_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    dumbmail::features::integrations::engine::schedule_pulls(&h.state).await.unwrap();
    dumbmail::features::integrations::engine::schedule_pulls(&h.state).await.unwrap();
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 3);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert_eq!(mock.seen.lock().unwrap()[0].0, "Bearer tok-k");
    let next: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT pull_next_at FROM integrations WHERE id = $1").bind(id).fetch_one(&h.state.db).await.unwrap();
    assert!(next > chrono::Utc::now() + chrono::Duration::minutes(4));
    // Turned off: no scheduled pulls.
    h.ok("POST", &format!("/api/integrations/{id}/status"), Some(json!({"status": "disabled"}))).await;
    sqlx::query("UPDATE integrations SET pull_next_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    dumbmail::features::integrations::engine::schedule_pulls(&h.state).await.unwrap();
    h.drain().await;
    assert_eq!(mock.seen.lock().unwrap().len(), 1);

    // Deliveries older than 30 days are pruned.
    sqlx::query("UPDATE integration_deliveries SET created_at = now() - interval '31 days'").execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/integrations/{id}/status"), Some(json!({"status": "enabled"}))).await;
    h.raw("POST", &path_of(&integ), &[], "{\"results\": []}").await;
    dumbmail::features::integrations::engine::prune(&h.state).await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries").await, 1);
    h.cleanup().await;
}

// ---------- Recipes, end to end ----------

fn fixture(recipe: &str, name: &str) -> Value {
    let p = format!("{}/recipes/{recipe}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str::<Value>(&std::fs::read_to_string(p).unwrap()).unwrap()["input"]["payload"].clone()
}

#[tokio::test]
async fn recipes_check_real_signatures() {
    let mut h = harness!();
    h.signup().await;
    // Guesty signs with Svix.
    let g = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty", "create_flows": false}))).await;
    let gid = g["integration"]["id"].as_str().unwrap().to_string();
    let key = b"guesty-svix-key-0123456789abcdef";
    let secret = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(key));
    h.ok("PATCH", &format!("/api/integrations/{gid}"), Some(json!({"signature_secret": secret}))).await;
    let body = fixture("guesty", "webhook-new-airbnb-relay.json").to_string();
    let ts = chrono::Utc::now().timestamp().to_string();
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).unwrap();
    m.update(format!("msg_2Yx.{ts}.{body}").as_bytes());
    let sig = format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes()));
    let path = path_of(&g["integration"]);
    let (s, _, r) = h.raw("POST", &path, &[("svix-id", "msg_2Yx"), ("svix-timestamp", &ts), ("svix-signature", &sig), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, _, _) = h.raw("POST", &path, &[("svix-id", "msg_other"), ("svix-timestamp", &ts), ("svix-signature", &sig)], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.drain().await;
    let (reason, source): (Option<String>, Option<String>) = sqlx::query_as("SELECT email_suppressed_reason, source FROM profiles WHERE email = 'sam-abc123xyz@guest.airbnb.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(reason.as_deref(), Some("booking_relay"));
    assert_eq!(source, Some(format!("integration:{gid}")));

    // Stripe signs with Stripe-Signature; Checkout's promotions box subscribes, nothing else does.
    let st = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "stripe"}))).await;
    let sid = st["integration"]["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/integrations/{sid}"), Some(json!({"signature_secret": "whsec_stripe_recipe_test"}))).await;
    let spath = path_of(&st["integration"]);
    for name in ["checkout-paid-opt-in.json", "checkout-paid-no-consent.json", "subscription-deleted.json"] {
        let body = fixture("stripe", name).to_string();
        let t = chrono::Utc::now().timestamp();
        let sig = format!("t={t},v1={}", hmac_hex("whsec_stripe_recipe_test", &format!("{t}.{body}")));
        let (s, _, r) = h.raw("POST", &spath, &[("stripe-signature", &sig), ("content-type", "application/json")], &body).await;
        assert_eq!(s, StatusCode::OK, "{name}: {r}");
        h.drain().await;
    }
    let consent: Vec<(String, String)> = sqlx::query_as("SELECT email, email_consent FROM profiles WHERE email IN ('jordan@example.com', 'casey@example.net') ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(consent, vec![("casey@example.net".into(), "never_subscribed".into()), ("jordan@example.com".into(), "subscribed".into())]);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Placed Order'").await, 2);
    // The subscription event found Jordan by the customer id saved from checkout.
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = 'Subscription Cancelled' AND p.email = 'jordan@example.com'").await, 1);
    // The suggested win-back flow is a draft.
    assert_eq!(st["flows"][0]["status"], "draft");
    h.cleanup().await;
}

// ---------- Event-date and cancellation-aware flows ----------

async fn track(h: &mut Harness, metric: &str, email: &str, props: Value, unique: &str, time: &str) {
    h.ok("POST", "/api/events", Some(json!({"metric": metric, "profile": {"email": email}, "properties": props, "unique_id": unique, "time": time}))).await;
}

#[tokio::test]
async fn event_date_flows_follow_bookings() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let flow = h
        .ok(
            "POST",
            "/api/flows",
            Some(json!({"name": "Pre-arrival", "trigger": {"type": "event_date", "metric": "Reservation Confirmed", "field": "check_in", "offset_days": -2, "hour": 0, "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"]},
                "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": "See you at {{ event.properties.listing }}", "blocks": [{"type": "text", "html": "<p>Soon!</p>"}]}}]}})),
        )
        .await;
    let fid = flow["id"].as_str().unwrap().to_string();
    let (s, v) = h.req("POST", "/api/flows", Some(json!({"name": "x", "trigger": {"type": "event_date", "metric": "M", "field": "d", "cancelled_by": ["C"]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "cancelled_by needs a key: {v}");
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;

    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    let target = (today + chrono::Duration::days(2)).format("%Y-%m-%d").to_string();
    let later = (today + chrono::Duration::days(5)).format("%Y-%m-%d").to_string();
    let rc = "Reservation Confirmed";
    // A guest with two bookings gets both.
    track(&mut h, rc, "ann@example.com", json!({"reservation_id": "A", "check_in": target, "listing": "Lake House"}), "A1", "2026-11-01T10:00:00Z").await;
    track(&mut h, rc, "ann@example.com", json!({"reservation_id": "D", "check_in": target, "listing": "Barn"}), "D1", "2026-11-02T10:00:00Z").await;
    // Cancelled later: nothing.
    track(&mut h, rc, "bo@example.com", json!({"reservation_id": "B", "check_in": target, "listing": "Loft"}), "B1", "2026-11-01T10:00:00Z").await;
    track(&mut h, "Reservation Cancelled", "bo@example.com", json!({"reservation_id": "B"}), "B2", "2026-11-03T10:00:00Z").await;
    // Dates moved: the newer event wins, and it isn't today.
    track(&mut h, rc, "cy@example.com", json!({"reservation_id": "C", "check_in": target, "listing": "Cabin"}), "C1", "2026-11-01T10:00:00Z").await;
    track(&mut h, rc, "cy@example.com", json!({"reservation_id": "C", "check_in": later, "listing": "Cabin"}), "C2", "2026-11-04T10:00:00Z").await;
    // A date-time is read in the account's time zone: 11:30pm Pacific is the next day in UTC.
    let la = chrono_tz::America::Los_Angeles;
    let late_local = chrono::TimeZone::from_local_datetime(&la, &(today + chrono::Duration::days(2)).and_hms_opt(23, 30, 0).unwrap()).unwrap().to_rfc3339();
    track(&mut h, rc, "eve@example.com", json!({"reservation_id": "E", "check_in": late_local, "listing": "Dome"}), "E1", "2026-11-01T10:00:00Z").await;

    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.drain().await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.drain().await;
    let sent: Vec<(String, String)> = sqlx::query_as("SELECT to_address, subject FROM messages WHERE flow_id = $1 ORDER BY to_address, subject").bind(fid.parse::<uuid::Uuid>().unwrap()).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(
        sent,
        vec![
            ("ann@example.com".into(), "See you at Barn".into()),
            ("ann@example.com".into(), "See you at Lake House".into()),
            ("eve@example.com".into(), "See you at Dome".into()),
        ],
        "one email per booking, event properties in the email, each event once"
    );
    h.cleanup().await;
}

#[tokio::test]
async fn keyed_flows_stop_when_cancelled() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let flow = h
        .ok(
            "POST",
            "/api/flows",
            Some(json!({"name": "Confirmation", "trigger": {"type": "metric", "metric": "Reservation Confirmed", "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"], "filters": [{"field": "historical", "op": "neq", "value": true}]},
                "definition": {"entry": "d1", "settings": {"reentry": "always"}, "nodes": [
                    {"id": "d1", "type": "delay", "amount": 10, "unit": "minutes", "next": "e1"},
                    {"id": "e1", "type": "email", "next": null, "content": {"subject": "Booked: {{ event.properties.reservation_id }}", "blocks": [{"type": "text", "html": "<p>Thanks</p>"}]}}]}})),
        )
        .await;
    let fid: uuid::Uuid = flow["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(flow["trigger"]["key"], "reservation_id");
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    let now = chrono::Utc::now();
    let t = |mins: i64| (now + chrono::Duration::minutes(mins)).to_rfc3339();
    track(&mut h, "Reservation Confirmed", "g@example.com", json!({"reservation_id": "G"}), "G1", &t(0)).await;
    track(&mut h, "Reservation Confirmed", "k@example.com", json!({"reservation_id": "K"}), "K1", &t(0)).await;
    track(&mut h, "Reservation Confirmed", "old@example.com", json!({"reservation_id": "O", "historical": true}), "O1", &t(0)).await;
    h.drain().await;
    track(&mut h, "Reservation Cancelled", "g@example.com", json!({"reservation_id": "G"}), "G2", &t(1)).await;
    h.fast_forward().await;
    let sent: Vec<String> = sqlx::query_scalar("SELECT subject FROM messages WHERE flow_id = $1").bind(fid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(sent, vec!["Booked: K".to_string()]);
    let statuses: Vec<String> = sqlx::query_scalar("SELECT r.status FROM flow_runs r JOIN profiles p ON p.id = r.profile_id WHERE r.flow_id = $1 ORDER BY p.email").bind(fid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(statuses, vec!["exited".to_string(), "completed".to_string()], "G exited; the historical one never started");
    h.cleanup().await;
}

// ---------- MCP ----------

#[tokio::test]
async fn mcp_tools_and_docs() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Claude"}))).await["key"].as_str().unwrap().to_string();
    let rpc = |method: &str, params: Value| json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let call = |body: String| {
        let router = h.router.clone();
        let auth = format!("Bearer {key}");
        async move {
            let req = Request::builder().method("POST").uri("/mcp").header("authorization", auth).header("content-type", "application/json").body(Body::from(body)).unwrap();
            let res = router.oneshot(req).await.unwrap();
            serde_json::from_slice::<Value>(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
        }
    };
    let tool = |name: &str, args: Value| rpc("tools/call", json!({"name": name, "arguments": args}));
    let list = call(rpc("tools/list", json!({}))).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    for name in ["list_integrations", "get_integration", "create_integration", "update_integration", "test_integration", "list_integration_deliveries", "get_integration_delivery",
        "replay_integration_deliveries", "run_integration_pull", "cancel_integration_backfill", "set_integration_status", "rotate_integration_url", "delete_integration",
        "list_integration_recipes", "create_integration_from_recipe"] {
        let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(t["inputSchema"]["type"], "object");
        fn no_nulls(v: &Value) {
            match v {
                Value::Object(o) => o.iter().for_each(|(k, x)| {
                    assert!(!x.is_null() || k == "default", "null schema value at {k}");
                    no_nulls(x)
                }),
                Value::Array(a) => a.iter().for_each(no_nulls),
                _ => {}
            }
        }
        no_nulls(&t["inputSchema"]);
    }
    let doc = call(rpc("resources/read", json!({"uri": "dumbmail://docs/integrations"}))).await;
    assert!(doc["result"]["contents"][0]["text"].as_str().unwrap().contains("function handle(input)"));

    let recipes = call(tool("list_integration_recipes", json!({}))).await;
    let keys: Vec<&str> = recipes["result"]["structuredContent"]["recipes"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["calendly", "guesty", "stripe"]);
    let made = call(tool("create_integration_from_recipe", json!({"recipe": "calendly"}))).await;
    let sc = &made["result"]["structuredContent"];
    assert!(sc["integration"]["inbound_url"].as_str().unwrap().contains("/webhooks/in/"));
    assert!(sc["setup"].as_array().unwrap().len() >= 3);
    let id = sc["integration"]["id"].as_str().unwrap().to_string();
    let t = call(tool("test_integration", json!({"id": id, "payload": fixture("calendly", "invitee-created.json")}))).await;
    assert_eq!(t["result"]["structuredContent"]["ok"], true, "{t}");
    assert_eq!(t["result"]["structuredContent"]["would_write"]["events"], 1);
    // Confirm-gated tools answer with a dry run first.
    for (name, args) in [("delete_integration", json!({"id": id})), ("rotate_integration_url", json!({"id": id})), ("replay_integration_deliveries", json!({"id": id, "all_failed": true}))] {
        let r = call(tool(name, args)).await;
        assert_eq!(r["result"]["structuredContent"]["dry_run"], true, "{name}: {r}");
    }
    let r = call(tool("run_integration_pull", json!({"id": id}))).await;
    assert_eq!(r["result"]["isError"], true, "calendly has no pull: {r}");
    let up = call(tool("update_integration", json!({"id": id, "signature_secret": "calendly-signing-key-1", "settings": {"x": 1}}))).await;
    assert_eq!(up["result"]["structuredContent"]["signature_secret"]["saved"], true);
    let bad = call(tool("update_integration", json!({"id": id, "nope": 1}))).await;
    assert_eq!(bad["result"]["isError"], true);
    let gone = call(tool("delete_integration", json!({"id": id, "confirm": true}))).await;
    assert_eq!(gone["result"]["structuredContent"]["deleted"], true);
    assert_eq!(call(tool("list_integrations", json!({}))).await["result"]["structuredContent"]["data"], json!([]));
    h.cleanup().await;
}

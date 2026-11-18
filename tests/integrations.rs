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

/// A harness whose integration scripts run in the real sandbox: the built `dumbmail` binary's
/// `sandbox-run` subcommand, as in production.
macro_rules! integ_harness {
    () => {{
        dumbmail::features::integrations::sandbox::set_program(env!("CARGO_BIN_EXE_dumbmail"));
        // Many tests (accounts) run at once in this process; the per-account cap still applies.
        dumbmail::features::integrations::sandbox::set_global_cap(64);
        harness!()
    }};
}

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
        Some(json!({"name": "Shipping", "script": SHIP_SCRIPT, "metrics": ["Order Shipped"], "signature": {"type": "hmac", "header": "X-Sig", "prefix": "sha256="}, "signature_secret": "ship-secret-123456"})),
    )
    .await
}

#[tokio::test]
async fn webhooks_scripts_and_delivery_log() {
    let mut h = integ_harness!();
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
    let mut h = integ_harness!();
    h.signup().await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Orders", "script": SHIP_SCRIPT, "metrics": ["Order Shipped"]}))).await;
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

    // Turned off: a webhook gets a log line (marked ignored), and nothing of its payload is kept.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"script": fixed}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/status"), Some(json!({"status": "disabled"}))).await;
    let body = json!({"id": "o-10", "email": "eve@example.com", "total": 1}).to_string();
    let (s, _, _) = h.raw("POST", &path, &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'eve@example.com'").await, 0);
    let ign = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=ignored"), None).await["data"][0].clone();
    assert_eq!(ign["has_payload"], false, "{ign}");
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE body LIKE '%eve@example.com%' OR headers::text LIKE '%eve%'").await, 0);
    let (s, _) = h.req("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"delivery_ids": [ign["id"]]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "turn it on first");
    h.ok("POST", &format!("/api/integrations/{id}/status"), Some(json!({"status": "enabled"}))).await;
    let r = h.ok("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"delivery_ids": [ign["id"]]}))).await;
    assert_eq!(r["replayed"], 0, "there's no payload to replay");
    h.cleanup().await;
}

#[tokio::test]
async fn consent_relays_and_suppression() {
    let mut h = integ_harness!();
    h.signup().await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": "function handle(i) { return i.payload.actions }", "metrics": ["Stay", "Reservation Confirmed", "Reservation Cancelled", "Order Placed"]}))).await;
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
    let mut h = integ_harness!();
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
    let mut h = integ_harness!();
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
    /// (authorization header, path, the lastUpdatedAt filter or the guests filter)
    seen: Mutex<Vec<(String, String, String)>>,
}

/// The mock's change times: an hour before the test run started, one second apart, so they stay
/// inside the pull's 24-hour look-back whatever day the tests run (a fixed date ran out of it).
fn mock_updated_at(i: usize) -> String {
    static BASE: std::sync::OnceLock<chrono::DateTime<chrono::Utc>> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(|| {
        use chrono::Timelike;
        (chrono::Utc::now() - chrono::Duration::hours(1)).with_nanosecond(0).unwrap()
    });
    (base + chrono::Duration::seconds(i as i64)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A stand-in for Guesty's Open API: OAuth tokens, `GET /v1/reservations-v3/search` (changed
/// reservations, oldest change first, `filter[lastUpdatedAt][gte]`, skip/limit and
/// `pagination.hasMore`) and `GET /v1/guests-crud`. As the live API does (checked read-only by the
/// lead): guests are found only by a filter on `_id` (a filter on `id` matches nothing), and come
/// back as `_id`, `email` and `phone` (digits with the country code, no "+"). Reservations carry
/// door codes in `notes` and contact details in `channelMetadata.guestContact`.
async fn mock_guesty(total: usize) -> (Arc<MockGuesty>, String) {
    use axum::extract::{Query, State};
    use axum::routing::{get, post};
    use std::collections::HashMap;
    let m: Arc<MockGuesty> = Default::default();
    let authed = |m: &MockGuesty, headers: &axum::http::HeaderMap| -> Result<String, (StatusCode, axum::Json<Value>)> {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        if *m.reject.lock().unwrap() || !auth.starts_with("Bearer tok-") {
            // Like some real APIs, the error echoes what was sent.
            return Err((StatusCode::UNAUTHORIZED, axum::Json(json!({"error": format!("invalid token {auth}")}))));
        }
        Ok(auth)
    };
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
            "/v1/reservations-v3/search",
            get(move |State(m): State<Arc<MockGuesty>>, headers: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                let gte = q.get("filter[lastUpdatedAt][gte]").cloned().unwrap_or_default();
                let auth = match authed(&m, &headers) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                m.seen.lock().unwrap().push((auth, "search".into(), gte.clone()));
                assert_eq!(q.get("sort").map(String::as_str), Some("lastUpdatedAt"));
                let skip: usize = q.get("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
                let limit: usize = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(25).min(100);
                let matching: Vec<usize> = (0..total).filter(|i| mock_updated_at(*i) >= gte).collect();
                let page: Vec<Value> = matching
                    .iter()
                    .skip(skip)
                    .take(limit)
                    .map(|i| {
                        json!({"reservationId": format!("res{i}"), "status": "confirmed", "source": "manual", "channel": "manual", "confirmationCode": format!("C{i}"),
                               "guestId": format!("g{i}"), "checkIn": "2027-01-10T23:00:00.000Z", "checkOut": "2027-01-12T18:00:00.000Z",
                               "checkInDateLocalized": "2027-01-10", "checkOutDateLocalized": "2027-01-12", "createdAt": "2026-11-17T09:00:00.000Z",
                               "lastUpdatedAt": mock_updated_at(*i), "notes": {"keyCode": "1234", "guest": "Door code 1234, lockbox 9876"},
                               "channelMetadata": {"confirmedAt": "2026-11-17T09:00:00.000Z", "guestContact": {"phone": "+15035550000", "accessCode": "9876"}},
                               "stay": [{"listingId": "L1", "checkInDateLocalized": "2027-01-10", "checkOutDateLocalized": "2027-01-12", "guestsCount": 2}]})
                    })
                    .collect();
                let more = skip + page.len() < matching.len();
                (StatusCode::OK, axum::Json(json!({"results": page, "pagination": {"skip": skip, "limit": limit, "hasMore": more}})))
            }),
        )
        .route(
            "/v1/guests-crud",
            get(move |State(m): State<Arc<MockGuesty>>, headers: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                let filters = q.get("filters").cloned().unwrap_or_default();
                let auth = match authed(&m, &headers) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                m.seen.lock().unwrap().push((auth, "guests".into(), filters.clone()));
                assert!(q.get("columns").is_some_and(|c| c.contains("guestEmail")), "{q:?}");
                assert!(!q.contains_key("sort") && !q.contains_key("filter[lastUpdatedAt][gte]"), "the search's own query isn't sent here: {q:?}");
                let f: Value = serde_json::from_str(&filters).unwrap_or(json!({}));
                let ids: Vec<String> = f["_id"]["@in"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                let results: Vec<Value> = ids
                    .iter()
                    .map(|g| {
                        let i: usize = g.trim_start_matches('g').parse().unwrap();
                        let email = if i == 0 { "relay-1@guest.airbnb.com".to_string() } else { format!("guest{i}@example.com") };
                        json!({"_id": g, "fullName": format!("Guest {i}"), "email": email, "phone": format!("1503555{:04}", 1000 + i)})
                    })
                    .collect();
                (StatusCode::OK, axum::Json(json!({"results": results, "total": results.len()})))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, format!("http://{addr}"))
}

async fn guesty_with_mock(h: &mut Harness, base: &str) -> String {
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    let integ = &created["integration"];
    let id = integ["id"].as_str().unwrap().to_string();
    let mut pull = integ["pull"].clone();
    pull["url"] = json!(format!("{base}/v1/reservations-v3/search"));
    pull["auth"]["token_url"] = json!(format!("{base}/oauth2/token"));
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": pull, "credentials": {"client_id": "cid-0001", "client_secret": "cs-secret-value"}}))).await;
    id
}

#[tokio::test]
async fn guesty_pull_reuses_its_token_and_backfills() {
    let (mock, base) = mock_guesty(150).await;
    let mut h = integ_harness!();
    h.signup().await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    assert_eq!(created["flows"].as_array().unwrap().len(), 3);
    assert_eq!(created["flows"][0]["status"], "draft", "suggested flows never start on their own");
    let integ = &created["integration"];
    let id = integ["id"].as_str().unwrap().to_string();
    assert_eq!(integ["missing_credentials"], json!(["client_id", "client_secret"]));
    assert_eq!(integ["message_id"], json!({"header": "svix-id"}));
    assert_eq!(integ["contacts_opted_in"], false);
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
    pull["url"] = json!(format!("{base}/v1/reservations-v3/search"));
    pull["auth"]["token_url"] = json!(format!("{base}/oauth2/token"));
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": pull, "credentials": {"client_id": "cid-0001", "client_secret": "cs-secret-value"}}))).await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert!(!got.to_string().contains("cs-secret-value"), "credentials are write-only");
    assert_eq!(got["pull"]["auth"]["client_secret"], "{{secret.client_secret}}", "auth fields are references");
    assert_eq!(got["missing_credentials"], json!([]));

    // A test pull fetches the first page and writes nothing (except caching the token).
    let t = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(t["ok"], true, "{t}");
    assert_eq!(t["next_request"]["url"], "/v1/guests-crud", "{t}");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 0);
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    assert!(!t["pull"]["request"].to_string().contains("tok-1"), "the token isn't shown");
    // Another test reuses the cached token.
    h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);

    // A real pull pages through all 150 (reservations, then their guests, page by page) with one token.
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 150);
    let metric = |m: &str| format!("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = '{m}'");
    assert_eq!(count(&h, &metric("Reservation Booked")).await, 150);
    assert_eq!(count(&h, &metric("Reservation Confirmed")).await, 150, "a reservation seen twice (the cursor's own row) counts once");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email_suppressed_reason = 'booking_relay'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE marketing_hold").await, 150, "guests didn't opt in: all held from marketing");
    assert_eq!(count(&h, "SELECT count(*) FROM metrics WHERE server_only").await, 6, "the recipe's metrics can't be forged from a browser, from the moment it's set up");
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["state"]["cursor"], mock_updated_at(149), "{}", got["state"]);
    assert!(got["state"].get("pending").is_none(), "{}", got["state"]);
    let pulls = h.ok("GET", &format!("/api/integrations/{id}/deliveries?source=pull"), None).await;
    assert_eq!(pulls["total"], 7, "three reservation pages, their three guest pages and an empty last page (the cursor's own row isn't read again); test pulls aren't logged: {pulls}");
    assert_eq!(got["state"]["skip"], 1, "the next run skips the row at the cursor it already read: {}", got["state"]);

    // A backfill runs from its own start date, in chained jobs, and reports progress.
    mock.seen.lock().unwrap().clear();
    let b = h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"backfill_since": "2025-01-01"}))).await;
    assert_eq!(b["backfill"]["status"], "running");
    h.drain().await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert_eq!(got["backfill"]["status"], "done", "{}", got["backfill"]);
    assert_eq!(got["backfill"]["pages"], 7, "no page is read twice");
    assert_eq!(mock.seen.lock().unwrap()[0].2, "2025-01-01T00:00:00.000Z", "{:?}", mock.seen.lock().unwrap()[0]);
    assert_eq!(got["state"]["cursor"], mock_updated_at(149), "a backfill leaves the live cursor alone");
    assert_eq!(count(&h, "SELECT count(*) FROM events").await, 300, "nothing counted twice");
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
    let t = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(t["ok"], false, "{t}");
    assert!(t["error"].as_str().unwrap().contains("daily cap"), "{t}");
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    // Once the cap allows it, a new token is fetched.
    sqlx::query("UPDATE integrations SET oauth_token_requests = '{}'").execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 2);
    // Re-saving the same credentials keeps the token.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"credentials": {"client_secret": "cs-secret-value"}}))).await;
    assert!(!h.ok("GET", &format!("/api/integrations/{id}"), None).await["oauth_token_cached_until"].is_null());
    // When the app rejects the token, it's dropped, and the error is logged without the app's own
    // text (which here echoes the token back).
    *mock.reject.lock().unwrap() = true;
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert!(got["oauth_token_cached_until"].is_null());
    let err = got["last_error"].as_str().unwrap();
    assert!(err.contains("HTTP 401") && err.contains("unauthorized"), "{err}");
    assert!(!err.contains("tok-") && err.contains("lastUpdatedAt][gte]=…"), "the origin and path, query values hidden, none of the app's text: {err}");
    let failed = h.ok("GET", &format!("/api/integrations/{id}/deliveries?status=error"), None).await;
    let did = failed["data"][0]["id"].as_str().unwrap();
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries/{did}"), None).await;
    assert!(!d.to_string().contains("tok-2") && !d.to_string().contains("cs-secret-value"), "no secrets in the log: {d}");
    assert!(d["body"].is_null(), "the app's error body isn't kept");

    // Pulls only reach public addresses over https.
    for url in ["https://127.0.0.1:9/x", "https://169.254.169.254/latest/meta-data", "https://10.0.0.5/api", "https://localhost/x"] {
        h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": {"url": url, "interval_minutes": 15}}))).await;
        let t = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
        assert_eq!(t["ok"], false, "{url}: {t}");
        assert!(t["error"].as_str().unwrap().contains("public internet address"), "{url}: {t}");
    }
    for url in ["http://example.com/x", "https://user:pw@example.com/x", "https://{{state.host}}/x"] {
        let (s, v) = h.req("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": {"url": url}}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {v}");
    }
    let (s, _) = h.req("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": {"url": "https://x.test", "interval_minutes": 2}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "five minutes is the shortest interval");
    // Literal credentials in the auth block are refused.
    let (s, v) = h.req("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": {"url": "https://x.test", "auth": {"type": "bearer", "token": "sk_live_123"}}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("secret reference"), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn token_cache_is_bound_to_its_settings_and_origin() {
    let (mock, base) = mock_guesty(3).await;
    let (other, other_base) = mock_guesty(3).await;
    let mut h = integ_harness!();
    h.signup().await;
    let id = guesty_with_mock(&mut h, &base).await;
    h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 1);
    // Move the pull to another origin but keep the same auth block: the cached token is never sent there.
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    let mut pull = got["pull"].clone();
    pull["url"] = json!(format!("{other_base}/v1/reservations-v3/search"));
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": pull}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    let sent: Vec<String> = other.seen.lock().unwrap().iter().map(|s| s.0.clone()).collect();
    assert!(!sent.iter().any(|a| a == "Bearer tok-1"), "the old origin's token never reaches the new one: {sent:?}");
    assert_eq!(mock.tokens.load(Ordering::SeqCst), 2, "a new token was asked for, from the configured token URL");

    // A run that started before a settings change can't write its token into the cache.
    let (v0,): (i64,) = sqlx::query_as("SELECT auth_version FROM integrations WHERE id = $1::uuid").bind(&id).fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE integrations SET oauth_token_enc = NULL, oauth_token_expires_at = NULL, oauth_token_key = NULL WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    let i = dumbmail::features::integrations::model::load_any(&h.state, id.parse().unwrap()).await.unwrap().unwrap();
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"settings": {"skip_owner_stays": false}}))).await;
    let cfg = i.pull_config().unwrap();
    let creds = i.credentials(&h.state).unwrap();
    let empty = serde_json::Map::new();
    let now = chrono::Utc::now();
    let vars = dumbmail::features::integrations::pull::Vars { secrets: &creds, state: &empty, settings: &empty, now, since: now, redact: false };
    let origin = cfg.origin();
    let who = dumbmail::features::integrations::pull::TokenFor { integration_id: i.id, auth_version: v0, origin: &origin };
    let r = dumbmail::features::integrations::pull::oauth_token(&h.state, &who, &cfg.auth, &vars).await;
    assert!(r.is_err(), "a stale run stops instead of fetching");
    let cached: Option<String> = sqlx::query_scalar("SELECT oauth_token_enc FROM integrations WHERE id = $1::uuid").bind(&id).fetch_one(&h.state.db).await.unwrap();
    assert!(cached.is_none());
    h.cleanup().await;
}

#[tokio::test]
async fn scheduled_pulls_and_retention() {
    let (mock, base) = mock_guesty(3).await;
    let mut h = integ_harness!();
    h.signup().await;
    let script = "function handle(i) { return i.payload.results.map(function (r) { return {type: 'upsert_profile', email: r.guestId + '@example.com'} }) }";
    let integ = h
        .ok("POST", "/api/integrations", Some(json!({"name": "Poll", "script": script, "credentials": {"key": "tok-k"},
            "pull": {"url": format!("{base}/v1/reservations-v3/search"), "query": {"limit": 10, "sort": "lastUpdatedAt"}, "auth": {"type": "bearer", "token": "{{secret.key}}"}, "interval_minutes": 5}})))
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
    let mut h = integ_harness!();
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
    let mut h = integ_harness!();
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
    let mut h = integ_harness!();
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
    let mut h = integ_harness!();
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
    assert_eq!(keys, vec!["aftership", "bigcommerce", "calendly", "guesty", "hostaway", "paddle", "recharge", "segment", "square", "stripe", "typeform", "webflow", "woocommerce", "zendesk"]);
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

// ---------- The process sandbox, against the real `dumbmail sandbox-run` ----------

fn gone(pid: Option<u32>) -> bool {
    // kill(pid, 0) fails with ESRCH only once the process is dead *and* reaped (a zombie still answers).
    let pid = pid.expect("the child had a pid") as i32;
    let rc = unsafe { libc::kill(pid, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

async fn sandboxed(script: &str) -> dumbmail::features::integrations::sandbox::Report {
    use dumbmail::features::integrations::sandbox;
    sandbox::run_program(std::path::Path::new(env!("CARGO_BIN_EXE_dumbmail")), &[sandbox::SUBCOMMAND], script, &json!({"payload": {"n": 1}}), sandbox::TIME_LIMIT).await
}

#[tokio::test]
async fn hostile_scripts_are_cut_off_and_leave_nothing_behind() {
    use dumbmail::features::integrations::sandbox;
    use std::time::Duration;
    let hard_stop = sandbox::TIME_LIMIT + Duration::from_millis(1500);
    // Plain data comes back, from a fresh process each time.
    let r = sandboxed("function handle(i) { return [{type: 'log', message: 'n=' + i.payload.n}] }").await;
    assert_eq!(r.result.unwrap(), json!([{"type": "log", "message": "n=1"}]));
    assert!(gone(r.pid));
    let r = sandboxed("function handle(i) { return [typeof process, typeof require, typeof fetch, typeof std, typeof os, typeof console] }").await;
    assert_eq!(r.result.unwrap(), json!(["undefined", "undefined", "undefined", "undefined", "undefined", "undefined"]), "no host access");

    let cases: Vec<(&str, String)> = vec![
        ("infinite loop", "function handle(i) { while (true) {} }".into()),
        ("caught interrupt", "function handle(i) { for (;;) { try { while (true) {} } catch (e) {} } }".into()),
        ("loop at load time", "for (;;) {} function handle(i) { return [] }".into()),
        ("memory bomb", "function handle(i) { var a = []; while (true) { a.push('x'.repeat(1 << 20) + a.length) } }".into()),
        ("doubling string", "function handle(i) { var s = 'x'; while (true) { s = s + s } }".into()),
        ("huge repeat", "function handle(i) { return ['x'.repeat(1 << 30)] }".into()),
        // Native loops that don't poll the engine's interrupt: the parent's kill stops them.
        ("native sort of a huge array", "function handle(i) { var a = []; for (var k = 0; k < 1500000; k++) a.push((k * 7919) % 1000003); a.sort(); a.sort(); a.sort(); a.sort(); return [a.length] }".into()),
        ("giant JSON.stringify", "function handle(i) { var o = []; for (var k = 0; k < 400000; k++) o.push({k: k, v: 'yyyyyyyyyyyyyyyyyyyy'}); var s = ''; for (var r = 0; r < 50; r++) s = JSON.stringify(o); return [s.length] }".into()),
        ("result over the output cap", "function handle(i) { var a = []; for (var k = 0; k < 3000; k++) a.push({type: 'log', message: 'y'.repeat(500)}); return a }".into()),
        ("deep recursion", "function f(n) { return f(n + 1) + 1 } function handle(i) { return f(0) }".into()),
        ("regex backtracking", "function handle(i) { return [/^(a+)+$/.test('a'.repeat(40) + 'b')] }".into()),
        ("toJSON trap", "function handle(i) { return {toJSON: function () { while (true) {} }} }".into()),
    ];
    for (name, script) in cases {
        let r = sandboxed(&script).await;
        let err = r.result.as_ref().expect_err(name);
        assert!(r.elapsed < hard_stop, "{name} ran {:?}: {err}", r.elapsed);
        assert!(gone(r.pid), "{name}: the child process is gone and reaped");
        assert!(["longer than", "memory", "limit", "stack", "recursed", "too much", "too long", "string length", "stopped"].iter().any(|w| err.contains(w)), "{name}: {err}");
    }

    // The parent caps what it reads *while reading*: a child that prints forever is cut off.
    let yes = sandbox::run_program(std::path::Path::new("/usr/bin/yes"), &[], "function handle() {}", &json!({}), sandbox::TIME_LIMIT).await;
    assert!(yes.result.as_ref().unwrap_err().contains("more than"), "{:?}", yes.result);
    assert!(yes.elapsed < hard_stop);
    assert!(gone(yes.pid));
    // A child that hangs without a word is killed at the deadline.
    let hang = sandbox::run_program(std::path::Path::new("/usr/bin/sleep"), &["60"], "function handle() {}", &json!({}), Duration::from_millis(300)).await;
    assert!(hang.result.as_ref().unwrap_err().contains("longer than"), "{:?}", hang.result);
    assert!(hang.elapsed < Duration::from_secs(3));
    assert!(gone(hang.pid));

    // Polluting prototypes in one run never reaches the next (each run is a new process).
    sandboxed("function handle(i) { Object.prototype.polluted = 1; Array.prototype.push = null; return [] }").await.result.unwrap();
    let r = sandboxed("function handle(i) { var a = []; a.push(1); return [typeof ({}).polluted, a.length] }").await;
    assert_eq!(r.result.unwrap(), json!(["undefined", 1]));
}

#[tokio::test]
async fn the_script_process_inherits_no_descriptors() {
    use dumbmail::features::integrations::sandbox;
    // A descriptor the server holds without close-on-exec (as a socket or file another thread
    // opened could be): the child must not have it.
    let path = std::ffi::CString::new("/proc/self/status").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    assert!(fd > 2);
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_eq!(flags & libc::FD_CLOEXEC, 0, "the test descriptor is inheritable");
    let probe = format!("cat >/dev/null; n=$(ls /proc/$$/fd | wc -l); if [ -e /proc/$$/fd/{fd} ]; then o=yes; else o=no; fi; printf '{{\"ok\": [\"%s\", %s]}}' $o $n");
    let r = sandbox::run_program(std::path::Path::new("/bin/sh"), &["-c", &probe], "function handle() {}", &json!({}), sandbox::TIME_LIMIT).await;
    unsafe { libc::close(fd) };
    let v = r.result.unwrap();
    assert_eq!(v[0], "no", "the server's descriptor {fd} reached the script's process");
    assert!(v[1].as_u64().unwrap() <= 4, "only stdin, stdout, stderr (and ls's own) are open: {v}");
}

#[tokio::test]
async fn scripts_are_admitted_fairly() {
    use dumbmail::features::integrations::sandbox::Slots;
    let a = uuid::Uuid::now_v7();
    let b = uuid::Uuid::now_v7();
    let s1 = Slots::try_take(a).unwrap();
    let s2 = Slots::try_take(a).unwrap();
    assert!(Slots::try_take(a).is_none(), "two at a time per account");
    let other = Slots::try_take(b);
    assert!(other.is_some(), "another account isn't blocked");
    drop((s1, other));
    assert!(Slots::try_take(a).is_some());
    drop(s2);

    // A delivery whose account is at its cap goes back in the queue instead of waiting on a worker.
    let mut h = integ_harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Busy", "script": "function handle(i) { return [{type: 'upsert_profile', email: 'busy@example.com'}] }"}))).await;
    let hold = (Slots::try_take(acct).unwrap(), Slots::try_take(acct).unwrap());
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE status = 'pending'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'integrations_delivery' AND run_at > now()").await, 1, "put back with a short delay");
    drop(hold);
    sqlx::query("UPDATE jobs SET run_at = now()").execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'busy@example.com'").await, 1);
    h.cleanup().await;
}

// ---------- The marketing hold ----------

#[tokio::test]
async fn marketing_hold_keeps_people_out_of_marketing_until_they_opt_in() {
    let mut h = integ_harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.credit(5.0).await;
    let script = "function handle(i) { return i.payload.actions }";
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": script, "metrics": ["Stay Booked"]}))).await;
    let path = path_of(&integ);
    let trusted = h.ok("POST", "/api/integrations", Some(json!({"name": "Newsletter app", "script": script, "contacts_opted_in": true, "metrics": ["Stay Booked"]}))).await;
    assert_eq!(trusted["contacts_opted_in"], true);
    // Someone who was already here before the integration saw them.
    h.ok("POST", "/api/profiles", Some(json!({"email": "pre@example.com"}))).await;
    let send = |path: String, actions: Value| {
        let body = json!({"actions": actions}).to_string();
        (path, body)
    };
    for (p, body) in [
        send(path.clone(), json!([
            {"type": "upsert_profile", "email": "held@example.com", "first_name": "Hal"},
            {"type": "upsert_profile", "email": "pre@example.com", "first_name": "Pre"},
            {"type": "upsert_profile", "email": "gone@example.com"},
            {"type": "unsubscribe", "channel": "email", "profile": {"email": "gone@example.com"}},
            {"type": "upsert_profile", "email": "supp@example.com"},
            {"type": "suppress", "email": "supp@example.com"},
            {"type": "upsert_profile", "email": "optin@example.com", "consent": [{"channel": "email", "subscribed": true, "source": "Booking form checkbox"}]},
            {"type": "upsert_profile", "email": "later@example.com"}
        ])),
        send(path_of(&trusted), json!([{"type": "upsert_profile", "email": "ok@example.com"}])),
    ] {
        let (s, _, r) = h.raw("POST", &p, &[("content-type", "application/json")], &body).await;
        assert_eq!(s, StatusCode::OK, "{r}");
    }
    h.drain().await;
    let hold_db = h.state.db.clone();
    let held = move |email: &'static str| {
        let db = hold_db.clone();
        async move { sqlx::query_scalar::<_, bool>("SELECT marketing_hold FROM profiles WHERE email = $1").bind(email).fetch_one(&db).await.unwrap() }
    };
    assert!(held("held@example.com").await, "made by an integration with no opt-in");
    assert!(!held("pre@example.com").await, "never set on someone who already existed");
    assert!(!held("ok@example.com").await, "the owner said this integration's contacts opted in");
    assert!(!held("optin@example.com").await, "an explicit opt-in");
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'held@example.com'").fetch_one(&h.state.db).await.unwrap();
    let detail = h.ok("GET", &format!("/api/profiles/{pid}"), None).await;
    assert_eq!(detail["marketing_hold"], true);
    assert_eq!(detail["source"]["name"], "Bookings");

    // Campaigns leave held people out.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Everyone"}))).await;
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM profiles").fetch_all(&h.state.db).await.unwrap();
    dumbmail::audience::add_to_list(&h.state.db, account_id(&h).await, list["id"].as_str().unwrap().parse().unwrap(), &ids).await.unwrap();
    let audience = json!({"include": [{"type": "list", "id": list["id"]}]});
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": audience}))).await;
    assert_eq!(est["count"], 3, "pre, ok and optin: {est}");
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": audience, "content": {"subject": "Sale", "blocks": [{"type": "text", "html": "<p>20% off</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let mut got: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject == "Sale").map(|m| m.to).collect();
    got.sort();
    assert_eq!(got, vec!["ok@example.com", "optin@example.com", "pre@example.com"]);

    // Segments: can_receive is false for them, and the hold can be selected.
    let can = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"match": "all", "conditions": [{"type": "can_receive", "channel": "email", "value": true}]}}))).await;
    assert_eq!(can["count"], 3, "{can}");
    let on_hold = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"match": "all", "conditions": [{"type": "property", "field": "marketing_hold", "op": "eq", "value": true}]}}))).await;
    assert_eq!(on_hold["count"], 4, "held, later, and the unsubscribed and suppressed ones it added: {on_hold}");

    // Flows: an ordinary flow skips held people; a flow marked as service messages reaches them;
    // nobody unsubscribed or suppressed gets either.
    let flow = |name: &str, service: bool| {
        json!({"name": name, "trigger": {"type": "metric", "metric": "Stay Booked"},
            "definition": {"entry": "e1", "settings": {"reentry": "always", "service_messages": service}, "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": name, "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}]}})
    };
    for (name, service) in [("Promo", false), ("Your booking", true)] {
        let f = h.ok("POST", "/api/flows", Some(flow(name, service))).await;
        h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    }
    let events: Vec<Value> = ["held@example.com", "gone@example.com", "supp@example.com", "pre@example.com"]
        .iter()
        .map(|e| json!({"type": "track_event", "metric": "Stay Booked", "profile": {"email": e}}))
        .collect();
    let (p, body) = send(path.clone(), Value::Array(events));
    h.raw("POST", &p, &[("content-type", "application/json")], &body).await;
    h.fast_forward().await;
    let sent_to = |subject: &str| {
        let mut v: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject == subject).map(|m| m.to).collect();
        v.sort();
        v
    };
    assert_eq!(sent_to("Promo"), vec!["pre@example.com"], "the held person is skipped by an ordinary flow");
    assert_eq!(sent_to("Your booking"), vec!["held@example.com", "pre@example.com"], "service messages reach them; never the unsubscribed or suppressed");

    // A real opt-in clears the hold: a sign-up form, and an integration's explicit consent block.
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Footer", "kind": "embed", "config": {"collect": ["email"]}}))).await;
    let form_id = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{form_id}"), Some(json!({"status": "live"}))).await;
    let (s, _, r) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": form_id, "email": "held@example.com"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert!(!held("held@example.com").await, "signed up through a form");
    let (p, body) = send(path.clone(), json!([{"type": "upsert_profile", "email": "later@example.com", "consent": [{"channel": "email", "subscribed": true, "source": "Checkout box"}]}]));
    h.raw("POST", &p, &[("content-type", "application/json")], &body).await;
    h.drain().await;
    assert!(!held("later@example.com").await, "an explicit opt-in from the integration");
    // An unsubscribed held person stays out even if a later upsert has no consent.
    assert!(held("gone@example.com").await);
    h.cleanup().await;
}

#[tokio::test]
async fn a_new_address_on_a_matched_profile_has_not_consented() {
    let mut h = integ_harness!();
    h.signup().await;
    let mk = |ext: &str, email: &str, consent: &str| json!({"external_id": ext, "email": email, "email_consent": consent});
    h.ok("POST", "/api/profiles", Some(mk("X1", "a1@example.com", "subscribed"))).await;
    h.ok("POST", "/api/profiles", Some(mk("X2", "a2@example.com", "unsubscribed"))).await;
    h.ok("POST", "/api/profiles", Some(mk("X3", "a3@example.com", "subscribed"))).await;
    h.ok("POST", "/api/profiles", Some(mk("X4", "a4@example.com", "subscribed"))).await;
    h.ok("POST", "/api/profiles", Some(mk("X5", "a5@example.com", "subscribed"))).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE external_id = 'X3'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'complaint' WHERE external_id = 'X4'").execute(&h.state.db).await.unwrap();
    for (ext, email) in [("X1", "b1@example.com"), ("X2", "b2@example.com"), ("X3", "b3@example.com"), ("X4", "b4@example.com"), ("X5", "a5@example.com")] {
        h.ok("POST", "/api/profiles", Some(json!({"external_id": ext, "email": email}))).await;
    }
    let row_db = h.state.db.clone();
    let row = move |ext: &'static str| {
        let db = row_db.clone();
        async move { sqlx::query_as::<_, (String, String, Option<String>)>("SELECT email, email_consent, email_suppressed_reason FROM profiles WHERE external_id = $1").bind(ext).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(row("X1").await, ("b1@example.com".into(), "never_subscribed".into(), None), "the new address never agreed");
    assert_eq!(row("X2").await, ("b2@example.com".into(), "unsubscribed".into(), None), "unsubscribed stays");
    assert_eq!(row("X3").await, ("b3@example.com".into(), "never_subscribed".into(), None), "the old address's hard bounce is cleared");
    assert_eq!(row("X4").await.2.as_deref(), Some("complaint"), "a complaint stays");
    assert_eq!(row("X5").await.1, "subscribed", "the same address keeps its consent");
    // The same through an integration, matched by phone.
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550111", "email": "c1@example.com", "email_consent": "subscribed"}))).await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Phones", "script": "function handle(i) { return [{type: 'upsert_profile', phone_number: '+15035550111', email: 'c2@example.com'}] }"}))).await;
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.drain().await;
    let (email, consent): (String, String) = sqlx::query_as("SELECT email, email_consent FROM profiles WHERE phone_number = '+15035550111'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((email.as_str(), consent.as_str()), ("c2@example.com", "never_subscribed"));
    // A signed-in member's explicit edit is unchanged: it keeps the consent.
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE external_id = 'X5'").fetch_one(&h.state.db).await.unwrap();
    h.ok("PATCH", &format!("/api/profiles/{id}"), Some(json!({"email": "a5-new@example.com"}))).await;
    assert_eq!(row("X5").await.1, "subscribed");
    h.cleanup().await;
}

#[tokio::test]
async fn browsers_cant_forge_an_integrations_events() {
    let mut h = integ_harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": "function handle(i) { return [{type: 'track_event', metric: 'Reservation Confirmed', profile: {email: 'g@example.com'}}] }", "metrics": ["Reservation Confirmed"]}))).await;
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.drain().await;
    let ev = |metric: &str| json!({"metric": metric, "profile": {"email": "victim@example.com"}, "properties": {"check_in": "2026-12-01"}}).to_string();
    let (s, _, r) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &ev("Reservation Confirmed")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert!(r.contains("server"), "{r}");
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &ev("Viewed Product")).await;
    assert_eq!(s, StatusCode::OK, "other metrics still work from the browser");
    // A recipe's metrics are refused from the moment it's set up, before any real event arrives.
    h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "stripe", "create_flows": false}))).await;
    let (s, _, r) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &ev("Placed Order")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    // The private API may still record it.
    h.ok("POST", "/api/events", Some(json!({"metric": "Reservation Confirmed", "profile": {"email": "staff@example.com"}}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'victim@example.com'").await, 1, "only the allowed event made the profile");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = 'Reservation Confirmed' AND p.email = 'victim@example.com'").await, 0);
    h.cleanup().await;
}

// ---------- Idempotent deliveries ----------

#[tokio::test]
async fn retries_and_replays_never_repeat_what_was_written() {
    let mut h = integ_harness!();
    h.signup().await;
    // Action 0 records an event with no unique_id; action 1 fails the first time (another profile
    // already has that external id), then succeeds once that's fixed.
    let script = "function handle(i) { return [
        {type: 'track_event', metric: 'Ping', profile: {email: 'p@example.com'}},
        {type: 'upsert_profile', email: 'p@example.com', external_id: 'EXT-1'}
    ] }";
    h.ok("POST", "/api/profiles", Some(json!({"email": "other@example.com", "external_id": "EXT-1"}))).await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Pings", "script": script, "message_id": {"header": "x-msg-id"}, "metrics": ["Ping"]}))).await;
    let id = integ["id"].as_str().unwrap().to_string();
    let path = path_of(&integ);
    let (s, _, _) = h.raw("POST", &path, &[("x-msg-id", "m-1")], "{}").await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let pings = "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Ping'";
    assert_eq!(count(&h, pings).await, 1);
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await["data"][0].clone();
    assert_eq!(d["status"], "error", "{d}");
    // Fix the conflict and let the retry run: the event isn't recorded again.
    sqlx::query("UPDATE profiles SET external_id = NULL WHERE email = 'other@example.com'").execute(&h.state.db).await.unwrap();
    let jobs: Vec<(String, Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as("SELECT kind, payload, run_at FROM jobs").fetch_all(&h.state.db).await.unwrap();
    sqlx::query("UPDATE jobs SET run_at = now()").execute(&h.state.db).await.unwrap();
    h.drain().await;
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await["data"][0].clone();
    assert_eq!(d["status"], "ok", "{d} {jobs:?}");
    assert_eq!(d["counts"]["already_written"], 1, "{d}");
    assert_eq!(count(&h, pings).await, 1, "the retry skipped the event it had already written");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE external_id = 'EXT-1' AND email = 'p@example.com'").await, 1);
    // Replaying it (even as a new delivery) can't record the same event twice.
    let did = d["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"delivery_ids": [did]}))).await;
    h.drain().await;
    assert_eq!(count(&h, pings).await, 1);
    // The app's own resend of the same message is acknowledged and ignored.
    let (s, _, r) = h.raw("POST", &path, &[("x-msg-id", "m-1")], "{}").await;
    assert_eq!(s, StatusCode::OK);
    assert!(r.contains("\"duplicate\":true"), "{r}");
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE source = 'webhook'").await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn bad_signatures_dont_use_up_the_allowance_and_admission_is_bounded() {
    let mut h = integ_harness!();
    h.signup().await;
    let integ = ship_integration(&mut h).await;
    let path = path_of(&integ);
    for _ in 0..(dumbmail::features::integrations::api::RATE_PER_MINUTE + 50) {
        let (s, _, _) = h.raw("POST", &path, &[("x-sig", "sha256=00ff")], "{}").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    assert!(count(&h, "SELECT count(*) FROM integration_deliveries").await <= 30, "rejections are logged only while they're few");
    let body = json!({"id": "o-9", "email": "real@example.com", "first": "R", "plan": "x", "total": 1}).to_string();
    let sig = format!("sha256={}", hmac_hex("ship-secret-123456", &body));
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", &sig)], &body).await;
    assert_eq!(s, StatusCode::OK, "a genuine delivery still gets in after a flood of bad ones");
    // Work still to run is bounded per integration, counting retries and replays too.
    let iid: uuid::Uuid = integ["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("INSERT INTO integration_deliveries (id, integration_id, account_id, source, status, next_attempt_at) SELECT gen_random_uuid(), $1, account_id, 'webhook', 'error', now() + interval '1 hour' FROM integrations, generate_series(1, $2) WHERE id = $1")
        .bind(iid)
        .bind(dumbmail::features::integrations::engine::MAX_OPEN_PER_INTEGRATION as i32)
        .execute(&h.state.db)
        .await
        .unwrap();
    let (s, _, _) = h.raw("POST", &path, &[("x-sig", &sig)], &body).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    h.cleanup().await;
}

// ---------- Guesty versions, and event-date scheduling ----------

fn svix(key: &[u8], id: &str, body: &str) -> Vec<(String, String)> {
    let ts = chrono::Utc::now().timestamp().to_string();
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).unwrap();
    m.update(format!("{id}.{ts}.{body}").as_bytes());
    let sig = format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes()));
    vec![("svix-id".into(), id.into()), ("svix-timestamp".into(), ts), ("svix-signature".into(), sig), ("content-type".into(), "application/json".into())]
}

#[tokio::test]
async fn guesty_reminders_follow_the_newest_version_and_stop_on_cancellation() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    let gid = created["integration"]["id"].as_str().unwrap().to_string();
    let key = b"guesty-svix-key-0123456789abcdef";
    let secret = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(key));
    h.ok("PATCH", &format!("/api/integrations/{gid}"), Some(json!({"signature_secret": secret}))).await;
    // The pre-arrival flow, set to fire at midnight so the test doesn't depend on the hour.
    let pre = created["flows"].as_array().unwrap().iter().find(|f| f["name"].as_str().unwrap().contains("pre-arrival")).unwrap()["id"].as_str().unwrap().to_string();
    let f = h.ok("GET", &format!("/api/flows/{pre}"), None).await;
    let mut trigger = f["trigger"].clone();
    trigger["hour"] = json!(0);
    h.ok("PATCH", &format!("/api/flows/{pre}"), Some(json!({"trigger": trigger}))).await;
    assert_eq!(f["definition"]["settings"]["service_messages"], true, "booking messages reach guests who haven't agreed to marketing");
    h.ok("POST", &format!("/api/flows/{pre}/status"), Some(json!({"status": "live"}))).await;
    // Guests Guesty already gave us (v2 webhooks only carry their Guesty id); the integration wrote their ids.
    h.ok("POST", "/api/profiles", Some(json!({"email": "ana@example.com"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "ben@example.com"}))).await;
    sqlx::query("UPDATE profiles SET integration_keys = jsonb_build_object('guesty_guest_id', CASE email WHEN 'ana@example.com' THEN 'gA' ELSE 'gB' END)").execute(&h.state.db).await.unwrap();
    // Someone claiming Ana's Guesty id through an ordinary property (as a browser could) gets nothing.
    h.ok("POST", "/api/profiles", Some(json!({"email": "mallory@example.com", "properties": {"guesty_guest_id": "gA"}}))).await;

    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    let day = |d: i64| (today + chrono::Duration::days(d)).format("%Y-%m-%d").to_string();
    let v2 = |ev: &str, res: &str, guest: &str, status: &str, check_in: &str, published: &str, msg: &str| {
        let body = json!({"event": ev, "meta": {"eventId": msg, "publishedAt": published},
            "data": {"reservationId": res, "guestId": guest, "status": status, "checkInDateLocalized": check_in, "checkOutDateLocalized": check_in, "confirmationCode": "C", "guestsCount": 2, "event": ev}})
        .to_string();
        (msg.to_string(), body)
    };
    // Ana: A (dates in 2 days) → B (in 5 days) → A again; delivered out of order (newest first).
    let a3 = v2("reservation.updated.v2", "R1", "gA", "confirmed", &day(2), "2026-11-17T12:00:00.000Z", "msg-a3");
    let a1 = v2("reservation.created.v2", "R1", "gA", "confirmed", &day(2), "2026-11-17T10:00:00.000Z", "msg-a1");
    let b2 = v2("reservation.updated.v2", "R1", "gA", "confirmed", &day(5), "2026-11-17T11:00:00.000Z", "msg-b2");
    // Ben: booked for 2 days from now, then cancelled.
    let c1 = v2("reservation.created.v2", "R2", "gB", "confirmed", &day(2), "2026-11-17T10:00:00.000Z", "msg-c1");
    let c2 = v2("reservation.updated.v2", "R2", "gB", "canceled", &day(2), "2026-11-17T10:30:00.000Z", "msg-c2");
    let path = path_of(&created["integration"]);
    for (msg, body) in [a3, a1, b2, c1, c2.clone(), c2] {
        let hdrs = svix(key, &msg, &body);
        let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (s, _, r) = h.raw("POST", &path, &hdrs, &body).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        h.drain().await;
    }
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE source = 'webhook'").await, 5, "the resent cancellation was ignored by its svix-id");
    let metric = |m: &str| format!("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = '{m}'");
    let evs: Vec<(String, Option<String>)> = sqlx::query_as("SELECT m.name, e.unique_id FROM events e JOIN metrics m ON m.id = e.metric_id ORDER BY 1, 2").fetch_all(&h.state.db).await.unwrap();
    let dl: Vec<(String, Option<String>, Option<Value>)> = sqlx::query_as("SELECT status, error, result FROM integration_deliveries").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(count(&h, &metric("Reservation Confirmed")).await, 4, "three distinct versions for Ana, one for Ben: {evs:?} {dl:?}");
    assert_eq!(count(&h, &metric("Reservation Booked")).await, 2, "booked once per reservation");
    // The pre-arrival flow's payment gate is on: v2 webhooks carry no money, so nothing goes yet.
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    let sent = |h: &Harness| h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("See you soon")).map(|m| m.to).collect::<Vec<_>>();
    assert!(sent(&h).is_empty(), "payment unknown: no reminder");
    // Payment evidence newer than everything above (a legacy webhook with the money), for both.
    for (res, guest, email, status, msg) in [("R1", "gA", "ana@example.com", "confirmed", "msg-p1"), ("R2", "gB", "ben@example.com", "canceled", "msg-p2")] {
        let body = json!({"event": "reservation.updated", "reservation": {"_id": res, "status": status, "guestId": guest, "lastUpdatedAt": "2026-11-17T13:00:00.000Z",
            "checkInDateLocalized": day(2), "checkOutDateLocalized": day(2), "guest": {"_id": guest, "email": email}, "money": {"totalPaid": 500, "balanceDue": 0, "currency": "USD"}}})
        .to_string();
        let hdrs = svix(key, msg, &body);
        let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (s, _, r) = h.raw("POST", &path, &hdrs, &body).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        h.drain().await;
    }
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h), vec!["ana@example.com"], "one reminder, for the newest version (A, in two days); none for the cancelled booking");
    let report = h.ok("GET", &format!("/api/flows/{pre}/report"), None).await;
    assert_eq!(report["object_dates"]["counts"]["started"], 1, "{report}");
    assert_eq!(report["object_dates"]["counts"]["filtered"], 1, "the cancelled booking: {report}");
    assert!(report["object_dates"]["counts"]["pending"].is_null(), "no fire left for B's date (the older versions were ignored): {report}");
    h.cleanup().await;
}

#[tokio::test]
async fn event_date_fires_catch_up_and_report_missed_ones() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    let day = |d: i64| (today + chrono::Duration::days(d)).format("%Y-%m-%d").to_string();
    // Events recorded before the flow exists are scheduled once it goes live.
    track(&mut h, "Booked", "early@example.com", json!({"id": "E", "on": day(0)}), "E", "2026-11-01T10:00:00Z").await;
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "Day-of", "trigger": {"type": "event_date", "metric": "Booked", "field": "on", "offset_days": 0, "hour": 0},
            "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": "Today: {{ event.properties.id }}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}]}})))
        .await;
    let fid = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    // One recorded now, and one whose fire was due 30 hours ago (the scheduler was down).
    track(&mut h, "Booked", "now@example.com", json!({"id": "N", "on": day(0)}), "N", "2026-11-02T10:00:00Z").await;
    track(&mut h, "Booked", "stale@example.com", json!({"id": "S", "on": day(0)}), "S", "2026-11-02T10:00:00Z").await;
    h.drain().await;
    sqlx::query("UPDATE flow_event_date_fires x SET due_at = now() - interval '30 hours' FROM events e WHERE e.id = x.event_id AND e.unique_id = 'S'").execute(&h.state.db).await.unwrap();
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    let mut sent: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("Today")).map(|m| m.subject).collect();
    sent.sort();
    assert_eq!(sent, vec!["Today: E", "Today: N"], "due within the last day: sent; a day late: not sent");
    let report = h.ok("GET", &format!("/api/flows/{fid}/report"), None).await;
    assert_eq!(report["event_dates"]["counts"]["missed"], 1, "{report}");
    // Each fire records the run it started.
    assert_eq!(count(&h, "SELECT count(*) FROM flow_event_date_fires x JOIN flow_runs r ON r.id = x.run_id WHERE x.outcome = 'started'").await, 2);
    // Running the scheduler again sends nothing more.
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("Today")).count(), 2);
    // An existing metric trigger with many filters still saves (no new cap on old trigger types).
    let filters: Vec<Value> = (0..25).map(|i| json!({"field": format!("f{i}"), "op": "exists"})).collect();
    h.ok("POST", "/api/flows", Some(json!({"name": "Many", "trigger": {"type": "metric", "metric": "M", "filters": filters}}))).await;
    h.cleanup().await;
}

// ---------- Backfills and retention ----------

#[tokio::test]
async fn capped_backfills_resume_and_old_pages_cant_overwrite_progress() {
    let (_mock, base) = mock_guesty(150).await;
    let mut h = integ_harness!();
    h.signup().await;
    let id = guesty_with_mock(&mut h, &base).await;
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    let mut pull = got["pull"].clone();
    pull["backfill_max_pages"] = json!(3);
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"pull": pull}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"backfill_since": "2025-01-01"}))).await;
    h.drain().await;
    let b = h.ok("GET", &format!("/api/integrations/{id}"), None).await["backfill"].clone();
    assert_eq!(b["status"], "capped", "stopping at the page limit isn't 'done': {b}");
    assert_eq!(b["pages"], 3);
    // Where to pick up is saved, encrypted (it can carry what the app put in a Link header), and never shown.
    let saved: Value = sqlx::query_scalar("SELECT backfill FROM integrations WHERE id = $1::uuid").bind(&id).fetch_one(&h.state.db).await.unwrap();
    assert!(saved["next_enc"].as_str().is_some_and(|s| !s.contains("http")), "{saved}");
    assert!(b.get("next_enc").is_none() && b.get("next").is_none(), "{b}");
    let first = count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Reservation Booked'").await;
    assert!(first < 150, "{first}");
    // Resume continues from there, page limit and all, until it's done.
    for _ in 0..5 {
        let b = h.ok("GET", &format!("/api/integrations/{id}"), None).await["backfill"].clone();
        if b["status"] == "done" {
            break;
        }
        h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"resume_backfill": true}))).await;
        h.drain().await;
    }
    let b = h.ok("GET", &format!("/api/integrations/{id}"), None).await["backfill"].clone();
    assert_eq!(b["status"], "done", "{b}");
    // Three pages of reservations, their three guest pages and the empty page that ends it: a
    // resumed run continues exactly where the capped one stopped, with no page read twice.
    assert_eq!(b["pages"], 7);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Reservation Booked'").await, 150);

    // A page from an older run arriving late never overwrites a newer run's progress.
    let old_run = b["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"backfill_since": "2026-01-01"}))).await;
    let before = h.ok("GET", &format!("/api/integrations/{id}"), None).await["backfill"].clone();
    let iid: uuid::Uuid = id.parse().unwrap();
    dumbmail::features::integrations::engine::run_pull(&h.state, &json!({"integration_id": iid, "mode": "backfill", "run_id": old_run, "next": {}, "pages": 0})).await.unwrap();
    let after = h.ok("GET", &format!("/api/integrations/{id}"), None).await["backfill"].clone();
    assert_eq!(before, after);
    h.cleanup().await;
}

#[tokio::test]
async fn retention_has_no_row_cap_and_payloads_stay_in_budget() {
    let mut h = integ_harness!();
    h.signup().await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Lots", "script": "function handle(i) { return [] }"}))).await;
    let iid: uuid::Uuid = integ["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("INSERT INTO integration_deliveries (id, integration_id, account_id, source, status, body, created_at) SELECT gen_random_uuid(), $1, account_id, 'webhook', 'ok', '{}', now() - interval '31 days' FROM integrations, generate_series(1, 12000) WHERE id = $1")
        .bind(iid)
        .execute(&h.state.db)
        .await
        .unwrap();
    // Five recent payloads of (nominally) 100 MB each against a 256 MB budget: the oldest three
    // finished ones lose their payloads (their log lines stay), and a delivery still waiting to
    // run keeps its payload.
    for (i, status) in ["ok", "ok", "pending", "ok", "ok"].iter().enumerate() {
        sqlx::query("INSERT INTO integration_deliveries (id, integration_id, account_id, source, status, body, body_bytes, created_at) SELECT gen_random_uuid(), $1, account_id, 'webhook', $2, '{\"n\": 1}', 100000000, now() - make_interval(hours => $3) FROM integrations WHERE id = $1")
            .bind(iid)
            .bind(*status)
            .bind(10 - i as i32)
            .execute(&h.state.db)
            .await
            .unwrap();
    }
    dumbmail::features::integrations::engine::prune(&h.state).await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries").await, 5, "every delivery past 30 days is gone, not just 5000");
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE body IS NULL").await, 3);
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries WHERE body IS NOT NULL AND status = 'pending'").await, 1);
    assert!(count(&h, "SELECT sum(body_bytes)::bigint FROM integration_deliveries").await <= dumbmail::features::integrations::engine::PAYLOAD_BUDGET_BYTES);
    h.cleanup().await;
}

// ---------- Stripe ----------

async fn stripe_post(h: &mut Harness, path: &str, name: &str) -> (StatusCode, String) {
    let body = fixture("stripe", name).to_string();
    let t = chrono::Utc::now().timestamp();
    let sig = format!("t={t},v1={}", hmac_hex("whsec_stripe_recipe_test", &format!("{t}.{body}")));
    let (s, _, r) = h.raw("POST", path, &[("stripe-signature", &sig), ("content-type", "application/json")], &body).await;
    h.drain().await;
    (s, r)
}

#[tokio::test]
async fn stripe_orders_refunds_and_late_customers() {
    let mut h = integ_harness!();
    h.signup().await;
    let st = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "stripe"}))).await;
    let sid = st["integration"]["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/integrations/{sid}"), Some(json!({"signature_secret": "whsec_stripe_recipe_test"}))).await;
    let path = path_of(&st["integration"]);
    let orders = "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Placed Order'";
    // A subscription for a customer we don't know yet: retried, not dropped.
    stripe_post(&mut h, &path, "subscription-deleted.json").await;
    let d = h.ok("GET", &format!("/api/integrations/{sid}/deliveries"), None).await["data"][0].clone();
    assert_eq!(d["status"], "error", "{d}");
    assert!(d["next_attempt_at"].is_string(), "it will be tried again: {d}");
    // An unpaid checkout records nothing; the delayed payment's success records it, once.
    stripe_post(&mut h, &path, "checkout-unpaid.json").await;
    assert_eq!(count(&h, orders).await, 0);
    stripe_post(&mut h, &path, "checkout-async-succeeded.json").await;
    assert_eq!(count(&h, orders).await, 1);
    let (s, r) = stripe_post(&mut h, &path, "checkout-async-succeeded.json").await;
    assert_eq!(s, StatusCode::OK);
    assert!(r.contains("duplicate"), "Stripe's own resend is recognized by the event id: {r}");
    assert_eq!(count(&h, orders).await, 1);
    // The retry finds the customer the checkout saved.
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'integrations_delivery'").execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Subscription Cancelled'").await, 1);
    // Two partial refunds record their own amounts, once each, however they arrive. A Refund names
    // no customer: it finds the buyer through the PaymentIntent the checkout saved.
    let refunds = "SELECT e.value FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = 'Refunded Order' AND p.email = 'jordan@example.com' ORDER BY e.value";
    for name in ["refund-created-10.json", "refund-created-20.json", "charge-refunded-with-list.json", "charge-partially-refunded.json"] {
        let (s, r) = stripe_post(&mut h, &path, name).await;
        assert_eq!(s, StatusCode::OK, "{name}: {r}");
    }
    let values: Vec<f64> = sqlx::query_scalar(refunds).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(values, vec![10.0, 20.0]);
    // A pending refund isn't counted; refund.updated counts it when it succeeds, once.
    stripe_post(&mut h, &path, "refund-created-pending.json").await;
    assert_eq!(sqlx::query_scalar::<_, f64>(refunds).fetch_all(&h.state.db).await.unwrap().len(), 2, "pending: not yet");
    stripe_post(&mut h, &path, "refund-updated-succeeded.json").await;
    let mut again = fixture("stripe", "refund-updated-succeeded.json");
    again["id"] = json!("evt_re_2P_u_again");
    let body = again.to_string();
    let t = chrono::Utc::now().timestamp();
    let sig = format!("t={t},v1={}", hmac_hex("whsec_stripe_recipe_test", &format!("{t}.{body}")));
    h.raw("POST", &path, &[("content-type", "application/json"), ("stripe-signature", &sig)], &body).await;
    h.drain().await;
    // A refund naming only its charge finds the buyer through the charge id charge.refunded saved.
    stripe_post(&mut h, &path, "refund-updated-charge-only.json").await;
    let values: Vec<f64> = sqlx::query_scalar(refunds).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(values, vec![10.0, 10.0, 10.0, 20.0], "re_1A, re_2P (once, though updated twice), re_3C, re_1B");
    // A checkout that needed no payment isn't an order.
    let before = count(&h, orders).await;
    stripe_post(&mut h, &path, "checkout-no-payment-required.json").await;
    assert_eq!(count(&h, orders).await, before);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'casey@example.net'").await, 1);
    // An incomplete subscription hasn't started; it starts when it becomes active, once.
    let started = "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Subscription Started' AND e.properties->>'subscription_id' = 'sub_incomplete1'";
    stripe_post(&mut h, &path, "subscription-created-incomplete.json").await;
    assert_eq!(count(&h, started).await, 0);
    stripe_post(&mut h, &path, "subscription-updated-activated.json").await;
    stripe_post(&mut h, &path, "subscription-updated-still-active.json").await;
    assert_eq!(count(&h, started).await, 1);
    h.cleanup().await;
}

// ---------- The last review round: one test per fix ----------

/// Invite `email` with `role` and sign in as them; returns the owner's cookie to switch back.
async fn join_as(h: &mut Harness, email: &str, role: &str) -> Option<String> {
    h.ok("POST", "/api/team/invites", Some(json!({"email": email, "role": role}))).await;
    let sent = h.state.email.sent();
    let m = sent.iter().rev().find(|e| e.to == email && e.text.contains("/invite/")).unwrap_or_else(|| panic!("no invite to {email}"));
    let i = m.text.find("/invite/").unwrap() + "/invite/".len();
    let token = m.text[i..].split_whitespace().next().unwrap().to_string();
    let owner = h.cookie.take();
    h.ok("POST", &format!("/api/invites/{token}/accept"), Some(json!({"name": "Member", "password": "member pass 1"}))).await;
    owner
}

async fn with_key(h: &Harness, key: &str, method: &str, path: &str, body: Option<Value>) -> StatusCode {
    use axum::body::Body;
    use tower::ServiceExt;
    let b = axum::http::Request::builder().method(method).uri(path).header("authorization", format!("Bearer {key}"));
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    h.router.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn integration_permissions_follow_roles_keys_and_are_audited() {
    let mut h = integ_harness!();
    h.signup().await;
    let integ = ship_integration(&mut h).await;
    let id = integ["id"].as_str().unwrap().to_string();
    let body = json!({"id": "p1", "email": "pay@example.com", "total": 3, "plan": "x"}).to_string();
    let sig = format!("sha256={}", hmac_hex("ship-secret-123456", &body));
    h.raw("POST", &path_of(&integ), &[("x-sig", &sig), ("content-type", "application/json")], &body).await;
    h.drain().await;
    let did = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await["data"][0]["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"name": "Shipping 2", "credentials": {"api_key": "live-secret-998877"}}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/replay"), Some(json!({"delivery_ids": [did]}))).await;
    let read_key = h.ok("POST", "/api/api-keys", Some(json!({"name": "r", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let acct_read = h.ok("POST", "/api/api-keys", Some(json!({"name": "ar", "scopes": ["account:read"]}))).await["key"].as_str().unwrap().to_string();
    let acct = h.ok("POST", "/api/api-keys", Some(json!({"name": "a", "scopes": ["account"]}))).await["key"].as_str().unwrap().to_string();

    // A manager sees integrations and the log, but not payloads, the inbound URL or script output.
    let owner = join_as(&mut h, "manager@acme.test", "manager").await;
    let list = h.ok("GET", "/api/integrations", None).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let got = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    assert!(got["inbound_url"].is_null() && got.get("state").is_none(), "{got}");
    h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await;
    let d = h.ok("GET", &format!("/api/integrations/{id}/deliveries/{did}"), None).await;
    assert!(d.get("payload").is_none() && d.get("body").is_none() && d.get("result").is_none() && d["payload_hidden"] == true, "{d}");
    assert!(!d.to_string().contains("pay@example.com"), "{d}");
    for (m, p, b) in [
        ("PATCH", format!("/api/integrations/{id}"), json!({"name": "x"})),
        ("POST", format!("/api/integrations/{id}/test"), json!({"payload": {}})),
        ("POST", format!("/api/integrations/{id}/replay"), json!({"all_failed": true})),
        ("POST", format!("/api/integrations/{id}/rotate-url"), json!({})),
        ("POST", "/api/integrations".to_string(), json!({"name": "new"})),
        ("POST", "/api/integrations/from-recipe".to_string(), json!({"recipe": "stripe"})),
        ("DELETE", format!("/api/integrations/{id}"), json!({})),
    ] {
        let (s, _) = h.req(m, &p, Some(b)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "a manager can't {m} {p}");
    }
    // An analyst doesn't see integrations at all.
    h.cookie = owner.clone();
    let _ = join_as(&mut h, "analyst@acme.test", "analyst").await;
    let (s, _) = h.req("GET", "/api/integrations", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    h.cookie = owner;
    // API keys need the account scope: plain `read` isn't enough, and reading isn't managing.
    assert_eq!(with_key(&h, &read_key, "GET", "/api/integrations", None).await, StatusCode::FORBIDDEN);
    assert_eq!(with_key(&h, &acct_read, "GET", "/api/integrations", None).await, StatusCode::OK);
    assert_eq!(with_key(&h, &acct_read, "PATCH", &format!("/api/integrations/{id}"), Some(json!({"name": "y"}))).await, StatusCode::FORBIDDEN);
    assert_eq!(with_key(&h, &acct, "PATCH", &format!("/api/integrations/{id}"), Some(json!({"name": "Shipping 3"}))).await, StatusCode::OK);
    // The owner still sees everything, and the changes are in the audit log, without secrets or payloads.
    let full = h.ok("GET", &format!("/api/integrations/{id}/deliveries/{did}"), None).await;
    assert!(full["payload"].is_object(), "{full}");
    h.ok("DELETE", &format!("/api/integrations/{id}"), None).await;
    let log: Vec<(String, Value)> = sqlx::query_as("SELECT action, details FROM audit_log WHERE action LIKE 'integration.%' ORDER BY created_at").fetch_all(&h.state.db).await.unwrap();
    let actions: Vec<&str> = log.iter().map(|(a, _)| a.as_str()).collect();
    for want in ["integration.create", "integration.update", "integration.secrets_change", "integration.replay", "integration.delete"] {
        assert!(actions.contains(&want), "{want} in {actions:?}");
    }
    let all = format!("{log:?}");
    assert!(!all.contains("live-secret-998877") && !all.contains("ship-secret-123456") && !all.contains("pay@example.com"), "{all}");
    assert!(all.contains("credential api_key"), "names what changed: {all}");
    h.cleanup().await;
}

#[tokio::test]
async fn the_profiles_index_is_built_after_startup_not_in_a_migration() {
    use dumbmail::features::Feature;
    let migration = include_str!("../migrations/0022_marketing_hold.sql");
    let statements: String = migration.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n");
    assert!(!statements.to_uppercase().contains("CREATE INDEX"), "no index is built on profiles inside the migration");
    let h = integ_harness!();
    let exists = "SELECT count(*) FROM pg_class WHERE relname = 'profiles_integration_keys_idx'";
    assert_eq!(count(&h, exists).await, 0, "the migration leaves it to startup");
    dumbmail::features::integrations::Module.after_start(&h.state).await.unwrap();
    let valid: bool = sqlx::query_scalar("SELECT i.indisvalid FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid WHERE c.relname = 'profiles_integration_keys_idx'").fetch_one(&h.state.db).await.unwrap();
    assert!(valid);
    h.cleanup().await;
}

#[tokio::test]
async fn a_new_profile_and_its_hold_commit_together() {
    let mut h = integ_harness!();
    h.signup().await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": "function handle(i) { return [{type: 'upsert_profile', email: 'boom@example.com', properties: {booking_ref: 'B-1'}}] }"}))).await;
    // Something fails while the profile is being made (here: anything that writes the hold).
    sqlx::query(
        "CREATE FUNCTION boom() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.marketing_hold AND NEW.email = 'boom@example.com' THEN RAISE EXCEPTION 'boom'; END IF; RETURN NEW; END $$;",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER boom BEFORE INSERT OR UPDATE ON profiles FOR EACH ROW EXECUTE FUNCTION boom()").execute(&h.state.db).await.unwrap();
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'boom@example.com'").await, 0, "nothing is left half-made (an unheld profile a retry would never hold)");
    sqlx::query("DROP TRIGGER boom ON profiles").execute(&h.state.db).await.unwrap();
    for _ in 0..2 {
        h.fast_forward().await;
    }
    let (held, source, keys): (bool, Option<String>, Value) = sqlx::query_as("SELECT marketing_hold, source, integration_keys FROM profiles WHERE email = 'boom@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(held && source.unwrap().starts_with("integration:") && keys["booking_ref"] == "B-1");
    h.cleanup().await;
}

#[tokio::test]
async fn browsers_cant_redirect_an_integrations_messages_or_borrow_consent() {
    let mut h = integ_harness!();
    let me = h.signup().await;
    h.credit(5.0).await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let script = "function handle(i) { return [{type: 'upsert_profile', email: 'guest@example.com', phone_number: '+15035550123', properties: {guesty_guest_id: 'g1'}}, {type: 'track_event', metric: 'Stay Booked', profile: {email: 'guest@example.com'}}] }";
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": script, "metrics": ["Stay Booked"]}))).await;
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.drain().await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Footer", "kind": "embed", "config": {"collect": ["email", "phone_number"]}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    // A form that matches the guest by phone, with someone else's email: the guest's profile keeps
    // its email; the new address gets its own profile, without the guest's phone.
    let (s, _, r) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": fid, "email": "attacker@evil.test", "phone_number": "+15035550123"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (email, email_src): (Option<String>, Option<String>) = sqlx::query_as("SELECT email, email_source FROM profiles WHERE phone_number = '+15035550123'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(email.as_deref(), Some("guest@example.com"));
    assert!(email_src.unwrap().starts_with("integration:"));
    let (phone, consent): (Option<String>, String) = sqlx::query_as("SELECT phone_number, email_consent FROM profiles WHERE email = 'attacker@evil.test'").fetch_one(&h.state.db).await.unwrap();
    assert!(phone.is_none(), "the guest's number isn't copied across");
    assert_eq!(consent, "subscribed", "the address they typed is theirs to subscribe");
    // The website script can't add an email to a held guest who has none either.
    let integ2 = h.ok("POST", "/api/integrations", Some(json!({"name": "Phones", "script": "function handle(i) { return [{type: 'upsert_profile', phone_number: '+15035550124'}] }"}))).await;
    h.raw("POST", &path_of(&integ2), &[], "{}").await;
    h.drain().await;
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Viewed Page", "profile": {"phone_number": "+15035550124", "email": "sneaky@evil.test"}}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE phone_number = '+15035550124' AND email IS NULL").await, 1);
    // Consent from a form goes only to what was typed: a phone match never subscribes another
    // profile's stored email (here an ordinary profile the API made).
    h.ok("POST", "/api/profiles", Some(json!({"email": "plain@example.com", "phone_number": "+15035550125"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": fid, "email": "typed@example.com", "phone_number": "+15035550125"}).to_string()).await;
    let c: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE phone_number = '+15035550125'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, "never_subscribed", "the stored address wasn't the one typed");
    // A service-message flow reaches the held guest only at the address the integration supplied.
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "Your stay", "trigger": {"type": "metric", "metric": "Stay Booked"},
        "definition": {"entry": "e1", "settings": {"reentry": "always", "service_messages": true}, "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": "Your stay", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.fast_forward().await;
    let sent = |h: &Harness| h.state.email.sent().into_iter().filter(|m| m.subject == "Your stay").map(|m| m.to).collect::<Vec<_>>();
    assert_eq!(sent(&h), vec!["guest@example.com"], "the held guest gets it, at the address the integration gave");
    let gid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'guest@example.com'").fetch_one(&h.state.db).await.unwrap();
    let acct = account_id(&h).await;
    let ev = |n: &str| dumbmail::audience::NewEvent { account_id: acct, profile_id: gid, metric: "Stay Booked", properties: json!({}), value: None, occurred_at: None, unique_id: Some(n.into()), message_id: None };
    // Any other change of the address (here straight in the database, as any other path would) means
    // the integration no longer vouches for it: the held person isn't sent service messages there.
    sqlx::query("UPDATE profiles SET email = 'changed@evil.test' WHERE id = $1").bind(gid).execute(&h.state.db).await.unwrap();
    dumbmail::audience::record_event_from(&h.state.db, ev("s-2"), dumbmail::audience::EventSource::Integration).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h), vec!["guest@example.com"]);
    let why: String = sqlx::query_scalar("SELECT error FROM messages WHERE to_address = 'changed@evil.test' OR (profile_id = $1 AND status = 'skipped') ORDER BY created_at DESC LIMIT 1").bind(gid).fetch_one(&h.state.db).await.unwrap_or_default();
    assert!(why.contains("no marketing consent") || count(&h, "SELECT count(*) FROM messages WHERE to_address = 'changed@evil.test' AND status = 'sent'").await == 0, "{why}");
    h.cleanup().await;
}

#[tokio::test]
async fn integrations_cant_record_dumbmails_alerts_and_held_people_get_their_back_in_stock_alert() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let (s, r) = h.req("POST", "/api/integrations", Some(json!({"name": "X", "metrics": ["Back in Stock"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    // A browser-reserved name that isn't DumbMail's own is fine to declare.
    h.ok("POST", "/api/integrations", Some(json!({"name": "Mail app", "metrics": ["Opened Email"]}))).await;
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Store", "script": "function handle(i) { return [{type: 'track_event', metric: 'Price Dropped', profile: {email: 'a@example.com'}}] }"}))).await;
    let t = h.ok("POST", &format!("/api/integrations/{}/test", integ["id"].as_str().unwrap()), Some(json!({"payload": {}}))).await;
    assert!(t["error"].as_str().unwrap().contains("recorded by DumbMail itself"), "{t}");
    // A held person who confirmed a back-in-stock request gets that alert, and stays held.
    let held = h.ok("POST", "/api/integrations", Some(json!({"name": "Bookings", "script": "function handle(i) { return [{type: 'upsert_profile', email: i.payload.e}] }"}))).await;
    for e in ["bis@example.com", "bis-gone@example.com"] {
        h.raw("POST", &path_of(&held), &[("content-type", "application/json")], &json!({"e": e}).to_string()).await;
    }
    h.drain().await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "bis-gone@example.com", "email_consent": "unsubscribed"}))).await;
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "Back", "trigger": {"type": "metric", "metric": "Back in Stock"},
        "definition": {"entry": "e1", "settings": {"reentry": "always"}, "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": "It's back", "blocks": [{"type": "text", "html": "<p>Back</p>"}]}}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let acct = account_id(&h).await;
    for e in ["bis@example.com", "bis-gone@example.com"] {
        let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = $1").bind(e).fetch_one(&h.state.db).await.unwrap();
        dumbmail::audience::record_event(&h.state.db, dumbmail::audience::NewEvent { account_id: acct, profile_id: pid, metric: "Back in Stock", properties: json!({"name": "Mug"}), value: None, occurred_at: None, unique_id: Some(format!("bis:{e}")), message_id: None }).await.unwrap();
    }
    h.fast_forward().await;
    let to: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject == "It's back").map(|m| m.to).collect();
    assert_eq!(to, vec!["bis@example.com"], "the held person gets the alert they asked for; the unsubscribed one doesn't");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'bis@example.com' AND marketing_hold").await, 1, "the alert doesn't clear the hold");
    h.cleanup().await;
}

#[tokio::test]
async fn a_retry_applies_the_actions_from_the_first_run() {
    let mut h = integ_harness!();
    h.signup().await;
    // The script's output changes every run (the time); the second action can't be written yet.
    let script = "function handle(i) { return [
        {type: 'track_event', metric: 'Tick', profile: {email: 'tick@example.com'}, properties: {at: String(Date.now()) + Math.random()}},
        {type: 'track_event', metric: 'Tock', profile: {property: 'order_ref', value: 'O-1', if_missing: 'retry'}}] }";
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Clock", "script": script, "metrics": ["Tick", "Tock"]}))).await;
    h.raw("POST", &path_of(&integ), &[], "{}").await;
    h.drain().await;
    let (first, applied): (Value, Value) = sqlx::query_as("SELECT actions, applied FROM integration_deliveries").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(first.as_array().unwrap().len(), 2, "the first run's list is kept: {first}");
    assert!(first[0]["unique_id"].as_str().unwrap().starts_with("delivery:") && first[0]["time"].is_string(), "missing ids and times are fixed: {first}");
    assert!(applied.get("0").is_some(), "the first action is in the ledger: {applied}");
    // Now the person the second action needs exists; the retry applies the same list.
    h.ok("POST", "/api/profiles", Some(json!({"email": "order@example.com"}))).await;
    sqlx::query("UPDATE profiles SET integration_keys = '{\"order_ref\": \"O-1\"}' WHERE email = 'order@example.com'").execute(&h.state.db).await.unwrap();
    for _ in 0..2 {
        h.fast_forward().await;
    }
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Tick'").await, 1, "never a second Tick");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Tock'").await, 1);
    let (after, status): (Value, String) = sqlx::query_as("SELECT actions, status FROM integration_deliveries").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(after, first, "the list never changes");
    assert_eq!(status, "ok");
    let at: String = sqlx::query_scalar("SELECT e.properties->>'at' FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Tick'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(at, first[0]["properties"]["at"].as_str().unwrap());
    h.cleanup().await;
}

#[tokio::test]
async fn a_trusted_event_replaces_a_browser_copy_never_the_reverse() {
    use dumbmail::audience::{self, EventSource, NewEvent};
    let mut h = integ_harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "buyer@example.com"}))).await;
    let pid: uuid::Uuid = p["id"].as_str().unwrap().parse().unwrap();
    let ev = |props: Value| NewEvent { account_id: acct, profile_id: pid, metric: "Ordered Thing", properties: props, value: Some(10.0), occurred_at: None, unique_id: Some("order-1".into()), message_id: None };
    let forged = audience::record_browser_event(&h.state.db, ev(json!({"by": "browser"}))).await.unwrap();
    assert!(forged.is_some());
    let jobs = "SELECT count(*) FROM jobs WHERE kind = 'flow_event'";
    let jobs_before = count(&h, jobs).await;
    let trusted = audience::record_event_from(&h.state.db, ev(json!({"by": "server"})), EventSource::Integration).await.unwrap();
    assert!(trusted.is_some() && trusted != forged, "the trusted event is recorded as new");
    let rows: Vec<(bool, Value)> = sqlx::query_as("SELECT from_browser, properties FROM events WHERE unique_id = 'order-1'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(rows, vec![(false, json!({"by": "server"}))], "the browser copy is gone");
    let _ = jobs_before;
    // A browser event never replaces a trusted one, and another trusted copy is a duplicate.
    assert!(audience::record_browser_event(&h.state.db, ev(json!({"by": "browser again"}))).await.unwrap().is_none());
    assert!(audience::record_api_event(&h.state.db, ev(json!({"by": "api"}))).await.unwrap().is_none());
    let rows: Vec<(bool, Value)> = sqlx::query_as("SELECT from_browser, properties FROM events WHERE unique_id = 'order-1'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(rows, vec![(false, json!({"by": "server"}))]);
    h.cleanup().await;
}

#[tokio::test]
async fn declared_metrics_are_server_only_from_the_moment_they_are_saved() {
    let mut h = integ_harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Lessons", "metrics": ["Lesson Booked"], "script": "function handle(i) { return [{type: 'track_event', metric: i.payload.m, profile: {email: 'l@example.com'}}] }"}))).await;
    let browser = |m: &str| json!({"metric": m, "profile": {"email": "victim@example.com"}}).to_string();
    // No event has ever arrived, and a browser still can't send it.
    let (s, _, r) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &browser("Lesson Booked")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    // The script may record only what it declared.
    h.raw("POST", &path_of(&integ), &[("content-type", "application/json")], &json!({"m": "Lesson Cancelled"}).to_string()).await;
    h.drain().await;
    let d = h.ok("GET", &format!("/api/integrations/{}/deliveries", integ["id"].as_str().unwrap()), None).await["data"][0].clone();
    assert_eq!(d["status"], "error");
    assert!(d["error"].as_str().unwrap().contains("isn't one of this integration's metrics"), "{d}");
    h.ok("PATCH", &format!("/api/integrations/{}", integ["id"].as_str().unwrap()), Some(json!({"metrics": ["Lesson Booked", "Lesson Cancelled"]}))).await;
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &browser("Lesson Cancelled")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn a_replaced_backfill_writes_nothing_from_a_page_it_already_fetched() {
    use axum::routing::get;
    let mut h = integ_harness!();
    h.signup().await;
    let integ_id: Arc<Mutex<Option<uuid::Uuid>>> = Default::default();
    let db = h.state.db.clone();
    let iid2 = integ_id.clone();
    // The app answers slowly; while it does, someone starts a new backfill (replacing this one).
    let app = axum::Router::new().route(
        "/items",
        get(move || {
            let db = db.clone();
            let id = *iid2.lock().unwrap();
            async move {
                if let Some(id) = id {
                    sqlx::query("UPDATE integrations SET backfill = backfill || '{\"id\": \"replacement\"}' WHERE id = $1").bind(id).execute(&db).await.unwrap();
                }
                axum::Json(json!({"items": [{"email": "late@example.com"}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let script = "function handle(i) { return i.payload.items.map(function (x) { return {type: 'upsert_profile', email: x.email} }) }";
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Items", "script": script, "pull": {"url": format!("http://{addr}/items")}}))).await;
    let id: uuid::Uuid = integ["id"].as_str().unwrap().parse().unwrap();
    *integ_id.lock().unwrap() = Some(id);
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({"backfill_since": "2026-01-01"}))).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'late@example.com'").await, 0, "the replaced run's page wrote nothing");
    let st: String = sqlx::query_scalar("SELECT status FROM integration_deliveries WHERE source = 'pull'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "ignored");
    h.cleanup().await;
}

#[test]
fn guesty_pages_continue_past_equal_timestamps_across_runs() {
    use dumbmail::features::integrations::{recipes, sandbox};
    let r = recipes::get("guesty").unwrap();
    let t = "2026-11-17T09:00:00.000Z";
    let res = |n: usize, at: &str| (0..n).map(|i| json!({"reservationId": format!("r{i}"), "guestId": format!("g{i}"), "status": "confirmed", "lastUpdatedAt": at})).collect::<Vec<_>>();
    let run = |input: Value| -> Value { serde_json::from_str(&sandbox::run_engine(&r.script, &input.to_string(), std::time::Duration::from_secs(5)).unwrap()).unwrap() };
    let page = |results: Vec<Value>, gte: &str, skip: &str, state: Value| {
        json!({"source": "pull", "page": 1, "payload": {"results": results, "pagination": {"hasMore": true}}, "settings": {}, "state": state, "now": "2026-11-17T18:00:00.000Z", "backfill": null,
               "request": {"url": "https://open-api.guesty.com/v1/reservations-v3/search", "query": [["filter[lastUpdatedAt][gte]", gte], ["skip", skip]]}})
    };
    // A first page: 48 rows at 08:00 and 2 at 09:00. The next one starts at 09:00 skipping those 2.
    let mut first = res(48, "2026-11-17T08:00:00.000Z");
    first.extend(res(2, t));
    let out = run(page(first, "2026-11-16T00:00:00.000Z", "0", json!({})));
    assert_eq!(out[0]["values"]["page_last"], t);
    assert_eq!(out[0]["values"]["page_skip"], 2);
    // A full page all at 09:00, read with skip 2: the next skip is 52, never back to the same rows.
    let out = run(page(res(50, t), t, "2", json!({"cursor": t, "skip": 2})));
    assert_eq!(out[0]["values"]["page_skip"], 52);
    // The guests step saves cursor and skip, so a run that stops at its page cap resumes there.
    let guests = json!({"source": "pull", "page": 2, "payload": {"results": []}, "settings": {}, "now": "2026-11-17T18:00:00.000Z", "backfill": null,
        "state": {"pending": [], "page_last": t, "page_skip": 52, "page_more": true}, "request": {"url": "https://open-api.guesty.com/v1/guests-crud", "query": []}});
    let out = run(guests);
    let saved = out.as_array().unwrap().iter().find(|a| a["type"] == "set_state").unwrap();
    assert_eq!(saved["values"]["cursor"], t);
    assert_eq!(saved["values"]["skip"], 52);
    let next = out.as_array().unwrap().iter().find(|a| a["type"] == "next_request").unwrap();
    assert_eq!(next["query"]["skip"], 52);
    // And the scheduled pull's own request carries the saved skip.
    assert_eq!(r.manifest["pull"]["query"]["skip"], "{{state.skip|'0'}}");
}

#[tokio::test]
async fn event_date_fires_found_late_are_missed_and_time_zone_changes_move_them() {
    let mut h = integ_harness!();
    h.signup().await;
    let flow = |name: &str| json!({"name": name, "trigger": {"type": "event_date", "metric": "Stay", "field": "check_in", "offset_days": -2, "hour": 10},
        "definition": {"entry": "e1", "settings": {}, "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": name, "blocks": [{"type": "text", "html": "<p>x</p>"}]}}]}});
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    let day = |d: i64| (today + chrono::Duration::days(d)).format("%Y-%m-%d").to_string();
    // History: a stay whose reminder was due days ago, and three upcoming stays.
    track(&mut h, "Stay", "old@example.com", json!({"check_in": day(-3)}), "s-old", "2026-01-01T00:00:00Z").await;
    for (i, e) in ["a@example.com", "b@example.com", "c@example.com"].iter().enumerate() {
        track(&mut h, "Stay", e, json!({"check_in": day(10 + i as i64)}), &format!("s-{i}"), "2026-01-01T00:00:00Z").await;
    }
    let f = h.ok("POST", "/api/flows", Some(flow("Soon"))).await;
    let fid = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    // One seeding budget per tick, across flows: two events this tick, the rest next tick.
    dumbmail::flows::event_date_tick(&h.state, 2).await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM flow_event_date_fires").await, 2);
    dumbmail::flows::event_date_tick(&h.state, 2).await.unwrap();
    dumbmail::flows::event_date_tick(&h.state, 2).await.unwrap();
    let report = h.ok("GET", &format!("/api/flows/{fid}/report"), None).await;
    assert_eq!(report["event_dates"]["counts"]["missed"], 1, "found more than a day late: shown, not dropped: {report}");
    assert_eq!(report["event_dates"]["counts"]["pending"], 3, "{report}");
    // The account moves to New York: 10:00 is now 10:00 there.
    let due = "SELECT min(due_at) FROM flow_event_date_fires WHERE outcome = 'pending'";
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(due).fetch_one(&h.state.db).await.unwrap();
    h.ok("PATCH", "/api/account", Some(json!({"timezone": "America/New_York"}))).await;
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(due).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(before - after, chrono::Duration::hours(3), "{before} → {after}");
    h.cleanup().await;
}

/// A stand-in app that echoes back whatever credentials it is sent (in its body, its Link header
/// and its error text), as some real APIs do.
async fn echoing_app() -> String {
    use axum::http::HeaderMap;
    use axum::routing::get;
    let app = axum::Router::new()
        .route(
            "/v1/items",
            get(|headers: HeaderMap, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let pin = headers.get("x-pin").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let token = auth.split(' ').nth(1).unwrap_or("").to_string();
                if q.contains_key("page") {
                    return (StatusCode::UNAUTHORIZED, [("link", format!("<https://x.test/?t={token}>"))], axum::Json(json!({"error": format!("bad auth {auth}")})));
                }
                (
                    StatusCode::OK,
                    [("link", format!("</v1/items?page=2&access_token={token}>; rel=\"next\""))],
                    axum::Json(json!({"echo": format!("you sent {auth} and pin {pin}"), "items": [{"email": "e@example.com"}]})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dumbmail::features::integrations::pull::allow_test_address(addr);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn nothing_a_vendor_echoes_back_leaks_a_credential() {
    let base = echoing_app().await;
    let mut h = integ_harness!();
    h.signup().await;
    // Page 1 follows the Link header (which carries the token) to page 2, which fails.
    let script = "function handle(i) {
        var link = (i.response && i.response.headers.link) || '';
        var m = /<([^>]+)>/.exec(link);
        var out = [{type: 'log', message: 'page ' + i.page + ': ' + String(i.payload.echo || '') + ' link ' + link}, {type: 'set_state', values: {seen: i.page}}];
        if (m && i.page === 1) out.push({type: 'next_request', url: m[1]});
        return out;
    }";
    for (name, auth, creds) in [
        ("Bearer", json!({"type": "bearer", "token": "{{secret.token}}"}), json!({"token": "tk_live_77aa88bb", "pin": "42"})),
        ("Basic", json!({"type": "basic", "username": "{{secret.user}}", "password": "{{secret.pass}}"}), json!({"user": "acme", "pass": "pw-9900", "pin": "42"})),
    ] {
        let integ = h
            .ok("POST", "/api/integrations", Some(json!({"name": name, "script": script, "credentials": creds,
                "pull": {"url": format!("{base}/v1/items"), "headers": {"X-Pin": "{{secret.pin}}"}, "auth": auth}})))
            .await;
        let id = integ["id"].as_str().unwrap().to_string();
        let test = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
        h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
        h.drain().await;
        let basic = base64::engine::general_purpose::STANDARD.encode("acme:pw-9900");
        let stored: Vec<String> = sqlx::query_scalar(
            "SELECT concat_ws(' ', body, headers::text, request::text, error, result::text, actions::text) FROM integration_deliveries WHERE integration_id = $1::uuid
             UNION ALL SELECT concat_ws(' ', last_error, state::text, backfill::text) FROM integrations WHERE id = $1::uuid
             UNION ALL SELECT payload::text FROM jobs",
        )
        .bind(&id)
        .fetch_all(&h.state.db)
        .await
        .unwrap();
        let deliveries = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await;
        let mut shown = vec![test.to_string(), deliveries.to_string()];
        for d in deliveries["data"].as_array().unwrap() {
            shown.push(h.ok("GET", &format!("/api/integrations/{id}/deliveries/{}", d["id"].as_str().unwrap()), None).await.to_string());
        }
        let everything = format!("{stored:?} {shown:?}");
        for secret in ["tk_live_77aa88bb", "pw-9900", basic.as_str(), "pin 42"] {
            assert!(!everything.contains(secret), "{name}: {secret:?} leaked: {everything}");
        }
        assert!(everything.contains("pin •••"), "{name}: the short secret is masked as a whole word: {everything}");
        assert_eq!(count(&h, &format!("SELECT count(*) FROM integration_deliveries WHERE integration_id = '{id}' AND source = 'pull'")).await, 2, "{name}: both pages ran");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn stored_payloads_stay_in_budget_from_admission() {
    let mut h = integ_harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    dumbmail::features::integrations::engine::set_payload_budget_for(acct, 3000);
    let integ = h.ok("POST", "/api/integrations", Some(json!({"name": "Big", "script": "function handle(i) { return [] }"}))).await;
    let body = json!({"pad": "x".repeat(900)}).to_string();
    // Three waiting payloads fill the budget; a fourth is refused (the app retries later).
    for _ in 0..3 {
        let (s, _, _) = h.raw("POST", &path_of(&integ), &[("content-type", "application/json")], &body).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _, r) = h.raw("POST", &path_of(&integ), &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{r}");
    // Once they've run, older payloads make room for new ones.
    h.drain().await;
    for _ in 0..4 {
        let (s, _, _) = h.raw("POST", &path_of(&integ), &[("content-type", "application/json")], &body).await;
        assert_eq!(s, StatusCode::OK);
        h.drain().await;
    }
    let used: i64 = sqlx::query_scalar("SELECT bytes FROM integration_payload_bytes WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let real: i64 = count(&h, "SELECT COALESCE(sum(octet_length(body)), 0)::bigint FROM integration_deliveries").await;
    assert!(used <= 3000 && used == real, "{used} {real}");
    assert_eq!(count(&h, "SELECT count(*) FROM integration_deliveries").await, 7, "every log line stays");
    h.cleanup().await;
}

#[tokio::test]
async fn guesty_door_codes_never_reach_stored_or_shown_data() {
    let (_mock, base) = mock_guesty(3).await;
    let mut h = integ_harness!();
    h.signup().await;
    let id = guesty_with_mock(&mut h, &base).await;
    let key = b"guesty-svix-key-0123456789abcdef";
    let secret = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(key));
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"signature_secret": secret}))).await;
    let integ = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    // A webhook whose reservation (and before-state) carries a door code.
    let body = fixture("guesty", "webhook-updated-with-door-code.json").to_string();
    let hdrs = svix(key, "msg-door", &body);
    let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (s, _, r) = h.raw("POST", &path_of(&integ), &hdrs, &body).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    h.drain().await;
    // A pull page (codes in notes and channelMetadata.guestContact), and a test pull.
    let test = h.ok("POST", &format!("/api/integrations/{id}/test"), Some(json!({"pull": true}))).await;
    h.ok("POST", &format!("/api/integrations/{id}/pull"), Some(json!({}))).await;
    h.drain().await;
    assert!(count(&h, "SELECT count(*) FROM profiles WHERE email LIKE 'guest%@example.com'").await >= 2, "the pull found the guests by _id");
    let phone: Option<String> = sqlx::query_scalar("SELECT phone_number FROM profiles WHERE email = 'guest1@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(phone.as_deref(), Some("+15035551001"), "Guesty's digits-only phone gets its +");
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT concat_ws(' ', body, headers::text, request::text, error, result::text, actions::text) FROM integration_deliveries
         UNION ALL SELECT concat_ws(' ', state::text, backfill::text) FROM integrations
         UNION ALL SELECT properties::text FROM events
         UNION ALL SELECT concat_ws(' ', properties::text, integration_keys::text) FROM profiles",
    )
    .fetch_all(&h.state.db)
    .await
    .unwrap();
    let deliveries = h.ok("GET", &format!("/api/integrations/{id}/deliveries"), None).await;
    let mut shown = vec![test.to_string()];
    for d in deliveries["data"].as_array().unwrap() {
        shown.push(h.ok("GET", &format!("/api/integrations/{id}/deliveries/{}", d["id"].as_str().unwrap()), None).await.to_string());
    }
    let everything = format!("{stored:?} {shown:?}");
    assert!(everything.contains("reservations-v3") && everything.contains("66f1c4a1246e7d9f0f8da88a"), "the deliveries are there");
    // Times and ids DumbMail writes (`…T15:11:27.015512Z`, uuids) can contain the codes' digits by
    // chance; take them out so only data that came from Guesty is searched.
    let noise = regex::Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}(:?\d{2})?)?|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let everything = noise.replace_all(&everything, "_").into_owned();
    for code in ["5512", "1234", "9876", "keyCode", "Lockbox", "guestContact"] {
        assert!(!everything.contains(code), "{code:?} survived: {everything}");
    }
    h.cleanup().await;
}

async fn send_signed(h: &Harness, path: &str, (hdrs, body): (Vec<(String, String)>, String)) {
    let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (s, _, r) = h.raw("POST", path, &hdrs, &body).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    h.drain().await;
}

#[tokio::test]
async fn guesty_orders_versions_by_its_own_time_and_waits_for_unknown_guests() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    let gid = created["integration"]["id"].as_str().unwrap().to_string();
    let key = b"guesty-svix-key-0123456789abcdef";
    let secret = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(key));
    h.ok("PATCH", &format!("/api/integrations/{gid}"), Some(json!({"signature_secret": secret}))).await;
    let pre = created["flows"].as_array().unwrap().iter().find(|f| f["name"].as_str().unwrap().contains("pre-arrival")).unwrap()["id"].as_str().unwrap().to_string();
    let f = h.ok("GET", &format!("/api/flows/{pre}"), None).await;
    let mut trigger = f["trigger"].clone();
    trigger["hour"] = json!(0);
    h.ok("PATCH", &format!("/api/flows/{pre}"), Some(json!({"trigger": trigger}))).await;
    h.ok("POST", &format!("/api/flows/{pre}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "ana@example.com"}))).await;
    sqlx::query("UPDATE profiles SET integration_keys = '{\"guesty_guest_id\": \"gA\"}' WHERE email = 'ana@example.com'").execute(&h.state.db).await.unwrap();
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    let in2 = (today + chrono::Duration::days(2)).format("%Y-%m-%d").to_string();
    let path = path_of(&created["integration"]);
    let post = |ev: &str, res: &str, guest: &str, status: &str, published: Option<&str>, changed: Option<Vec<&str>>, msg: &str| {
        let mut meta = json!({"eventId": msg});
        if let Some(p) = published {
            meta["publishedAt"] = json!(p);
        }
        if let Some(c) = changed {
            meta["changedFields"] = json!(c);
        }
        let body = json!({"event": ev, "meta": meta, "data": {"reservationId": res, "guestId": guest, "status": status, "checkInDateLocalized": in2, "checkOutDateLocalized": in2, "guestsCount": 2, "event": ev}}).to_string();
        (svix(key, msg, &body), body)
    };
    let metric = |m: &str| format!("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = '{m}'");
    // No Guesty time: it can't be put in order, so nothing is recorded (the pull will).
    send_signed(&h, &path, post("reservation.created.v2", "R0", "gA", "confirmed", None, None, "m0")).await;
    assert_eq!(count(&h, &metric("Reservation Confirmed")).await, 0);
    // Booked at 10:00, cancelled at 11:00, reinstated at 12:00; the cancellation arrives last.
    send_signed(&h, &path, post("reservation.created.v2", "R1", "gA", "confirmed", Some("2026-11-17T10:00:00.000Z"), None, "m1")).await;
    send_signed(&h, &path, post("reservation.updated.v2", "R1", "gA", "confirmed", Some("2026-11-17T12:00:00.000Z"), Some(vec!["status"]), "m3")).await;
    send_signed(&h, &path, post("reservation.updated.v2", "R1", "gA", "canceled", Some("2026-11-17T11:00:00.000Z"), Some(vec!["status"]), "m2")).await;
    // Only the notes changed: no new version.
    send_signed(&h, &path, post("reservation.updated.v2", "R1", "gA", "confirmed", Some("2026-11-17T13:00:00.000Z"), Some(vec!["notes"]), "m4")).await;
    assert_eq!(count(&h, &metric("Reservation Confirmed")).await, 2, "10:00 and the reinstatement; nothing for the notes");
    let sent = |h: &Harness| h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("See you soon")).count();
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h), 0, "the payment gate is on and v2 webhooks carry no money");
    // Payment evidence newer than the reinstatement (a legacy webhook with the money).
    let paid = json!({"event": "reservation.updated", "reservation": {"_id": "R1", "status": "confirmed", "guestId": "gA", "lastUpdatedAt": "2026-11-17T13:30:00.000Z",
        "checkInDateLocalized": in2, "checkOutDateLocalized": in2, "guest": {"_id": "gA", "email": "ana@example.com"}, "money": {"totalPaid": 500, "balanceDue": 0, "currency": "USD"}}})
    .to_string();
    send_signed(&h, &path, (svix(key, "m-paid", &paid), paid)).await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h), 1, "the late, older cancellation didn't undo the reinstatement");
    // A later version that didn't move the date (as a pull sees it) doesn't send the reminder again.
    send_signed(&h, &path, post("reservation.updated.v2", "R1", "gA", "confirmed", Some("2026-11-17T14:00:00.000Z"), Some(vec!["checkIn"]), "m5")).await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h), 1);
    let report = h.ok("GET", &format!("/api/flows/{pre}/report"), None).await;
    assert_eq!(report["object_dates"]["counts"]["started"], 1, "one start for the one date: {report}");
    // A guest DumbMail doesn't know yet: the webhook waits (retried) instead of being dropped.
    send_signed(&h, &path, post("reservation.created.v2", "R9", "gNew", "confirmed", Some("2026-11-17T15:00:00.000Z"), None, "m9")).await;
    let d = &h.ok("GET", &format!("/api/integrations/{gid}/deliveries"), None).await["data"][0];
    assert_eq!(d["status"], "error", "{d}");
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@example.com"}))).await;
    sqlx::query("UPDATE profiles SET integration_keys = '{\"guesty_guest_id\": \"gNew\"}' WHERE email = 'new@example.com'").execute(&h.state.db).await.unwrap();
    for _ in 0..2 {
        h.fast_forward().await;
    }
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Reservation Booked' AND e.properties->>'reservation_id' = 'R9'").await, 1);
    // A booking first seen without its total, then with it: booked once, the value once.
    let legacy = |total: Option<f64>, msg: &str| {
        let mut r = json!({"_id": "R7", "status": "confirmed", "guestId": "gL", "lastUpdatedAt": "2026-11-17T16:00:00.000Z", "confirmedAt": "2026-11-17T15:59:00.000Z",
            "checkInDateLocalized": in2, "checkOutDateLocalized": in2, "guest": {"_id": "gL", "email": "lee@example.com", "firstName": "Lee"}});
        if let Some(t) = total {
            r["money"] = json!({"totalPaid": t, "balanceDue": 0, "currency": "USD"});
        }
        (json!({"event": "reservation.new", "reservation": r}).to_string(), msg.to_string())
    };
    for (body, msg) in [legacy(None, "l1"), legacy(Some(480.0), "l2")] {
        let hdrs = svix(key, &msg, &body);
        let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        h.raw("POST", &path, &hdrs, &body).await;
        h.drain().await;
    }
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Reservation Booked' AND e.properties->>'reservation_id' = 'R7'").await, 1);
    let totals: Vec<f64> = sqlx::query_scalar("SELECT e.value FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Reservation Total' AND e.properties->>'reservation_id' = 'R7'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(totals, vec![480.0]);
    h.cleanup().await;
}

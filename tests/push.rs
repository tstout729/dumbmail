//! Push notifications end to end, against local stand-ins for a browser push service (payloads are
//! decrypted with the browser's key), APNs over HTTP/2 and FCM v1 with its token endpoint.
//!
//! The races are staged exactly: a test hook holds a hand-off between the message claim and the
//! final claim, and the stand-in holds a provider call between the final claim and its answer, so
//! a competing change lands precisely there.

#[macro_use]
mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode, Version};
use axum::response::IntoResponse;
use axum::routing::post;
use chrono::Timelike;
use dumbmail::features::push::{crypto, limit, providers, send};
use dumbmail::models::{Profile, PROFILE_COLS};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use uuid::Uuid;

use common::*;

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
}

/// Holds requests whose path starts with a prefix until released.
#[derive(Clone)]
struct Gate {
    arrived: Arc<Semaphore>,
    go: Arc<Semaphore>,
}

#[derive(Default)]
struct MockState {
    hits: Vec<Hit>,
    /// How many times each path answered "busy" so far.
    busy: HashMap<String, u32>,
    gates: Vec<(String, Gate)>,
}

#[derive(Clone, Default)]
struct Mock {
    s: Arc<Mutex<MockState>>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

impl Mock {
    fn hits(&self, prefix: &str) -> Vec<Hit> {
        self.s.lock().unwrap().hits.iter().filter(|h| h.path.starts_with(prefix)).cloned().collect()
    }
    fn gate(&self, prefix: &str) -> Gate {
        let g = Gate { arrived: Arc::new(Semaphore::new(0)), go: Arc::new(Semaphore::new(0)) };
        self.s.lock().unwrap().gates.push((prefix.to_string(), g.clone()));
        g
    }
    async fn record(&self, path: String, version: Version, headers: HeaderMap, body: Bytes) {
        self.s.lock().unwrap().hits.push(Hit { path: path.clone(), version, headers, body });
        let gate = self.s.lock().unwrap().gates.iter().find(|(p, _)| path.starts_with(p.as_str())).map(|(_, g)| g.clone());
        if let Some(g) = gate {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            g.arrived.add_permits(1);
            g.go.acquire().await.unwrap().forget();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Gate {
    async fn wait(&self, n: u32) {
        self.arrived.acquire_many(n).await.unwrap().forget();
    }
    fn open(&self) {
        self.go.add_permits(10_000);
    }
}

/// The stand-in. Web endpoints: `gone-*` 410, `busy-*` 429 once, `drop-*` drops the connection
/// without answering. APNs tokens: `dead*` 410 Unregistered, `ab12*` 410 with an old timestamp,
/// `bad0*` 400 BadDeviceToken, `cafe*` 400 DeviceTokenNotForTopic. FCM tokens: `dead*` UNREGISTERED,
/// `nf*` a bare 404, `sid*` SENDER_ID_MISMATCH, `huge*` a 2 MB error body.
async fn mock() -> (Mock, String) {
    let m = Mock::default();
    let app = axum::Router::new()
        .route(
            "/webpush/{id}",
            post(|State(m): State<Mock>, Path(id): Path<String>, v: Version, h: HeaderMap, b: Bytes| async move {
                m.record(format!("/webpush/{id}"), v, h, b).await;
                if id.starts_with("drop") {
                    panic!("connection dropped on purpose");
                }
                if id.starts_with("gone") {
                    return StatusCode::GONE.into_response();
                }
                if id.starts_with("busy") {
                    let first = {
                        let mut s = m.s.lock().unwrap();
                        let n = s.busy.entry(id.clone()).or_default();
                        *n += 1;
                        *n == 1
                    };
                    if first {
                        return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "30")]).into_response();
                    }
                }
                (StatusCode::CREATED, [("location", format!("https://push.test/m/{id}"))]).into_response()
            }),
        )
        .route(
            "/apns/3/device/{token}",
            post(|State(m): State<Mock>, Path(token): Path<String>, v: Version, h: HeaderMap, b: Bytes| async move {
                m.record(format!("/apns/{token}"), v, h.clone(), b).await;
                let reason = |s: StatusCode, r: &str| (s, axum::Json(json!({"reason": r}))).into_response();
                if token.starts_with("dead") {
                    return reason(StatusCode::GONE, "Unregistered");
                }
                if token.starts_with("ab12") {
                    return (StatusCode::GONE, axum::Json(json!({"reason": "Unregistered", "timestamp": 1000}))).into_response();
                }
                if token.starts_with("bad0") {
                    return reason(StatusCode::BAD_REQUEST, "BadDeviceToken");
                }
                if token.starts_with("cafe") {
                    return reason(StatusCode::BAD_REQUEST, "DeviceTokenNotForTopic");
                }
                let id = h.get("apns-id").and_then(|v| v.to_str().ok()).unwrap_or("x").to_string();
                (StatusCode::OK, [("apns-id", id)]).into_response()
            }),
        )
        .route(
            "/oauth2/token",
            post(|State(m): State<Mock>, h: HeaderMap, b: Bytes| async move {
                m.record("/oauth2/token".into(), Version::HTTP_11, h, b.clone()).await;
                let form = String::from_utf8_lossy(&b).to_string();
                if !form.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer") || !form.contains("assertion=") {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"}))).into_response();
                }
                axum::Json(json!({"access_token": "ya29.mock", "expires_in": 3599, "token_type": "Bearer"})).into_response()
            }),
        )
        .route(
            "/fcm/v1/projects/{project}/messages:send",
            post(|State(m): State<Mock>, Path(project): Path<String>, h: HeaderMap, b: Bytes| async move {
                m.record(format!("/fcm/{project}"), Version::HTTP_11, h.clone(), b.clone()).await;
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer ya29.mock") {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": {"code": 401, "status": "UNAUTHENTICATED"}}))).into_response();
                }
                let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                let token = v["message"]["token"].as_str().unwrap_or("").to_string();
                let err = |s: StatusCode, status: &str, code: Option<&str>| {
                    let details = code.map(|c| json!([{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError", "errorCode": c}])).unwrap_or(json!([]));
                    (s, axum::Json(json!({"error": {"code": s.as_u16(), "status": status, "details": details}}))).into_response()
                };
                if token.starts_with("dead") {
                    return err(StatusCode::NOT_FOUND, "NOT_FOUND", Some("UNREGISTERED"));
                }
                if token.starts_with("nf") {
                    return err(StatusCode::NOT_FOUND, "NOT_FOUND", None);
                }
                if token.starts_with("sid") {
                    return err(StatusCode::FORBIDDEN, "PERMISSION_DENIED", Some("SENDER_ID_MISMATCH"));
                }
                if token.starts_with("huge") {
                    return (StatusCode::BAD_REQUEST, "x".repeat(2 * 1024 * 1024)).into_response();
                }
                axum::Json(json!({"name": format!("projects/{project}/messages/0:123")})).into_response()
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, base)
}

async fn harness_with_mock(price: f64) -> Option<(Harness, Mock, String)> {
    let (m, base) = mock().await;
    let b = base.clone();
    let h = Harness::with(move |c| {
        c.push_api_base = Some(b);
        c.price_push = price;
    })
    .await?;
    Some((h, m, base))
}

macro_rules! push_harness {
    ($price:expr) => {
        match harness_with_mock($price).await {
            Some(x) => x,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

/// A time zone where the local hour is in `hours` now (the fixed-offset zones cover every hour).
fn zone_where(hours: std::ops::Range<u32>) -> &'static str {
    const ZONES: &[&str] = &[
        "Etc/GMT+12", "Etc/GMT+11", "Etc/GMT+10", "Etc/GMT+9", "Etc/GMT+8", "Etc/GMT+7", "Etc/GMT+6", "Etc/GMT+5", "Etc/GMT+4", "Etc/GMT+3", "Etc/GMT+2", "Etc/GMT+1",
        "Etc/GMT", "Etc/GMT-1", "Etc/GMT-2", "Etc/GMT-3", "Etc/GMT-4", "Etc/GMT-5", "Etc/GMT-6", "Etc/GMT-7", "Etc/GMT-8", "Etc/GMT-9", "Etc/GMT-10", "Etc/GMT-11", "Etc/GMT-12",
    ];
    ZONES
        .iter()
        .copied()
        .find(|z| hours.contains(&chrono::Utc::now().with_timezone(&z.parse::<chrono_tz::Tz>().unwrap()).hour()))
        .expect("a zone in that range")
}

/// Midday somewhere (quiet hours never hold a send) and the middle of the night somewhere.
fn daytime() -> &'static str {
    zone_where(11..17)
}
fn night() -> &'static str {
    zone_where(1..5)
}

struct Acct {
    id: Uuid,
    key: String,
    secret: String,
}

async fn setup(h: &mut Harness) -> Acct {
    h.signup().await;
    h.ok("PATCH", "/api/account", Some(json!({"timezone": daytime()}))).await;
    let s = h.ok("PATCH", "/api/push/settings", Some(json!({"web_enabled": true}))).await;
    assert_eq!(s["web"]["ready"], true, "{s}");
    let secret = h.ok("POST", "/api/push/identity-secret", None).await["identity_secret"].as_str().unwrap().to_string();
    let (id, key): (Uuid, String) = sqlx::query_as("SELECT id, public_key FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    Acct { id, key, secret }
}

impl Acct {
    /// An enrollment grant as the customer's server makes it.
    fn grant(&self, id_type: &str, value: &str) -> Value {
        let exp = chrono::Utc::now().timestamp() + 600;
        json!({"type": id_type, "value": value, "grant": crypto::make_enroll_grant(&self.secret, &self.id.to_string(), id_type, value, exp)})
    }
}

async fn list(h: &mut Harness, name: &str) -> String {
    h.ok("POST", "/api/lists", Some(json!({"name": name}))).await["id"].as_str().unwrap().to_string()
}

async fn person(h: &mut Harness, list: &str, email: &str, first: &str) -> Uuid {
    let v = h.ok("POST", "/api/profiles", Some(json!({"email": email, "first_name": first, "list_ids": [list]}))).await;
    v["id"].as_str().unwrap().parse().unwrap()
}

#[derive(Clone)]
struct Browser {
    endpoint: String,
    p256dh: String,
    private: String,
    auth: String,
}

fn browser(base: &str, id: &str) -> Browser {
    let (p256dh, private, auth) = crypto::test_subscription_keys();
    Browser { endpoint: format!("{base}/webpush/{id}"), p256dh, private, auth }
}

fn post_req(path: &str, key: &str, ip: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("{path}?k={key}"))
        .header("content-type", "application/json")
        .header("x-real-ip", ip)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn public_post_from(h: &Harness, path: &str, key: &str, ip: &str, body: Value) -> (StatusCode, Value) {
    let res = h.router.clone().oneshot(post_req(path, key, ip, &body)).await.unwrap();
    let s = res.status();
    let b = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    (s, serde_json::from_slice(&b).unwrap_or(json!({"_raw": String::from_utf8_lossy(&b)})))
}

async fn public_post(h: &Harness, path: &str, key: &str, body: Value) -> (StatusCode, Value) {
    public_post_from(h, path, key, "203.0.113.9", body).await
}

async fn subscribe(h: &Harness, a: &Acct, b: &Browser, identity: Value) -> (StatusCode, Value) {
    public_post(h, "/client/push/subscribe", &a.key, json!({"subscription": {"endpoint": b.endpoint, "keys": {"p256dh": b.p256dh, "auth": b.auth}}, "identity": identity})).await
}

/// Enroll a browser for the person with this email (with a grant).
async fn enroll(h: &Harness, a: &Acct, b: &Browser, email: &str) {
    let (s, v) = subscribe(h, a, b, a.grant("email", email)).await;
    assert_eq!((s, v["linked"].as_bool()), (StatusCode::OK, Some(true)), "{v}");
}

/// Decrypt what the stand-in push service received, as the browser would.
fn opened(hit: &Hit, b: &Browser) -> Value {
    serde_json::from_slice(&crypto::decrypt(&b.private, &b.auth, &hit.body).expect("decrypts with the browser's key")).unwrap()
}

async fn push_campaign(h: &mut Harness, list: &str, content: Value) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Push", "channel": "push", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    c["id"].as_str().unwrap().to_string()
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

async fn events(h: &Harness, metric: &str) -> i64 {
    count(h, &format!("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = '{metric}'")).await
}

async fn message_of(h: &Harness, profile: Uuid) -> (Uuid, String, Option<String>) {
    sqlx::query_as("SELECT id, status, error FROM messages WHERE profile_id = $1 AND channel = 'push' AND NOT is_test ORDER BY created_at DESC LIMIT 1")
        .bind(profile)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn delivery_of(h: &Harness, message: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error FROM push_deliveries WHERE message_id = $1 ORDER BY created_at LIMIT 1").bind(message).fetch_one(&h.state.db).await.unwrap()
}

/// Time passes until every waiting retry is due, and the jobs run.
async fn retry_now(h: &Harness) {
    sqlx::query("UPDATE push_deliveries SET next_attempt_at = now() WHERE status = 'retry'").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
}

/// Run every queued job in the background, so the test can act while a hand-off is held.
fn drain_in_background(h: &Harness) -> tokio::task::JoinHandle<()> {
    let st = h.state.clone();
    tokio::spawn(async move {
        dumbmail::jobs::drain(&st).await.unwrap();
    })
}

#[tokio::test]
async fn web_push_end_to_end() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;

    // The worker, the importable handler and the snippet add-on carry the account's key and settings.
    let (s, _, sw) = h.raw("GET", &format!("/push/sw.js?k={}", a.key), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(sw.contains(&format!("\"{}\"", a.key)) && sw.contains("showNotification") && !sw.contains("skipWaiting") && !sw.contains("clients.claim"), "{sw}");
    let (_, _, handler) = h.raw("GET", &format!("/push/handler.js?k={}", a.key), &[], "").await;
    assert!(!handler.contains("showNotification"), "no handler to import into a site's own worker");
    let (_, _, js) = h.raw("GET", &format!("/client/push.js?k={}", a.key), &[], "").await;
    let settings = h.ok("GET", "/api/push/settings", None).await;
    let vapid = settings["web"]["vapid_public_key"].as_str().unwrap().to_string();
    assert!(js.contains(&vapid) && js.contains("/dumbmail-push/"), "registered with a scope of its own");
    assert!(!settings.to_string().contains("vapid_private"), "the private key is never shown");
    // The scope is fixed: it can't be set.
    let (s, _) = h.req("PATCH", "/api/push/settings", Some(json!({"sw_scope": "/"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let l = list(&mut h, "Push people").await;
    let ana = person(&mut h, &l, "ana@x.test", "Ana").await;
    person(&mut h, &l, "bo@x.test", "Bo").await;

    // A browser without a grant gets an anonymous profile of its own (it allowed notifications, so
    // it's reachable), never the person it names.
    let stranger = browser(&base, "s1");
    let (st, v) = subscribe(&h, &a, &stranger, json!({"type": "email", "value": "bo@x.test"})).await;
    assert_eq!((st, v["linked"].as_bool()), (StatusCode::OK, Some(false)), "{v}");
    let ana_browser = browser(&base, "a1");
    enroll(&h, &a, &ana_browser, "ana@x.test").await;
    let s = h.ok("GET", "/api/push/settings", None).await;
    assert_eq!((s["devices"]["web"].as_i64(), s["devices"]["anonymous"].as_i64(), s["reachable_people"].as_i64()), (Some(2), Some(1), Some(2)), "{s}");

    // Only people a push would reach count, and only on the platforms it targets.
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l}]}}))).await;
    assert_eq!(est["count"], 1);
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "push", "platforms": ["ios"], "audience": {"include": [{"type": "list", "id": l}]}}))).await;
    assert_eq!(est["count"], 0);

    let cid = push_campaign(
        &mut h,
        &l,
        json!({"title": "Hi {{ first_name }}", "body": "New arrivals are in", "url": "https://acme.test/new", "actions": [{"title": "Shop", "url": "https://acme.test/shop"}], "topic": "arrivals", "urgency": "high", "ttl_seconds": 3600}),
    )
    .await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;

    let web = m.hits("/webpush/");
    assert_eq!(web.len(), 1, "only Ana's browser is in the list");
    let hit = &web[0];
    assert_eq!(hit.path, "/webpush/a1");
    let hv = |k: &str| hit.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert_eq!((hv("content-encoding"), hv("ttl"), hv("urgency"), hv("topic")), ("aes128gcm".into(), "3600".into(), "high".into(), "arrivals".into()));
    let auth = hv("authorization");
    let (t, k) = auth.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
    assert_eq!(k, vapid);
    {
        use p256::ecdsa::signature::Verifier;
        let (signing, sig) = t.rsplit_once('.').unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&crypto::unb64(&vapid).unwrap()).unwrap();
        vk.verify(signing.as_bytes(), &p256::ecdsa::Signature::from_slice(&crypto::unb64(sig).unwrap()).unwrap()).expect("VAPID signature verifies");
    }
    let payload = opened(hit, &ana_browser);
    assert_eq!((payload["t"].as_str(), payload["dm"].as_i64(), payload["tag"].as_str()), (Some("Hi Ana"), Some(1), Some("arrivals")));
    let (mid, status, _) = message_of(&h, ana).await;
    assert_eq!(status, "sent");
    assert_eq!(events(&h, "Received Push").await, 1);

    // Shown: reported once; a made-up or expired token is refused.
    let (s, _) = public_post(&h, "/client/push/delivered", &a.key, json!({"t": payload["r"]})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = public_post(&h, "/client/push/delivered", &a.key, json!({"t": "pd0000.forged"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(message_of(&h, ana).await.1, "delivered");
    let did: Uuid = sqlx::query_scalar("SELECT id FROM push_deliveries WHERE message_id = $1").bind(mid).fetch_one(&h.state.db).await.unwrap();
    let expired = dumbmail::auth::sign(&h.state.cfg.secret_key, &format!("pd{}{:x}", did.simple(), chrono::Utc::now().timestamp() - 10));
    let (s, _, _) = h.raw("GET", &format!("/p/c/{expired}?u=https%3A%2F%2Facme.test%2Fnew&s=x"), &[], "").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an expired tap token is refused");

    // A tap goes through the tracked link; a link we didn't sign doesn't redirect; two taps count once.
    let link = payload["u"].as_str().unwrap();
    let path = link.split_once("/p/c/").map(|(_, p)| format!("/p/c/{p}")).unwrap();
    let (s, headers, _) = h.raw("GET", &path, &[], "").await;
    assert!(s.is_redirection(), "{s}");
    assert!(headers.get("location").unwrap().to_str().unwrap().starts_with("https://acme.test/new?_pl="));
    assert_eq!(h.raw("GET", &path.replace("acme.test%2Fnew", "evil.test%2Fnew"), &[], "").await.0, StatusCode::BAD_REQUEST);
    h.raw("GET", &path, &[], "").await;
    h.drain().await;
    assert_eq!(events(&h, "Clicked Push").await, 1);
    let report = h.ok("GET", &format!("/api/push/report?campaign_id={cid}"), None).await;
    assert_eq!((report["people"]["sent"].as_i64(), report["people"]["clicked"].as_i64(), report["platforms"][0]["delivered"].as_i64()), (Some(1), Some(1), Some(1)), "{report}");

    // An order after the tap: credited to the push, in the campaign and in attribution's push line.
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ana@x.test"}, "value": 42.5}))).await;
    let camp = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(camp["stats"]["revenue"], 42.5, "{camp}");
    let settings = dumbmail::features::reports::attribution::Settings::default();
    let att = dumbmail::features::reports::attribution::preview(&h.state, a.id, &settings, 30).await.unwrap();
    assert_eq!(att["current"]["totals"]["push_revenue"], 42.5, "{att}");

    // The browser can't forge the server's push events.
    let (s, _) = public_post(&h, "/client/events", &a.key, json!({"metric": "Clicked Push", "profile": {"email": "ana@x.test"}})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let seg = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"match": "all", "conditions": [{"type": "can_receive", "channel": "push"}]}}))).await;
    assert_eq!(seg["count"], 2, "Ana and the anonymous subscriber: {seg}");
    h.cleanup().await;
}

#[tokio::test]
async fn enrollment_needs_a_grant_made_for_it() {
    let (mut h, _m, base) = push_harness!(0.0);
    h.signup().await;
    h.ok("PATCH", "/api/push/settings", Some(json!({"web_enabled": true}))).await;
    let (account, key): (Uuid, String) = sqlx::query_as("SELECT id, public_key FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let l = list(&mut h, "L").await;
    let ana = person(&mut h, &l, "ana@x.test", "Ana").await;
    let b = browser(&base, "g1");
    let no_secret = Acct { id: account, key: key.clone(), secret: "x".into() };
    let (s, _) = subscribe(&h, &no_secret, &b, no_secret.grant("email", "ana@x.test")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no enrollment secret yet");
    let secret = h.ok("POST", "/api/push/identity-secret", None).await["identity_secret"].as_str().unwrap().to_string();
    let a = Acct { id: account, key, secret };
    assert!(!h.ok("GET", "/api/push/settings", None).await.to_string().contains(&a.secret), "shown once");

    let pdb = h.state.db.clone();
    let profile_of = |endpoint: String| {
        let db = pdb.clone();
        async move { sqlx::query_scalar::<_, Option<Uuid>>("SELECT profile_id FROM push_devices WHERE address = $1").bind(endpoint).fetch_one(&db).await.unwrap() }
    };
    // A tracked-link token (a forwarded email) is not a grant: the browser stays anonymous.
    let link_token = dumbmail::public::profile_token(&h.state, a.id, ana);
    let (_, v) = subscribe(&h, &a, &b, json!({"$token": link_token})).await;
    assert_eq!(v["linked"], false);
    assert_ne!(profile_of(b.endpoint.clone()).await, Some(ana));
    // Bound to the identifier's type, the value, the account and an expiry within a day.
    let exp = chrono::Utc::now().timestamp() + 600;
    let mk = |t: &str, v: &str, acct: &str, exp: i64| crypto::make_enroll_grant(&a.secret, acct, t, v, exp);
    for bad in [
        json!({"type": "external_id", "value": "ana@x.test", "grant": mk("email", "ana@x.test", &a.id.to_string(), exp)}),
        json!({"type": "email", "value": "bo@x.test", "grant": mk("email", "ana@x.test", &a.id.to_string(), exp)}),
        json!({"type": "email", "value": "ana@x.test", "grant": mk("email", "ana@x.test", &Uuid::new_v4().to_string(), exp)}),
        json!({"type": "email", "value": "ana@x.test", "grant": mk("email", "ana@x.test", &a.id.to_string(), chrono::Utc::now().timestamp() - 5)}),
        json!({"type": "email", "value": "ana@x.test", "grant": mk("email", "ana@x.test", &a.id.to_string(), chrono::Utc::now().timestamp() + 3 * 86_400)}),
    ] {
        let (s, v) = subscribe(&h, &a, &b, bad.clone()).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{bad}: {v}");
    }
    assert_ne!(profile_of(b.endpoint.clone()).await, Some(ana));
    // An external-id grant whose value happens to be Ana's email names a different person.
    let (_, v) = subscribe(&h, &a, &browser(&base, "g2"), a.grant("external_id", "ana@x.test")).await;
    assert_eq!(v["linked"], true);
    assert_ne!(profile_of(format!("{base}/webpush/g2")).await, Some(ana));
    // The right grant links; consent never changes.
    enroll(&h, &a, &b, "ana@x.test").await;
    assert_eq!(profile_of(b.endpoint.clone()).await, Some(ana));
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1").bind(ana).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed");
    let v1: i64 = sqlx::query_scalar("SELECT version FROM push_devices WHERE address = $1").bind(&b.endpoint).fetch_one(&h.state.db).await.unwrap();

    // Sign-out: the browser goes back to an anonymous profile (a new registration version).
    let (s, _) = public_post(&h, "/client/push/sign-out", &a.key, json!({"endpoint": b.endpoint, "auth": b.auth})).await;
    assert_eq!(s, StatusCode::OK);
    assert_ne!(profile_of(b.endpoint.clone()).await, Some(ana));
    let v2: i64 = sqlx::query_scalar("SELECT version FROM push_devices WHERE address = $1").bind(&b.endpoint).fetch_one(&h.state.db).await.unwrap();
    assert!(v2 > v1);
    let r = h.ok("GET", &format!("/api/push/profiles/{ana}"), None).await;
    assert_eq!(r["can_receive"], false, "{r}");
    h.cleanup().await;
}

async fn load(h: &Harness, account: Uuid, profile: Uuid) -> (dumbmail::models::Account, Profile) {
    let acct = dumbmail::campaigns::load_account(&h.state.db, account).await.unwrap();
    let p: Profile = sqlx::query_as(&format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1")).bind(profile).fetch_one(&h.state.db).await.unwrap();
    (acct, p)
}

#[tokio::test]
async fn a_push_goes_only_to_the_person_and_registration_it_was_made_for() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "L").await;
    let alice = person(&mut h, &l, "alice@x.test", "Alice").await;
    person(&mut h, &l, "bob@x.test", "Bob").await;

    // 1. Bob signs in on Alice's browser between the message claim and the hand-off.
    let shared = browser(&base, "shared");
    enroll(&h, &a, &shared, "alice@x.test").await;
    let cid = push_campaign(&mut h, &l, json!({"title": "For Alice"})).await;
    let pause = send::hook::Pause::hold(a.id);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let task = drain_in_background(&h);
    pause.arrived().await;
    enroll(&h, &a, &shared, "bob@x.test").await;
    pause.release(a.id);
    task.await.unwrap();
    assert!(m.hits("/webpush/shared").is_empty(), "Alice's push never reaches Bob");
    let (mid, status, error) = message_of(&h, alice).await;
    assert_eq!(status, "skipped", "{error:?}");
    assert_eq!(delivery_of(&h, mid).await.0, "skipped");

    // 2. The same while a retry waits.
    let busy = browser(&base, "busy-1");
    let carol = person(&mut h, &l, "carol@x.test", "Carol").await;
    enroll(&h, &a, &busy, "carol@x.test").await;
    let l2 = list(&mut h, "L2").await;
    h.ok("POST", &format!("/api/lists/{l2}/members"), Some(json!({"profile_ids": [carol]}))).await;
    let cid2 = push_campaign(&mut h, &l2, json!({"title": "For Carol"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid2}/send"), None).await;
    h.drain().await;
    let (mid2, status, _) = message_of(&h, carol).await;
    assert_eq!(status, "retrying");
    enroll(&h, &a, &busy, "bob@x.test").await;
    retry_now(&h).await;
    assert_eq!(m.hits("/webpush/busy-1").len(), 1, "the retry isn't handed to Bob's device");
    assert_eq!(delivery_of(&h, mid2).await.0, "skipped");

    // 3. A stale "gone" answer can't delete a newer registration at the same address.
    let gone = browser(&base, "gone-z");
    let dan = person(&mut h, &l2, "dan@x.test", "Dan").await;
    enroll(&h, &a, &gone, "dan@x.test").await;
    let gate = m.gate("/webpush/gone-z");
    let cid3 = push_campaign(&mut h, &l2, json!({"title": "For Dan"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid3}/send"), None).await;
    let task = drain_in_background(&h);
    gate.wait(1).await;
    // The browser renews its keys while the push service is answering.
    let renewed = Browser { endpoint: gone.endpoint.clone(), ..browser(&base, "gone-z") };
    enroll(&h, &a, &renewed, "dan@x.test").await;
    gate.open();
    task.await.unwrap();
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_devices WHERE profile_id = '{dan}'")).await, 1, "the renewed registration stays");
    assert_eq!(message_of(&h, dan).await.1, "failed");

    // 4. A test to one device still respects its owner's push opt-out.
    let erin = person(&mut h, &l2, "erin@x.test", "Erin").await;
    let eb = browser(&base, "e1");
    enroll(&h, &a, &eb, "erin@x.test").await;
    h.ok("PUT", &format!("/api/push/profiles/{erin}/opt-out"), Some(json!({"opted_out": true}))).await;
    let device: Uuid = sqlx::query_scalar("SELECT id FROM push_devices WHERE profile_id = $1").bind(erin).fetch_one(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", "/api/push/test", Some(json!({"device_id": device}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(m.hits("/webpush/e1").is_empty());

    // 5. The test-send path: the device changes hands between its claim and its hand-off.
    let fay = person(&mut h, &l2, "fay@x.test", "Fay").await;
    let fb = browser(&base, "f1");
    enroll(&h, &a, &fb, "fay@x.test").await;
    let (acct, fay_p) = load(&h, a.id, fay).await;
    let pause = send::hook::Pause::hold(a.id);
    let st = h.state.clone();
    let task = tokio::spawn(async move { send::test_to_profile(&st, &acct, &fay_p, &json!({"title": "Test"}), None, None).await.map(|o| format!("{o:?}")).map_err(|e| e.public_message()) });
    pause.arrived().await;
    enroll(&h, &a, &fb, "bob@x.test").await;
    pause.release(a.id);
    let out = task.await.unwrap();
    assert!(out.unwrap().contains("Skipped"));
    assert!(m.hits("/webpush/f1").is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn sent_exactly_once_or_honestly_unknown() {
    let (mut h, m, base) = push_harness!(0.01);
    let a = setup(&mut h).await;
    h.credit(5.0).await;
    let l = list(&mut h, "L").await;

    // No answer after the request went out: unconfirmed, never sent again.
    let dropped = person(&mut h, &l, "drop@x.test", "D").await;
    enroll(&h, &a, &browser(&base, "drop-1"), "drop@x.test").await;
    let cid = push_campaign(&mut h, &l, json!({"title": "Maybe"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let (mid, status, _) = message_of(&h, dropped).await;
    assert_eq!(status, "unconfirmed");
    assert_eq!(delivery_of(&h, mid).await.0, "unconfirmed");
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'push_retry'").await, 0);
    h.fast_forward().await;
    send::hand_off(&h.state, mid, send::Attempt::Later).await.unwrap();
    assert_eq!(m.hits("/webpush/drop-1").len(), 1, "never sent twice");
    let report = h.ok("GET", &format!("/api/push/report?campaign_id={cid}"), None).await;
    assert_eq!(report["people"]["unconfirmed"], 1, "{report}");

    // Acceptance, the charge and the event job commit together; a replay changes nothing.
    let l2 = list(&mut h, "L2").await;
    let ok = person(&mut h, &l2, "ok@x.test", "O").await;
    enroll(&h, &a, &browser(&base, "ok-1"), "ok@x.test").await;
    let cid2 = push_campaign(&mut h, &l2, json!({"title": "Yes"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid2}/send"), None).await;
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::MAIN).await.unwrap();
    let (mid2, status, _) = message_of(&h, ok).await;
    assert_eq!(status, "sent");
    let cost: Option<i64> = sqlx::query_scalar("SELECT cost_micros FROM messages WHERE id = $1").bind(mid2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(cost, Some(10_000));
    assert_eq!(count(&h, &format!("SELECT count(*) FROM jobs WHERE kind = 'push_event' AND payload->>'message_id' = '{mid2}'")).await, 1, "the event is queued with the acceptance");
    send::hand_off(&h.state, mid2, send::Attempt::Later).await.unwrap();
    h.drain().await;
    assert_eq!(m.hits("/webpush/ok-1").len(), 1);
    assert_eq!(count(&h, "SELECT count(*) FROM ledger WHERE kind = 'push'").await, 1);
    let charged: i64 = sqlx::query_scalar("SELECT -sum(amount_micros)::bigint FROM ledger WHERE kind = 'push'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(charged, 10_000, "charged once");
    assert_eq!(count(&h, &format!("SELECT count(*) FROM events WHERE message_id = '{mid2}'")).await, 1);

    // A flow's push step run again finds its message instead of sending a second.
    let l3 = list(&mut h, "Welcome").await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Welcome push", "trigger": {"type": "list", "list_id": l3}, "definition": {"entry": "p", "nodes": [
        {"id": "p", "type": "push", "content": {"title": "Welcome"}, "next": null}]}}))).await;
    let fid = flow["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", &format!("/api/lists/{l3}/members"), Some(json!({"profile_ids": [ok]}))).await;
    h.drain().await;
    assert_eq!(m.hits("/webpush/ok-1").len(), 2);
    let run: Uuid = sqlx::query_scalar("SELECT id FROM flow_runs WHERE flow_id = $1::uuid").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE flow_runs SET status = 'active', current_node = 'p', finished_at = NULL WHERE id = $1").bind(run).execute(&h.state.db).await.unwrap();
    dumbmail::flows::step(&h.state, run).await.unwrap();
    h.drain().await;
    assert_eq!(m.hits("/webpush/ok-1").len(), 2, "the re-run step sent nothing");
    assert_eq!(count(&h, &format!("SELECT count(*) FROM messages WHERE flow_id = '{fid}'")).await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn the_rules_are_checked_at_every_hand_off() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "L").await;
    let p = person(&mut h, &l, "p@x.test", "P").await;
    enroll(&h, &a, &browser(&base, "p1"), "p@x.test").await;

    // Cancelled between the claim and the hand-off.
    let cid = push_campaign(&mut h, &l, json!({"title": "One"})).await;
    let pause = send::hook::Pause::hold(a.id);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let task = drain_in_background(&h);
    pause.arrived().await;
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    pause.release(a.id);
    task.await.unwrap();
    assert!(m.hits("/webpush/p1").is_empty());
    assert_eq!(message_of(&h, p).await.2.as_deref(), Some("campaign cancelled"));

    // The account closed meanwhile (a retry is settled the same way).
    let cid = push_campaign(&mut h, &l, json!({"title": "Closing"})).await;
    let pause = send::hook::Pause::hold(a.id);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let task = drain_in_background(&h);
    pause.arrived().await;
    sqlx::query("UPDATE accounts SET closed_at = now() WHERE id = $1").bind(a.id).execute(&h.state.db).await.unwrap();
    pause.release(a.id);
    task.await.unwrap();
    assert!(m.hits("/webpush/p1").is_empty());
    assert_eq!(message_of(&h, p).await.2.as_deref(), Some("account closed"));
    sqlx::query("UPDATE accounts SET closed_at = NULL WHERE id = $1").bind(a.id).execute(&h.state.db).await.unwrap();

    // The platform turned off meanwhile.
    let cid = push_campaign(&mut h, &l, json!({"title": "Two"})).await;
    let pause = send::hook::Pause::hold(a.id);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let task = drain_in_background(&h);
    pause.arrived().await;
    h.ok("PATCH", "/api/push/settings", Some(json!({"web_enabled": false}))).await;
    pause.release(a.id);
    task.await.unwrap();
    assert!(m.hits("/webpush/p1").is_empty());
    assert_eq!(message_of(&h, p).await.1, "skipped");
    h.ok("PATCH", "/api/push/settings", Some(json!({"web_enabled": true}))).await;

    // The flow turned off meanwhile.
    let l2 = list(&mut h, "Flow").await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "list", "list_id": l2}, "definition": {"entry": "p", "nodes": [{"id": "p", "type": "push", "content": {"title": "Three"}, "next": null}]}}))).await;
    let fid = flow["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    let pause = send::hook::Pause::hold(a.id);
    h.ok("POST", &format!("/api/lists/{l2}/members"), Some(json!({"profile_ids": [p]}))).await;
    let task = drain_in_background(&h);
    pause.arrived().await;
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "paused"}))).await;
    pause.release(a.id);
    task.await.unwrap();
    assert!(m.hits("/webpush/p1").is_empty());
    assert_eq!(message_of(&h, p).await.2.as_deref(), Some("flow turned off"));

    // Quiet hours reached while a retry waited.
    let q = person(&mut h, &l, "q@x.test", "Q").await;
    enroll(&h, &a, &browser(&base, "busy-q"), "q@x.test").await;
    let l3 = list(&mut h, "Q").await;
    h.ok("POST", &format!("/api/lists/{l3}/members"), Some(json!({"profile_ids": [q]}))).await;
    let cid = push_campaign(&mut h, &l3, json!({"title": "Four"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(message_of(&h, q).await.1, "retrying");
    h.ok("PATCH", "/api/account", Some(json!({"timezone": night()}))).await;
    retry_now(&h).await;
    assert_eq!(m.hits("/webpush/busy-q").len(), 1);
    let (mid, _, _) = message_of(&h, q).await;
    assert_eq!(delivery_of(&h, mid).await, ("skipped".into(), Some("quiet hours".into())));
    h.ok("PATCH", "/api/account", Some(json!({"timezone": daytime()}))).await;

    // Smart sending: another push reached them while this one's retry waited.
    let r = person(&mut h, &l, "r@x.test", "R").await;
    enroll(&h, &a, &browser(&base, "busy-r"), "r@x.test").await;
    let l4 = list(&mut h, "R").await;
    h.ok("POST", &format!("/api/lists/{l4}/members"), Some(json!({"profile_ids": [r]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Smart", "channel": "push", "audience": {"include": [{"type": "list", "id": l4}]}, "content": {"title": "Five"}, "send_options": {"smart_sending": true}}))).await;
    let smart = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{smart}/send"), None).await;
    h.drain().await;
    let (smart_mid, status, _) = message_of(&h, r).await;
    assert_eq!(status, "retrying");
    let other = push_campaign(&mut h, &l4, json!({"title": "Six"})).await;
    h.ok("POST", &format!("/api/campaigns/{other}/send"), None).await;
    h.drain().await;
    assert_eq!(m.hits("/webpush/busy-r").len(), 2, "the other campaign went out");
    retry_now(&h).await;
    assert_eq!(m.hits("/webpush/busy-r").len(), 2);
    assert_eq!(delivery_of(&h, smart_mid).await, ("skipped".into(), Some("smart sending".into())));
    h.cleanup().await;
}

fn apns_key() -> String {
    use p256::pkcs8::EncodePrivateKey;
    p256::SecretKey::from_slice(&[7u8; 32]).unwrap().to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string()
}

fn service_account(project: &str) -> String {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
    json!({"type": "service_account", "project_id": project, "private_key_id": "kid123", "private_key": key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string(),
        "client_email": format!("push@{project}.iam.gserviceaccount.com"), "token_uri": "https://oauth2.googleapis.com/token"})
    .to_string()
}

async fn register(h: &Harness, a: &Acct, platform: &str, token: &str, permission: Option<&str>, email: &str) -> (StatusCode, Value) {
    let mut body = json!({"platform": platform, "token": token, "identity": a.grant("email", email)});
    if let Some(p) = permission {
        body["permission"] = json!(p);
    }
    public_post(h, "/client/push/devices", &a.key, body).await
}

async fn mobile_setup(h: &mut Harness) -> Acct {
    let a = setup(h).await;
    h.ok("PATCH", "/api/push/settings", Some(json!({"apns": {"key_id": "ABC123DEFG", "team_id": "TEAM123456", "bundle_id": "com.acme.app", "production": false, "key_p8": apns_key()}, "fcm": {"service_account_json": service_account("acme-app")}}))).await;
    a
}

#[tokio::test]
async fn apps_need_the_device_permission() {
    let (mut h, m, _base) = push_harness!(0.0);
    let a = mobile_setup(&mut h).await;
    let l = list(&mut h, "App").await;
    let ivy = person(&mut h, &l, "ivy@x.test", "Ivy").await;
    let dan = person(&mut h, &l, "dan@x.test", "Dan").await;
    let tok = "a".repeat(64);
    let (s, _) = register(&h, &a, "ios", &tok, None, "ivy@x.test").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the permission is required");
    let (s, v) = register(&h, &a, "ios", &tok, Some("not_determined"), "ivy@x.test").await;
    assert_eq!((s, v["sendable"].as_bool()), (StatusCode::OK, Some(false)));
    let r = h.ok("GET", &format!("/api/push/profiles/{ivy}"), None).await;
    assert_eq!(r["can_receive"], false, "a token without permission isn't reachable");
    // iOS provisional is sendable, and marked as such.
    register(&h, &a, "ios", &tok, Some("provisional"), "ivy@x.test").await;
    let r = h.ok("GET", &format!("/api/push/profiles/{ivy}"), None).await;
    assert_eq!((r["can_receive"].as_bool(), r["devices"][0]["permission"].as_str()), (Some(true), Some("provisional")), "{r}");
    let (s, _) = register(&h, &a, "android", &format!("fcm{}", "x".repeat(40)), Some("provisional"), "dan@x.test").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "provisional is iOS only");
    register(&h, &a, "android", &format!("fcm{}", "x".repeat(40)), Some("granted"), "dan@x.test").await;
    // Dan turns notifications off: the app reports it and he's no longer sent to.
    register(&h, &a, "android", &format!("fcm{}", "x".repeat(40)), Some("denied"), "dan@x.test").await;
    let cid = push_campaign(&mut h, &l, json!({"title": "Hi"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(m.hits("/apns/").len(), 1, "Ivy (provisional) gets it");
    assert!(m.hits("/fcm/").is_empty(), "Dan said no");
    assert_eq!(count(&h, &format!("SELECT count(*) FROM messages WHERE profile_id = '{dan}' AND status = 'sent'")).await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn apple_and_firebase_answers_are_read_right() {
    let (mut h, m, _base) = push_harness!(0.0);
    let a = mobile_setup(&mut h).await;
    let l = list(&mut h, "App").await;
    let ivy = person(&mut h, &l, "ivy@x.test", "Ivy").await;
    let dan = person(&mut h, &l, "dan@x.test", "Dan").await;
    let db = h.state.db.clone();
    let devices = |p: Uuid| {
        let db = db.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM push_devices WHERE profile_id = $1").bind(p).fetch_one(&db).await.unwrap() }
    };
    // Before this environment has accepted anything, BadDeviceToken may mean development vs
    // production: an account problem, nothing deleted.
    register(&h, &a, "ios", &format!("bad0{}", "1".repeat(60)), Some("granted"), "ivy@x.test").await;
    register(&h, &a, "ios", &format!("cafe{}", "1".repeat(60)), Some("granted"), "ivy@x.test").await;
    register(&h, &a, "android", &format!("nf{}", "x".repeat(40)), Some("granted"), "dan@x.test").await;
    register(&h, &a, "android", &format!("sid{}", "x".repeat(40)), Some("granted"), "dan@x.test").await;
    let cid = push_campaign(&mut h, &l, json!({"title": "One"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!((devices(ivy).await, devices(dan).await), (2, 2), "nothing deleted on account problems");
    let s = h.ok("GET", "/api/push/settings", None).await;
    assert!(s["apns"]["error"].as_str().is_some_and(|e| e.contains("BadDeviceToken") || e.contains("DeviceTokenNotForTopic")), "{s}");
    assert!(s["fcm"]["error"].as_str().is_some(), "{s}");
    assert_eq!(s["apns"]["verified"], false);

    // Once APNs accepted something here, BadDeviceToken is about the device; Unregistered too,
    // unless the device registered again after the time Apple gives.
    sqlx::query("DELETE FROM push_devices").execute(&h.state.db).await.unwrap();
    let good = "a".repeat(64);
    register(&h, &a, "ios", &good, Some("granted"), "ivy@x.test").await;
    register(&h, &a, "ios", &format!("bad0{}", "2".repeat(60)), Some("granted"), "ivy@x.test").await;
    register(&h, &a, "ios", &format!("dead{}", "3".repeat(60)), Some("granted"), "ivy@x.test").await;
    register(&h, &a, "ios", &format!("ab12{}", "4".repeat(60)), Some("granted"), "ivy@x.test").await;
    register(&h, &a, "android", &format!("dead{}", "x".repeat(40)), Some("granted"), "dan@x.test").await;
    register(&h, &a, "android", &format!("huge{}", "x".repeat(40)), Some("granted"), "dan@x.test").await;
    sqlx::query("UPDATE push_settings SET apns_verified_at = now()").execute(&h.state.db).await.unwrap();
    let cid = push_campaign(&mut h, &l, json!({"title": "Two"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let ok = m.hits(&format!("/apns/{good}"));
    assert_eq!(ok.last().unwrap().version, Version::HTTP_2);
    assert_eq!(devices(ivy).await, 2, "the good one and the re-registered one stay");
    assert_eq!(devices(dan).await, 1, "UNREGISTERED is removed; a huge error body is just a failure");

    // Connections and tokens are per account and credential version: new credentials, new ones.
    assert_eq!(providers::apns_pools_for(a.id).await, 1);
    assert_eq!(providers::fcm_tokens_for(a.id).await, 1);
    assert_eq!(m.hits("/oauth2/token").len(), 1, "token reused across sends");
    h.ok("PATCH", "/api/push/settings", Some(json!({"apns": {"key_id": "ZZZ123DEFG", "team_id": "TEAM999999", "bundle_id": "com.acme.app", "production": false}, "fcm": {"service_account_json": service_account("acme-app")}}))).await;
    assert_eq!((providers::apns_pools_for(a.id).await, providers::fcm_tokens_for(a.id).await), (0, 0));
    let cid = push_campaign(&mut h, &l, json!({"title": "Three"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(m.hits("/oauth2/token").len(), 2);
    assert_eq!(providers::apns_pools_for(a.id).await, 1);
    let jwt = m.hits(&format!("/apns/{good}")).last().unwrap().headers.get("authorization").unwrap().to_str().unwrap().to_string();
    let header: Value = serde_json::from_slice(&crypto::unb64(jwt.strip_prefix("bearer ").unwrap().split('.').next().unwrap()).unwrap()).unwrap();
    assert_eq!(header["kid"], "ZZZ123DEFG");
    h.cleanup().await;
}

#[tokio::test]
async fn work_is_bounded() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "L").await;
    let p = person(&mut h, &l, "p@x.test", "P").await;
    // Ten devices for one person: at most eight provider calls at once for the account.
    for i in 0..10 {
        let (s, v) = public_post_from(&h, "/client/push/subscribe", &a.key, &format!("198.51.100.{i}"), json!({"subscription": {"endpoint": format!("{base}/webpush/hold-{i}"), "keys": {"p256dh": browser(&base, "x").p256dh, "auth": browser(&base, "x").auth}}, "identity": a.grant("email", "p@x.test")})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let gate = m.gate("/webpush/hold-");
    let cid = push_campaign(&mut h, &l, json!({"title": "Many"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let task = drain_in_background(&h);
    gate.wait(limit::PER_ACCOUNT as u32).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(m.in_flight.load(Ordering::SeqCst), limit::PER_ACCOUNT);
    gate.open();
    task.await.unwrap();
    assert_eq!(m.hits("/webpush/hold-").len(), 10);
    assert!(m.max_in_flight.load(Ordering::SeqCst) <= limit::PER_ACCOUNT);
    assert_eq!(message_of(&h, p).await.1, "sent");

    // Caps hold under concurrent registrations: ten devices per person, thirty per address an hour.
    let q = person(&mut h, &l, "q@x.test", "Q").await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let b = browser(&base, &format!("cap-{i}"));
        let req = post_req("/client/push/subscribe", &a.key, &format!("192.0.2.{i}"), &json!({"subscription": {"endpoint": b.endpoint, "keys": {"p256dh": b.p256dh, "auth": b.auth}}, "identity": a.grant("email", "q@x.test")}));
        let router = h.router.clone();
        tasks.push(tokio::spawn(async move { router.oneshot(req).await.unwrap().status() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_devices WHERE profile_id = '{q}' AND status = 'active'")).await, 10);
    let mut tasks = Vec::new();
    for i in 0..40 {
        let b = browser(&base, &format!("ip-{i}"));
        let req = post_req("/client/push/subscribe", &a.key, "203.0.113.77", &json!({"subscription": {"endpoint": b.endpoint, "keys": {"p256dh": b.p256dh, "auth": b.auth}}}));
        let router = h.router.clone();
        tasks.push(tokio::spawn(async move { router.oneshot(req).await.unwrap().status() }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 30);

    // Upkeep runs as a background-lane job; registrations turned off a month ago are deleted.
    sqlx::query("UPDATE push_devices SET status = 'disabled', disabled_at = now() - interval '31 days' WHERE profile_id = $1 AND status = 'disabled'").bind(q).execute(&h.state.db).await.unwrap();
    dumbmail::features::tick(&h.state, 0).await;
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'push_cleanup' AND lane = 'background'").await, 1);
    h.drain().await;
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_devices WHERE profile_id = '{q}'")).await, 10);
    h.cleanup().await;
}

#[tokio::test]
async fn reports_say_what_happened() {
    let (mut h, m, base) = push_harness!(0.01);
    let a = setup(&mut h).await;
    h.credit(5.0).await;
    let l = list(&mut h, "L").await;

    // Not sent while its only device is still waiting to retry.
    let w = person(&mut h, &l, "w@x.test", "W").await;
    enroll(&h, &a, &browser(&base, "busy-w"), "w@x.test").await;
    let cid = push_campaign(&mut h, &l, json!({"title": "Wait"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(message_of(&h, w).await.1, "retrying");
    let camp = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(camp["stats"]["delivered"], 0, "{camp}");
    assert_eq!(count(&h, "SELECT count(*) FROM ledger WHERE kind = 'push'").await, 0, "not charged yet");

    // The browser shows it before the push service's answer is saved: kept and reconciled.
    let l2 = list(&mut h, "L2").await;
    let s = person(&mut h, &l2, "s@x.test", "S").await;
    let slow = browser(&base, "slow-1");
    enroll(&h, &a, &slow, "s@x.test").await;
    let second = browser(&base, "slow-2");
    enroll(&h, &a, &second, "s@x.test").await;
    let gate = m.gate("/webpush/slow-");
    let cid2 = push_campaign(&mut h, &l2, json!({"title": "Fast"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid2}/send"), None).await;
    let task = drain_in_background(&h);
    gate.wait(2).await;
    let p1 = opened(&m.hits("/webpush/slow-1")[0], &slow);
    let p2 = opened(&m.hits("/webpush/slow-2")[0], &second);
    let (st, _) = public_post(&h, "/client/push/delivered", &a.key, json!({"t": p1["r"]})).await;
    assert_eq!(st, StatusCode::OK);
    gate.open();
    task.await.unwrap();
    h.drain().await;
    let (mid, status, _) = message_of(&h, s).await;
    assert_eq!(status, "delivered");
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_deliveries WHERE message_id = '{mid}' AND delivered_at IS NOT NULL")).await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM ledger WHERE kind = 'push'").await, 1, "charged once");
    assert_eq!(events(&h, "Received Push").await, 1);

    // Taps on two devices: one "Clicked Push" for the message.
    for p in [&p1, &p2] {
        let link = p["u"].as_str().unwrap();
        let path = link.split_once("/p/c/").map(|(_, x)| format!("/p/c/{x}")).unwrap();
        assert!(h.raw("GET", &path, &[], "").await.0.is_redirection());
    }
    h.drain().await;
    assert_eq!(events(&h, "Clicked Push").await, 1);

    // The send-time preview waits for quiet hours, as the send does.
    h.ok("PATCH", "/api/account", Some(json!({"timezone": night()}))).await;
    let pv = h.ok("POST", "/api/sendtime/preview", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l2}]}}))).await;
    let at: chrono::DateTime<chrono::Utc> = pv["buckets"][0]["at"].as_str().unwrap().parse().unwrap();
    assert!(at > chrono::Utc::now() + chrono::Duration::hours(2), "{pv}");
    h.cleanup().await;
}

#[tokio::test]
async fn the_marketing_hold_is_about_email_not_push() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "Held").await;
    let held = person(&mut h, &l, "held@x.test", "Hal").await;
    sqlx::query("UPDATE profiles SET marketing_hold = true WHERE id = $1").bind(held).execute(&h.state.db).await.unwrap();
    enroll(&h, &a, &browser(&base, "held-1"), "held@x.test").await;
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": l}]}}))).await;
    assert_eq!(est["count"], 0, "no marketing email");
    for platforms in [json!(null), json!(["web"])] {
        let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "push", "platforms": platforms, "audience": {"include": [{"type": "list", "id": l}]}}))).await;
        assert_eq!(est["count"], 1, "push reaches them");
    }
    let cid = push_campaign(&mut h, &l, json!({"title": "Allowed"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(m.hits("/webpush/held-1").len(), 1, "a push campaign reaches a held person who allowed notifications");
    let l2 = list(&mut h, "Flow").await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "list", "list_id": l2}, "definition": {"entry": "p", "nodes": [{"id": "p", "type": "push", "content": {"title": "Flow"}, "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", &format!("/api/lists/{l2}/members"), Some(json!({"profile_ids": [held]}))).await;
    h.drain().await;
    assert_eq!(m.hits("/webpush/held-1").len(), 2, "and so does a push flow step");
    h.cleanup().await;
}

#[tokio::test]
async fn flows_tests_and_isolation() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "Welcome").await;
    let (s, _) = h.req("POST", "/api/campaigns", Some(json!({"channel": "push", "content": {"title": "Code {% coupon_code 'WELCOME' %}"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let wid = person(&mut h, &l, "w@x.test", "Wen").await;
    let wb = browser(&base, "w1");
    enroll(&h, &a, &wb, "w@x.test").await;
    let cid = push_campaign(&mut h, &l, json!({"title": "Test me"})).await;
    let r = h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["w@x.test"]}))).await;
    assert_eq!(r["sent"], 1);
    assert_eq!(opened(&m.hits("/webpush/w1")[0], &wb)["t"], "[Test] Test me");
    let mine = browser(&base, "me1");
    h.ok("POST", "/api/push/test-devices", Some(json!({"subscription": {"endpoint": mine.endpoint, "keys": {"p256dh": mine.p256dh, "auth": mine.auth}}}))).await;
    let r = h.ok("POST", "/api/push/test", Some(json!({}))).await;
    assert_eq!(r["devices"][0]["status"], "sent", "{r}");
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l}]}}))).await;
    assert_eq!(est["count"], 1, "test devices aren't in audiences");
    let (s, _) = subscribe(&h, &a, &Browser { endpoint: "http://169.254.169.254/latest".into(), ..browser(&base, "x") }, json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let device: Uuid = sqlx::query_scalar("SELECT id FROM push_devices WHERE profile_id = $1").bind(wid).fetch_one(&h.state.db).await.unwrap();
    let mut h2 = Harness { cookie: None, ..h };
    h2.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    assert_eq!(h2.req("GET", &format!("/api/push/profiles/{wid}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h2.req("POST", "/api/push/test", Some(json!({"device_id": device}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h2.ok("GET", &format!("/api/push/report?campaign_id={cid}"), None).await["people"]["sent"], 0);
    h2.cleanup().await;
}

#[tokio::test]
async fn public_endpoints_refuse_unknown_keys_and_setup_needs_settings_permission() {
    let mut h = harness!();
    let (s, _, _) = h.raw("POST", "/client/push/devices?k=nope", &[("content-type", "application/json")], &json!({"platform": "ios", "token": "a".repeat(64), "permission": "granted"}).to_string()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = h.req("GET", "/api/push/settings", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.signup().await;
    let l = list(&mut h, "L").await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l}]}, "content": {"title": "Hi"}}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("Settings → Push"), "{v}");
    h.cleanup().await;
}

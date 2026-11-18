//! Push notifications end to end: web push against a local stand-in for a browser push service
//! (the payload is decrypted with the browser's key), APNs over HTTP/2 and FCM v1 with its OAuth
//! token endpoint, linking devices only with signed identities, gone and busy devices, opt-outs
//! winning over queued sends, retries that never send twice, flows, segments, billing, limits and
//! account isolation.

#[macro_use]
mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Version};
use axum::response::IntoResponse;
use axum::routing::post;
use chrono::Timelike;
use dumbmail::features::push::crypto;
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Default)]
struct MockState {
    hits: Vec<Hit>,
    /// How many times each path answered "busy" so far.
    busy: HashMap<String, u32>,
}

type Mock = Arc<Mutex<MockState>>;

/// A stand-in for the browsers' push services, APNs, FCM and Google's token endpoint.
/// Web: endpoints `/webpush/gone-*` answer 410, `/webpush/busy-*` 429 the first time.
/// APNs and FCM: device tokens starting with `dead` are unregistered.
async fn mock() -> (Mock, String) {
    async fn record(m: &Mock, path: String, version: Version, headers: HeaderMap, body: Bytes) {
        m.lock().unwrap().hits.push(Hit { path, version, headers, body });
    }
    let m: Mock = Default::default();
    let app = axum::Router::new()
        .route(
            "/webpush/{id}",
            post(|State(m): State<Mock>, Path(id): Path<String>, v: Version, h: HeaderMap, b: Bytes| async move {
                record(&m, format!("/webpush/{id}"), v, h, b).await;
                if id.starts_with("gone") {
                    return StatusCode::GONE.into_response();
                }
                if id.starts_with("busy") {
                    let mut s = m.lock().unwrap();
                    let n = s.busy.entry(id.clone()).or_default();
                    *n += 1;
                    if *n == 1 {
                        return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "30")]).into_response();
                    }
                }
                (StatusCode::CREATED, [("location", format!("https://push.test/m/{id}"))]).into_response()
            }),
        )
        .route(
            "/apns/3/device/{token}",
            post(|State(m): State<Mock>, Path(token): Path<String>, v: Version, h: HeaderMap, b: Bytes| async move {
                record(&m, format!("/apns/3/device/{token}"), v, h.clone(), b).await;
                if token.starts_with("dead") {
                    return (StatusCode::GONE, axum::Json(json!({"reason": "Unregistered", "timestamp": 1}))).into_response();
                }
                let id = h.get("apns-id").and_then(|v| v.to_str().ok()).unwrap_or("x").to_string();
                (StatusCode::OK, [("apns-id", id)]).into_response()
            }),
        )
        .route(
            "/oauth2/token",
            post(|State(m): State<Mock>, h: HeaderMap, b: Bytes| async move {
                record(&m, "/oauth2/token".into(), Version::HTTP_11, h, b.clone()).await;
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
                record(&m, format!("/fcm/{project}"), Version::HTTP_11, h.clone(), b.clone()).await;
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer ya29.mock") {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": {"code": 401, "status": "UNAUTHENTICATED"}}))).into_response();
                }
                let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                if v["message"]["token"].as_str().unwrap_or("").starts_with("dead") {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"code": 404, "status": "NOT_FOUND", "details": [{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError", "errorCode": "UNREGISTERED"}]}}))).into_response();
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

fn hits(m: &Mock, prefix: &str) -> Vec<Hit> {
    m.lock().unwrap().hits.iter().filter(|h| h.path.starts_with(prefix)).cloned().collect()
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

/// A time zone where it's the middle of the day now, so quiet hours never hold a test's push.
fn daytime_zone() -> &'static str {
    for z in ["America/Los_Angeles", "America/New_York", "Europe/London", "Europe/Berlin", "Asia/Dubai", "Asia/Kolkata", "Asia/Singapore", "Asia/Tokyo", "Australia/Sydney", "Pacific/Auckland", "Pacific/Honolulu", "America/Anchorage"] {
        let tz: chrono_tz::Tz = z.parse().unwrap();
        let hour = chrono::Utc::now().with_timezone(&tz).hour();
        if (11..17).contains(&hour) {
            return z;
        }
    }
    panic!("no daytime zone found");
}

struct Acct {
    id: Uuid,
    key: String,
}

async fn setup(h: &mut Harness) -> Acct {
    h.signup().await;
    h.ok("PATCH", "/api/account", Some(json!({"timezone": daytime_zone()}))).await;
    let s = h.ok("PATCH", "/api/push/settings", Some(json!({"web_enabled": true}))).await;
    assert_eq!(s["web"]["ready"], true, "{s}");
    let (id, key): (Uuid, String) = sqlx::query_as("SELECT id, public_key FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    Acct { id, key }
}

async fn list(h: &mut Harness, name: &str) -> String {
    h.ok("POST", "/api/lists", Some(json!({"name": name}))).await["id"].as_str().unwrap().to_string()
}

async fn person(h: &mut Harness, list: &str, email: &str, first: &str) -> Uuid {
    let v = h.ok("POST", "/api/profiles", Some(json!({"email": email, "first_name": first, "list_ids": [list]}))).await;
    v["id"].as_str().unwrap().parse().unwrap()
}

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

async fn public_post(h: &Harness, path: &str, key: &str, body: Value) -> (StatusCode, Value) {
    let (s, _, b) = h.raw("POST", &format!("{path}?k={key}"), &[("content-type", "application/json"), ("x-real-ip", "203.0.113.9")], &body.to_string()).await;
    (s, serde_json::from_str(&b).unwrap_or(json!({"_raw": b})))
}

async fn subscribe(h: &Harness, a: &Acct, b: &Browser, identity: Value) -> (StatusCode, Value) {
    public_post(h, "/client/push/subscribe", &a.key, json!({"subscription": {"endpoint": b.endpoint, "keys": {"p256dh": b.p256dh, "auth": b.auth}}, "identity": identity})).await
}

fn link_token(h: &Harness, a: &Acct, profile: Uuid) -> String {
    dumbmail::public::profile_token(&h.state, a.id, profile)
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

#[tokio::test]
async fn web_push_end_to_end() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;

    // The service worker and the snippet add-on carry the account's key and settings.
    let (s, headers, sw) = h.raw("GET", &format!("/push/sw.js?k={}", a.key), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(sw.contains(&format!("\"{}\"", a.key)) && sw.contains("showNotification"), "{sw}");
    assert_eq!(headers.get("service-worker-allowed").unwrap(), "/");
    let (_, _, js) = h.raw("GET", &format!("/client/push.js?k={}", a.key), &[], "").await;
    let settings = h.ok("GET", "/api/push/settings", None).await;
    let vapid = settings["web"]["vapid_public_key"].as_str().unwrap().to_string();
    assert!(js.contains(&vapid) && js.contains("/dumbmail-sw.js"));
    assert!(!settings.to_string().contains("vapid_private"), "the private key is never shown");
    let (_, _, off) = h.raw("GET", "/client/push.js?k=nope", &[], "").await;
    assert!(off.contains("CFG = null"), "an unknown key gets the no-op script");

    let l = list(&mut h, "Push people").await;
    let ana = person(&mut h, &l, "ana@x.test", "Ana").await;
    let bo = person(&mut h, &l, "bo@x.test", "Bo").await;

    // A browser naming someone without a signed identity stays unlinked, however it asks.
    let stranger = browser(&base, "s1");
    let (st, v) = subscribe(&h, &a, &stranger, json!({"email": "bo@x.test", "external_id": "x"})).await;
    assert_eq!((st, v["linked"].as_bool()), (StatusCode::OK, Some(false)), "{v}");
    // Ana arrived from a tracked link: her browser is hers.
    let ana_browser = browser(&base, "a1");
    let (_, v) = subscribe(&h, &a, &ana_browser, json!({"$token": link_token(&h, &a, ana)})).await;
    assert_eq!(v["linked"], true, "{v}");
    // A forged link token links nothing.
    let (_, v) = subscribe(&h, &a, &browser(&base, "f1"), json!({"$token": format!("p{}aaaa.zzzz", bo.simple())})).await;
    assert_eq!(v["linked"], false);
    let s = h.ok("GET", "/api/push/settings", None).await;
    assert_eq!((s["devices"]["web"].as_i64(), s["devices"]["not_linked"].as_i64(), s["reachable_people"].as_i64()), (Some(1), Some(2), Some(1)), "{s}");

    // Only people a push would reach count.
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l}]}}))).await;
    assert_eq!(est["count"], 1);

    let cid = push_campaign(
        &mut h,
        &l,
        json!({"title": "Hi {{ first_name }}", "body": "New arrivals are in", "url": "https://acme.test/new", "actions": [{"title": "Shop", "url": "https://acme.test/shop"}], "topic": "arrivals", "urgency": "high", "ttl_seconds": 3600}),
    )
    .await;
    // Preview renders the person's notification.
    let pv = h.ok("POST", "/api/render", Some(json!({"channel": "push", "content": {"title": "Hi {{ first_name }}", "body": "x"}, "profile_id": ana}))).await;
    assert_eq!(pv["title"], "Hi Ana");
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;

    let web = hits(&m, "/webpush/");
    assert_eq!(web.len(), 1, "only Ana's linked browser gets it");
    let hit = &web[0];
    assert_eq!(hit.path, "/webpush/a1");
    let hv = |k: &str| hit.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert_eq!((hv("content-encoding"), hv("ttl"), hv("urgency"), hv("topic")), ("aes128gcm".into(), "3600".into(), "high".into(), "arrivals".into()));
    // RFC 8292: vapid t=<ES256 JWT for the push service's origin>, k=<the account's public key>.
    let auth = hv("authorization");
    let (t, k) = auth.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
    assert_eq!(k, vapid);
    let claims: Value = serde_json::from_slice(&crypto::unb64(t.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!(claims["aud"], base);
    {
        use p256::ecdsa::signature::Verifier;
        let (signing, sig) = t.rsplit_once('.').unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&crypto::unb64(&vapid).unwrap()).unwrap();
        vk.verify(signing.as_bytes(), &p256::ecdsa::Signature::from_slice(&crypto::unb64(sig).unwrap()).unwrap()).expect("VAPID signature verifies");
    }
    let payload = opened(hit, &ana_browser);
    assert_eq!((payload["t"].as_str(), payload["b"].as_str(), payload["tag"].as_str()), (Some("Hi Ana"), Some("New arrivals are in"), Some("arrivals")));
    assert_eq!(payload["a"][0]["t"], "Shop");

    let (status, error): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM messages WHERE campaign_id = $1::uuid AND profile_id = $2").bind(&cid).bind(ana).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "sent", "{error:?}");
    let camp = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(camp["status"], "sent");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Received Push'").await, 1);

    // The service worker reports it was shown; a made-up token is refused.
    let (s, _) = public_post(&h, "/client/push/delivered", &a.key, json!({"t": payload["r"]})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = public_post(&h, "/client/push/delivered", &a.key, json!({"t": "pd0000.forged"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM messages WHERE campaign_id = '{cid}' AND status = 'delivered'")).await, 1);

    // A tap goes through the tracked link to the page, recognizing her on the brand's own site.
    let link = payload["u"].as_str().unwrap();
    let path = link.split_once("/p/c/").map(|(_, p)| format!("/p/c/{p}")).unwrap();
    let (s, headers, _) = h.raw("GET", &path, &[], "").await;
    assert!(s.is_redirection(), "{s}");
    let to = headers.get("location").unwrap().to_str().unwrap();
    assert!(to.starts_with("https://acme.test/new?_pl="), "{to}");
    // A link we didn't sign doesn't redirect.
    let tampered = path.replace("acme.test%2Fnew", "evil.test%2Fnew");
    assert_eq!(h.raw("GET", &tampered, &[], "").await.0, StatusCode::BAD_REQUEST);
    h.raw("GET", &path, &[], "").await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Clicked Push'").await, 1, "a second tap counts once");
    let report = h.ok("GET", &format!("/api/push/report?campaign_id={cid}"), None).await;
    assert_eq!(report["people"]["sent"], 1, "{report}");
    assert_eq!(report["people"]["clicked"], 1);
    assert_eq!(report["platforms"][0]["delivered"], 1);

    // The browser can't forge the server's push events.
    let (s, v) = public_post(&h, "/client/events", &a.key, json!({"metric": "Clicked Push", "profile": {"email": "ana@x.test"}})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // Segments: can_receive push.
    let seg = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"match": "all", "conditions": [{"type": "can_receive", "channel": "push"}]}}))).await;
    assert_eq!(seg["count"], 1, "{seg}");
    let (s, _) = h.req("POST", "/api/segments/preview", Some(json!({"definition": {"match": "all", "conditions": [{"type": "consent", "channel": "push", "status": "subscribed"}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn identity_verification_links_only_with_a_valid_hash() {
    let (mut h, _m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let b = browser(&base, "i1");
    let (s, _) = subscribe(&h, &a, &b, json!({"external_id": "cust_1", "user_hash": "00"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no identity secret yet");

    let r = h.ok("POST", "/api/push/identity-secret", None).await;
    let secret = r["identity_secret"].as_str().unwrap().to_string();
    let s = h.ok("GET", "/api/push/settings", None).await;
    assert!(!s.to_string().contains(&secret), "shown once, never again");
    assert_eq!(s["identity"]["hint"].as_str().unwrap(), format!("…{}", &secret[secret.len() - 4..]));

    let (s, _) = subscribe(&h, &a, &b, json!({"external_id": "cust_1", "user_hash": crypto::hmac_hex(&secret, "cust_2")})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = subscribe(&h, &a, &b, json!({"external_id": "cust_1", "user_hash": crypto::hmac_hex(&secret, "cust_1")})).await;
    assert_eq!((s, v["linked"].as_bool()), (StatusCode::OK, Some(true)), "{v}");
    let (pid, consent): (Uuid, String) = sqlx::query_as("SELECT id, email_consent FROM profiles WHERE external_id = 'cust_1'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed", "a verified identity never changes consent");
    let devices = h.ok("GET", &format!("/api/push/profiles/{pid}"), None).await;
    assert_eq!(devices["devices"].as_array().unwrap().len(), 1);
    assert_eq!(devices["can_receive"], true);

    // Email works too (lowercased).
    let (_, v) = subscribe(&h, &a, &browser(&base, "i2"), json!({"email": "Zed@X.test", "user_hash": crypto::hmac_hex(&secret, "zed@x.test")})).await;
    assert_eq!(v["linked"], true, "{v}");
    let audit = count(&h, "SELECT count(*) FROM audit_log WHERE action = 'push.identity_secret_rotate'").await;
    assert_eq!(audit, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn gone_devices_are_removed_and_busy_ones_retry_once() {
    let (mut h, m, base) = push_harness!(0.01);
    let a = setup(&mut h).await;
    h.credit(5.0).await;
    let l = list(&mut h, "L").await;
    let gone = person(&mut h, &l, "gone@x.test", "G").await;
    let busy = person(&mut h, &l, "busy@x.test", "B").await;
    subscribe(&h, &a, &browser(&base, "gone-1"), json!({"$token": link_token(&h, &a, gone)})).await;
    let busy_browser = browser(&base, "busy-1");
    subscribe(&h, &a, &busy_browser, json!({"$token": link_token(&h, &a, busy)})).await;
    let balance_before: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts WHERE id = $1").bind(a.id).fetch_one(&h.state.db).await.unwrap();

    let cid = push_campaign(&mut h, &l, json!({"title": "Sale", "body": "Today only"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;

    // 410: the device is removed and the message failed (and reported as unsent).
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_devices WHERE profile_id = '{gone}'")).await, 0);
    let (st,): (String,) = sqlx::query_as("SELECT status FROM messages WHERE profile_id = $1").bind(gone).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "failed");
    // 429: waiting for a retry in the background lane, not charged yet.
    let (st, next): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as("SELECT d.status, d.next_attempt_at FROM push_deliveries d JOIN messages m ON m.id = d.message_id WHERE m.profile_id = $1").bind(busy).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "retry");
    assert!(next.unwrap() > chrono::Utc::now() + chrono::Duration::seconds(25), "honours Retry-After");
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'push_retry' AND lane = 'background'").await, 1);
    let camp = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(camp["status"], "sent", "a waiting retry doesn't hold the campaign up");

    h.fast_forward().await;
    assert_eq!(hits(&m, "/webpush/busy-1").len(), 2);
    assert_eq!(opened(&hits(&m, "/webpush/busy-1")[1], &busy_browser)["t"], "Sale");
    let (st, sent_at): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as("SELECT status, sent_at FROM messages WHERE profile_id = $1").bind(busy).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "sent");
    assert!(sent_at.is_some());
    // Charged once, for the one person it reached.
    let balance_after: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts WHERE id = $1").bind(a.id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(balance_before - balance_after, 10_000);
    assert_eq!(count(&h, "SELECT count(*) FROM ledger WHERE kind = 'push'").await, 1);

    // A replayed retry job finds nothing to do.
    let did: Uuid = sqlx::query_scalar("SELECT d.id FROM push_deliveries d JOIN messages m ON m.id = d.message_id WHERE m.profile_id = $1").bind(busy).fetch_one(&h.state.db).await.unwrap();
    dumbmail::features::push::send::retry(&h.state, did).await.unwrap();
    assert_eq!(hits(&m, "/webpush/busy-1").len(), 2, "never sent twice");
    h.cleanup().await;
}

#[tokio::test]
async fn an_opt_out_wins_over_queued_and_waiting_sends() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "L").await;
    let p = person(&mut h, &l, "p@x.test", "P").await;
    let q = person(&mut h, &l, "q@x.test", "Q").await;
    subscribe(&h, &a, &browser(&base, "p1"), json!({"$token": link_token(&h, &a, p)})).await;
    subscribe(&h, &a, &browser(&base, "busy-q"), json!({"$token": link_token(&h, &a, q)})).await;

    // Queued: the campaign is sending, the person turns push off before their batch runs.
    let cid = push_campaign(&mut h, &l, json!({"title": "One"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    // The audience is fixed (their message queued), then they opt out before it's handed over.
    dumbmail::campaigns::prepare_send(&h.state, cid.parse().unwrap()).await.unwrap();
    assert_eq!(count(&h, &format!("SELECT count(*) FROM messages WHERE profile_id = '{p}' AND status = 'queued'")).await, 1);
    let r = h.ok("PUT", &format!("/api/push/profiles/{p}/opt-out"), Some(json!({"opted_out": true}))).await;
    assert_eq!(r["can_receive"], false);
    h.drain().await;
    assert!(hits(&m, "/webpush/p1").is_empty());
    let (st, err): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM messages WHERE profile_id = $1").bind(p).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((st.as_str(), err.as_deref()), ("skipped", Some("push turned off")));

    // Waiting: Q's push service was busy; Q opts out before the retry runs.
    assert_eq!(hits(&m, "/webpush/busy-q").len(), 1);
    h.ok("PUT", &format!("/api/push/profiles/{q}/opt-out"), Some(json!({"opted_out": true}))).await;
    h.fast_forward().await;
    assert_eq!(hits(&m, "/webpush/busy-q").len(), 1, "the retry checks the opt-out right before the hand-off");
    let (st,): (String,) = sqlx::query_as("SELECT d.status FROM push_deliveries d JOIN messages m ON m.id = d.message_id WHERE m.profile_id = $1").bind(q).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "skipped");
    let (st,): (String,) = sqlx::query_as("SELECT status FROM messages WHERE profile_id = $1").bind(q).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "failed", "nothing went out");

    // Turned back on, they can get push again.
    h.ok("PUT", &format!("/api/push/profiles/{p}/opt-out"), Some(json!({"opted_out": false}))).await;
    let r = h.ok("GET", &format!("/api/push/profiles/{p}"), None).await;
    assert_eq!(r["can_receive"], true);
    h.cleanup().await;
}

fn apns_key() -> String {
    use p256::pkcs8::EncodePrivateKey;
    p256::SecretKey::from_slice(&[7u8; 32]).unwrap().to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string()
}

fn service_account() -> String {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
    json!({"type": "service_account", "project_id": "acme-app", "private_key_id": "kid123", "private_key": key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string(),
        "client_email": "push@acme-app.iam.gserviceaccount.com", "token_uri": "https://oauth2.googleapis.com/token"})
    .to_string()
}

#[tokio::test]
async fn mobile_push_through_apns_and_fcm() {
    let (mut h, m, _base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let (s, v) = h.req("PATCH", "/api/push/settings", Some(json!({"apns": {"key_id": "ABC123DEFG", "team_id": "TEAM123456", "bundle_id": "com.acme.app", "key_p8": "not a key"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, _) = h.req("PATCH", "/api/push/settings", Some(json!({"fcm": {"service_account_json": "{\"type\":\"service_account\",\"token_uri\":\"https://evil.test\"}"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let p8 = apns_key();
    let sa = service_account();
    let s = h.ok("PATCH", "/api/push/settings", Some(json!({"apns": {"key_id": "ABC123DEFG", "team_id": "TEAM123456", "bundle_id": "com.acme.app", "production": false, "key_p8": p8}, "fcm": {"service_account_json": sa}}))).await;
    assert_eq!((s["apns"]["configured"].as_bool(), s["fcm"]["configured"].as_bool()), (Some(true), Some(true)), "{s}");
    let shown = s.to_string();
    assert!(!shown.contains("PRIVATE KEY") && !shown.contains("BEGIN"), "secrets are never returned");
    let stored: (String, String) = sqlx::query_as("SELECT apns_key_enc, fcm_credentials_enc FROM push_settings").fetch_one(&h.state.db).await.unwrap();
    assert!(!stored.0.contains("PRIVATE") && !stored.1.contains("private_key"), "stored encrypted");
    assert_eq!(count(&h, "SELECT count(*) FROM audit_log WHERE action = 'push.settings_update'").await, 2);

    let l = list(&mut h, "App users").await;
    let ios = person(&mut h, &l, "ios@x.test", "Ivy").await;
    let droid = person(&mut h, &l, "droid@x.test", "Dan").await;
    let ios_token = "a".repeat(64);
    let (st, v) = public_post(&h, "/client/push/devices", &a.key, json!({"platform": "ios", "token": ios_token, "identity": {"$token": link_token(&h, &a, ios)}})).await;
    assert_eq!((st, v["linked"].as_bool()), (StatusCode::OK, Some(true)), "{v}");
    public_post(&h, "/client/push/devices", &a.key, json!({"platform": "ios", "token": format!("dead{}", "b".repeat(60)), "identity": {"$token": link_token(&h, &a, ios)}})).await;
    let fcm_token = "fcm-token-for-dan:APA91b".to_string() + &"x".repeat(40);
    public_post(&h, "/client/push/devices", &a.key, json!({"platform": "android", "token": fcm_token, "identity": {"$token": link_token(&h, &a, droid)}})).await;
    let (st, _) = public_post(&h, "/client/push/devices", &a.key, json!({"platform": "ios", "token": "not hex!"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let cid = push_campaign(&mut h, &l, json!({"title": "Hey {{ first_name }}", "body": "Your order shipped", "ios_badge": 1, "android_channel_id": "orders"})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;

    let apns = hits(&m, "/apns/");
    assert_eq!(apns.len(), 2);
    let ok = apns.iter().find(|x| x.path.ends_with(&ios_token)).unwrap();
    assert_eq!(ok.version, Version::HTTP_2, "APNs is HTTP/2");
    let hv = |x: &Hit, k: &str| x.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert_eq!((hv(ok, "apns-topic"), hv(ok, "apns-push-type"), hv(ok, "apns-priority")), ("com.acme.app".into(), "alert".into(), "10".into()));
    let jwt = hv(ok, "authorization").strip_prefix("bearer ").unwrap().to_string();
    let header: Value = serde_json::from_slice(&crypto::unb64(jwt.split('.').next().unwrap()).unwrap()).unwrap();
    assert_eq!((header["alg"].as_str(), header["kid"].as_str()), (Some("ES256"), Some("ABC123DEFG")));
    let body: Value = serde_json::from_slice(&ok.body).unwrap();
    assert_eq!(body["aps"]["alert"]["title"], "Hey Ivy");
    assert_eq!(body["aps"]["badge"], 1);
    let tok = body["dumbmail"]["t"].as_str().unwrap().to_string();
    // The unregistered iOS token is removed; the good one stays.
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_devices WHERE profile_id = '{ios}'")).await, 1);

    // FCM: one OAuth exchange, cached, then the v1 send with the bearer token.
    assert_eq!(hits(&m, "/oauth2/token").len(), 1);
    let fcm = hits(&m, "/fcm/acme-app");
    assert_eq!(fcm.len(), 1);
    let msg: Value = serde_json::from_slice(&fcm[0].body).unwrap();
    assert_eq!(msg["message"]["token"].as_str(), Some(fcm_token.as_str()));
    assert_eq!(msg["message"]["notification"]["title"], "Hey Dan");
    assert_eq!(msg["message"]["android"]["notification"]["channel_id"], "orders");
    assert!(msg["message"]["data"]["dumbmail_t"].is_string());

    // The app reports the open.
    let (st, _) = public_post(&h, "/client/push/opened", &a.key, json!({"t": tok})).await;
    assert_eq!(st, StatusCode::OK);
    let report = h.ok("GET", &format!("/api/push/report?campaign_id={cid}"), None).await;
    let ios_row = report["platforms"].as_array().unwrap().iter().find(|p| p["platform"] == "ios").unwrap().clone();
    assert_eq!((ios_row["sent"].as_i64(), ios_row["clicked"].as_i64(), ios_row["removed"].as_i64()), (Some(1), Some(1), Some(1)), "{report}");
    assert!(ios_row["delivered"].is_null(), "Apple doesn't report delivery");

    // A second send reuses the cached Google token; an unregistered FCM token is removed.
    public_post(&h, "/client/push/devices", &a.key, json!({"platform": "android", "token": format!("dead{}", "y".repeat(40)), "identity": {"$token": link_token(&h, &a, droid)}})).await;
    let cid2 = push_campaign(&mut h, &l, json!({"title": "Again", "platforms": ["android"]})).await;
    h.ok("POST", &format!("/api/campaigns/{cid2}/send"), None).await;
    h.drain().await;
    assert_eq!(hits(&m, "/oauth2/token").len(), 1, "token cached");
    assert_eq!(hits(&m, "/fcm/acme-app").len(), 3);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM push_devices WHERE profile_id = '{droid}'")).await, 1);
    assert_eq!(hits(&m, "/apns/").len(), 2, "platforms: android only");
    h.cleanup().await;
}

#[tokio::test]
async fn flows_tests_limits_and_isolation() {
    let (mut h, m, base) = push_harness!(0.0);
    let a = setup(&mut h).await;
    let l = list(&mut h, "Welcome").await;

    // Content checks at save time.
    let (s, v) = h.req("POST", "/api/campaigns", Some(json!({"channel": "push", "content": {"title": "Code {% coupon_code 'WELCOME' %}"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, _) = h.req("POST", "/api/campaigns", Some(json!({"channel": "push", "content": {"title": "x", "url": "javascript:alert(1)"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A flow push step reaches a new list member's device.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Welcome push", "trigger": {"type": "list", "list_id": l}, "definition": {"entry": "p", "nodes": [
        {"id": "p", "type": "push", "content": {"title": "Welcome, {{ first_name }}"}, "next": null}]}}))).await;
    let fid = flow["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    let other = h.ok("POST", "/api/profiles", Some(json!({"email": "w@x.test", "first_name": "Wen"}))).await;
    let wid: Uuid = other["id"].as_str().unwrap().parse().unwrap();
    let wb = browser(&base, "w1");
    subscribe(&h, &a, &wb, json!({"$token": link_token(&h, &a, wid)})).await;
    h.ok("POST", &format!("/api/lists/{l}/members"), Some(json!({"profile_ids": [wid]}))).await;
    h.drain().await;
    let w = hits(&m, "/webpush/w1");
    assert_eq!(w.len(), 1);
    assert_eq!(opened(&w[0], &wb)["t"], "Welcome, Wen");
    let fr = h.ok("GET", &format!("/api/push/report?flow_id={fid}"), None).await;
    assert_eq!(fr["people"]["sent"], 1, "{fr}");

    // Tests: to this person by email (a real send, for people who can send campaigns).
    let cid = push_campaign(&mut h, &l, json!({"title": "Test me"})).await;
    let r = h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["w@x.test"]}))).await;
    assert_eq!(r["sent"], 1);
    assert_eq!(opened(&hits(&m, "/webpush/w1")[1], &wb)["t"], "[Test] Test me");
    // Send me a test: the dashboard's own browser, which never joins an audience.
    let mine = browser(&base, "me1");
    let d = h.ok("POST", "/api/push/test-devices", Some(json!({"subscription": {"endpoint": mine.endpoint, "keys": {"p256dh": mine.p256dh, "auth": mine.auth}}}))).await;
    assert!(d["device_id"].is_string());
    let r = h.ok("POST", "/api/push/test", Some(json!({}))).await;
    assert_eq!(r["devices"][0]["status"], "sent", "{r}");
    assert_eq!(hits(&m, "/webpush/me1").len(), 1);
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l}]}}))).await;
    assert_eq!(est["count"], 1, "test devices aren't in audiences");
    // A browser push service only: no private or made-up addresses.
    let (s, _) = subscribe(&h, &a, &Browser { endpoint: "http://169.254.169.254/latest".into(), ..browser(&base, "x") }, json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Registrations are capped per client address per hour.
    let mut last = StatusCode::OK;
    for i in 0..40 {
        last = subscribe(&h, &a, &browser(&base, &format!("cap{i}")), json!({})).await.0;
        if last != StatusCode::OK {
            break;
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    let regs: i32 = sqlx::query_scalar("SELECT max(n) FROM push_registration_counts").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(regs, 30);

    // Another account can't see or test this account's people and devices.
    let device: Uuid = sqlx::query_scalar("SELECT id FROM push_devices WHERE profile_id = $1").bind(wid).fetch_one(&h.state.db).await.unwrap();
    let mut h2 = Harness { cookie: None, ..h };
    h2.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let (s, _) = h2.req("GET", &format!("/api/push/profiles/{wid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h2.req("POST", "/api/push/test", Some(json!({"device_id": device}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h2.req("GET", &format!("/api/push/report?campaign_id={cid}"), None).await;
    assert!(s.is_success());
    assert_eq!(h2.ok("GET", &format!("/api/push/report?campaign_id={cid}"), None).await["people"]["sent"], 0);
    h2.cleanup().await;
}

#[tokio::test]
async fn mcp_tools_need_confirm_for_real_people() {
    let (mut h, _m, _base) = push_harness!(0.0);
    let _a = setup(&mut h).await;
    let token = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let call = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let auth = format!("Bearer {token}");
    let (s, _, body) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &call("send_test_push", json!({"to": "someone@x.test"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("dry_run"), "{body}");
    let (_, _, body) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &call("get_push_settings", json!({}))).await;
    assert!(body.contains("vapid_public_key") && !body.contains("vapid_private"), "{body}");
    h.cleanup().await;
}

#[tokio::test]
async fn public_endpoints_refuse_unknown_keys_and_setup_needs_settings_permission() {
    let mut h = harness!();
    let (s, _, _) = h.raw("POST", "/client/push/devices?k=nope", &[("content-type", "application/json")], &json!({"platform": "ios", "token": "a".repeat(64)}).to_string()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = h.req("GET", "/api/push/settings", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.signup().await;
    // Push isn't set up: a push campaign can't launch, and says why.
    let l = list(&mut h, "L").await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "push", "audience": {"include": [{"type": "list", "id": l}]}, "content": {"title": "Hi"}}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("Settings → Push"), "{v}");
    h.cleanup().await;
}

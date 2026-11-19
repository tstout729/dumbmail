//! RCS for texts, against a local stand-in for Telnyx's RCS API (agent lookup, capability checks,
//! sending) and signed webhooks, built from Telnyx's published documentation. Nothing here
//! reaches Telnyx. Each test names the rule it holds.

#[macro_use]
mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Json;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

const KEY: &str = "KEYtelnyxplatform0123";
const AGENT: &str = "acme_agent_1";
const PROFILE: &str = "4001781e-626f-4a41-a914-b1b682150f94";
const OURS: &str = "+18885550100";
const PRICE_RCS: f64 = 0.02;

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[derive(Default)]
struct MockState {
    /// The agent's webhook URL as Telnyx would show it.
    webhook_url: String,
    agent_enabled: bool,
    /// Numbers that can get RCS, with their features. Others answer `features: null`.
    capable: HashMap<String, Vec<String>>,
    lookups: Vec<String>,
    bulk_lookups: Vec<Vec<String>>,
    /// Every send: (idempotency key, body).
    sends: Vec<(String, Value)>,
    /// Answers to give the next sends to a number, in order.
    answers: HashMap<String, Vec<(u16, Value)>>,
    bad_auth: usize,
}

type Mock = Arc<Mutex<MockState>>;

fn authorized(m: &Mock, headers: &HeaderMap) -> bool {
    let ok = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {KEY}"));
    if !ok {
        m.lock().unwrap().bad_auth += 1;
    }
    ok
}

fn unauthorized() -> (StatusCode, Json<Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({"errors": [{"code": "10009", "title": "Authentication failed"}]})))
}

fn caps(m: &MockState, phone: &str) -> Value {
    match m.capable.get(phone) {
        Some(f) => json!({"record_type": "rcs.capabilities", "phone_number": phone, "agent_id": AGENT, "agent_name": "Acme", "status": "Success", "features": f}),
        None => json!({"record_type": "rcs.capabilities", "phone_number": phone, "agent_id": AGENT, "agent_name": "Acme", "status": "RCS is disabled or agent is not provisioned for the carrier", "features": null}),
    }
}

async fn mock() -> (Mock, String) {
    let m: Mock = Arc::new(Mutex::new(MockState { agent_enabled: true, ..Default::default() }));
    let app = axum::Router::new()
        .route(
            "/v2/messaging/rcs/agents/{id}",
            get(|State(m): State<Mock>, headers: HeaderMap, Path(id): Path<String>| async move {
                if !authorized(&m, &headers) {
                    return unauthorized();
                }
                if id != AGENT {
                    return (StatusCode::NOT_FOUND, Json(json!({"errors": [{"code": "10005", "title": "Resource not found"}]})));
                }
                let s = m.lock().unwrap();
                (StatusCode::OK, Json(json!({"data": {"agent_id": AGENT, "user_id": "u1", "profile_id": PROFILE, "webhook_url": s.webhook_url, "webhook_failover_url": null,
                    "agent_name": "Acme Goods", "enabled": s.agent_enabled, "created_at": "2026-01-01T12:00:00.000Z", "updated_at": "2026-01-01T12:00:00.000Z"}})))
            }),
        )
        .route(
            "/v2/messaging/rcs/capabilities/{agent}/{phone}",
            get(|State(m): State<Mock>, headers: HeaderMap, Path((agent, phone)): Path<(String, String)>| async move {
                if !authorized(&m, &headers) {
                    return unauthorized();
                }
                assert_eq!(agent, AGENT);
                let mut s = m.lock().unwrap();
                s.lookups.push(phone.clone());
                (StatusCode::OK, Json(json!({"data": caps(&s, &phone)})))
            }),
        )
        .route(
            "/v2/messaging/rcs/bulk_capabilities",
            post(|State(m): State<Mock>, headers: HeaderMap, Json(b): Json<Value>| async move {
                if !authorized(&m, &headers) {
                    return unauthorized();
                }
                let phones: Vec<String> = b["phone_numbers"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
                assert!(phones.len() <= 100);
                let mut s = m.lock().unwrap();
                s.bulk_lookups.push(phones.clone());
                let data: Vec<Value> = phones.iter().map(|p| caps(&s, p)).collect();
                (StatusCode::OK, Json(json!({"data": data})))
            }),
        )
        .route(
            "/v2/messages/rcs",
            post(|State(m): State<Mock>, headers: HeaderMap, Json(b): Json<Value>| async move {
                if !authorized(&m, &headers) {
                    return unauthorized();
                }
                let key = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                let to = b["to"].as_str().unwrap_or_default().to_string();
                let mut s = m.lock().unwrap();
                s.sends.push((key, b.clone()));
                let n = s.sends.len();
                if let Some(q) = s.answers.get_mut(&to)
                    && !q.is_empty()
                {
                    let (code, body) = q.remove(0);
                    return (StatusCode::from_u16(code).unwrap(), Json(body));
                }
                (StatusCode::OK, Json(json!({"data": {"record_type": "message", "direction": "outbound", "id": format!("rcs-{n}"), "type": "RCS",
                    "messaging_profile_id": b["messaging_profile_id"], "from": {"agent_id": AGENT, "carrier": "Telnyx", "agent_name": "Acme Goods"},
                    "to": [{"phone_number": to, "status": "queued", "carrier": "T-Mobile", "line_type": "Wireless"}], "body": b["agent_message"], "encoding": "utf-8", "wait_seconds": 0.0}})))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, url)
}

/// A harness on the platform's Telnyx (texts through the log provider, RCS to the stand-in).
macro_rules! rcs_harness {
    ($url:expr) => {
        match common::Harness::with(|c| {
            c.telnyx_api_key = Some(KEY.into());
            c.telnyx_public_key = Some(b64(&signing_key().verifying_key().to_bytes()));
            c.rcs.api_base = Some($url.clone());
            c.rcs.price_rcs = Some(PRICE_RCS);
        })
        .await
        {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// Sign up with a texting number and credit (numbers aren't claimed through Telnyx here).
async fn setup(h: &mut Harness) -> Uuid {
    h.signup().await;
    h.credit(5.0).await;
    let id = account_id(h).await;
    sqlx::query("UPDATE accounts SET sms_from = $2 WHERE id = $1").bind(id).bind(OURS).execute(&h.state.db).await.unwrap();
    id
}

/// Save the agent, point it at this account, Check it and turn RCS on.
async fn rcs_on(h: &mut Harness, m: &Mock) {
    let s = h.ok("PATCH", "/api/rcs/settings", Some(json!({"agent_id": AGENT}))).await;
    m.lock().unwrap().webhook_url = s["webhook_url"].as_str().unwrap().to_string();
    let c = h.ok("POST", "/api/rcs/settings/check", None).await;
    assert_eq!(c["confirmed"], true, "{c}");
    let on = h.ok("PATCH", "/api/rcs/settings", Some(json!({"enabled": true}))).await;
    assert_eq!(on["sending"], true, "{on}");
}

fn capable(m: &Mock, phone: &str, features: &[&str]) {
    m.lock().unwrap().capable.insert(phone.to_string(), features.iter().map(|f| f.to_string()).collect());
}

const FULL: &[&str] = &["RICHCARD_STANDALONE", "RICHCARD_CAROUSEL", "ACTION_OPEN_URL", "ACTION_DIAL"];

/// A number in a zone where it's the middle of the day now (texts aren't held for quiet hours).
fn daytime_number(n: u32) -> String {
    zone_number(n, |h| (11..17).contains(&h))
}

fn nighttime_number(n: u32) -> String {
    zone_number(n, |h| !(7..22).contains(&h))
}

fn zone_number(n: u32, ok: impl Fn(u32) -> bool) -> String {
    let candidates = [
        format!("+1808555{:04}", n), format!("+1503555{:04}", n), format!("+1303555{:04}", n), format!("+1312555{:04}", n), format!("+1212555{:04}", n),
        format!("+4420794{:05}", n), format!("+9198123{:05}", n), format!("+97798123{:04}", n), format!("+8190123{:05}", n),
    ];
    candidates
        .into_iter()
        .find(|c| dumbmail::features::sendtime::zone::phone_zone(c).is_some_and(|tz| ok(chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&tz)))))
        .expect("some zone fits")
}

async fn list_with(h: &mut Harness, phones: &[&str]) -> String {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": format!("L{}", Uuid::new_v4().simple())}))).await;
    let id = list["id"].as_str().unwrap().to_string();
    for p in phones {
        h.ok("POST", "/api/profiles", Some(json!({"phone_number": p, "sms_consent": "subscribed", "list_ids": [id]}))).await;
    }
    id
}

async fn send_campaign(h: &mut Harness, list: &str, content: Value) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "sms", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    cid
}

fn texts_to(h: &Harness, phone: &str) -> Vec<String> {
    h.state.sms.sent().into_iter().filter(|s| s.to == phone).map(|s| s.body).collect()
}

fn rcs_to(m: &Mock, phone: &str) -> Vec<Value> {
    m.lock().unwrap().sends.iter().filter(|(_, b)| b["to"] == phone).map(|(_, b)| b.clone()).collect()
}

async fn balance(h: &Harness) -> i64 {
    sqlx::query_scalar("SELECT balance_micros FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn usage(h: &Harness, kind: &str) -> (i64, i64) {
    sqlx::query_as("SELECT COALESCE(sum(quantity), 0)::bigint, COALESCE(sum(-amount_micros), 0)::bigint FROM ledger WHERE kind = $1 AND day IS NOT NULL").bind(kind).fetch_one(&h.state.db).await.unwrap()
}

async fn message_to(h: &Harness, phone: &str) -> (Uuid, String, Option<String>, Option<i64>) {
    sqlx::query_as("SELECT id, status, provider_message_id, cost_micros FROM messages WHERE to_address = $1 AND channel = 'sms' ORDER BY created_at DESC LIMIT 1")
        .bind(phone)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn token(h: &Harness) -> String {
    sqlx::query_scalar("SELECT webhook_token FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn hook(h: &Harness, token: &str, body: &Value, sign: bool) -> StatusCode {
    let body = body.to_string();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = b64(&signing_key().sign(format!("{ts}|{body}").as_bytes()).to_bytes());
    let mut headers = vec![("content-type", "application/json")];
    if sign {
        headers.push(("telnyx-timestamp", ts.as_str()));
        headers.push(("telnyx-signature-ed25519", sig.as_str()));
    }
    h.raw("POST", &format!("/webhooks/rcs/{token}"), &headers, &body).await.0
}

/// A `message.finalized` (or other outbound event) as documented.
fn status_event(kind: &str, rcs_id: &str, to: &str, status: &str) -> Value {
    json!({"data": {"event_type": kind, "id": Uuid::new_v4().to_string(), "occurred_at": chrono::Utc::now().to_rfc3339(), "record_type": "event",
        "payload": {"id": rcs_id, "type": "RCS", "direction": "outbound", "from": {"agent_id": AGENT, "agent_name": "Acme Goods"},
            "to": [{"phone_number": to, "status": status, "carrier": "T-Mobile", "line_type": "Wireless"}]}}, "meta": {"attempt": 1}})
}

/// A `message.received` as documented: `body.text`, or `body.suggestion_response` for a tap.
fn inbound_event(id: &str, from: &str, agent: &str, body: Value) -> Value {
    json!({"data": {"event_type": "message.received", "id": Uuid::new_v4().to_string(), "occurred_at": chrono::Utc::now().to_rfc3339(), "record_type": "event",
        "payload": {"id": id, "type": "RCS", "direction": "inbound", "received_at": chrono::Utc::now().to_rfc3339(), "body": body,
            "from": {"phone_number": from, "carrier": "T-Mobile", "line_type": "Wireless", "status": "webhook_delivered"},
            "to": [{"agent_id": agent, "agent_name": "Acme Goods"}]}}})
}

fn lookups_made(m: &Mock) -> usize {
    let s = m.lock().unwrap();
    s.lookups.len() + s.bulk_lookups.len()
}

fn rcs_id(m: &Mock, phone: &str) -> String {
    let sends = m.lock().unwrap().sends.iter().filter(|(_, b)| b["to"] == phone).count();
    let all = m.lock().unwrap().sends.iter().position(|(_, b)| b["to"] == phone).map(|i| i + 1).unwrap();
    assert_eq!(sends, 1);
    format!("rcs-{all}")
}

#[tokio::test]
async fn nothing_changes_while_rcs_is_off() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    let a = daytime_number(1);
    capable(&m, &a, FULL);
    let list = list_with(&mut h, &[&a]).await;
    // Off with no agent, and off with a confirmed agent switched off: the same SMS, no Telnyx RCS call.
    send_campaign(&mut h, &list, json!({"body": "Spring sale today!", "rcs": {"suggestions": [{"type": "reply", "text": "Yes"}]}})).await;
    let s = h.ok("PATCH", "/api/rcs/settings", Some(json!({"agent_id": AGENT}))).await;
    m.lock().unwrap().webhook_url = s["webhook_url"].as_str().unwrap().to_string();
    h.ok("POST", "/api/rcs/settings/check", None).await;
    send_campaign(&mut h, &list, json!({"body": "Spring sale today!"})).await;
    let texts = texts_to(&h, &a);
    assert_eq!(texts, vec!["Acme Goods: Spring sale today! Reply STOP to opt out.".to_string(); 2]);
    {
        let s = m.lock().unwrap();
        assert!(s.sends.is_empty() && s.lookups.is_empty() && s.bulk_lookups.is_empty(), "no RCS work while off");
    }
    assert_eq!(usage(&h, "sms").await, (2, 2 * 9900));
    assert_eq!(usage(&h, "rcs").await, (0, 0));
    h.cleanup().await;
}

#[tokio::test]
async fn rcs_phones_get_rcs_billed_once_and_others_get_todays_sms() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let a = daytime_number(2);
    let b = daytime_number(3);
    capable(&m, &a, &["GENERIC_RCS_FEATURE"]);
    let before = balance(&h).await;
    let list = list_with(&mut h, &[&a, &b]).await;
    let body = "Spring sale today! 20% off everything.";
    send_campaign(&mut h, &list, json!({"body": body})).await;

    // B can't get RCS: exactly today's text, billed per segment as today.
    assert_eq!(texts_to(&h, &b), vec![format!("Acme Goods: {body} Reply STOP to opt out.")]);
    assert!(rcs_to(&m, &b).is_empty());
    let (_, st, pid, cost) = message_to(&h, &b).await;
    assert_eq!((st.as_str(), cost), ("sent", Some(9900)));
    assert!(!pid.unwrap().starts_with("rcs:"));

    // A gets the same text as RCS from the agent, not as SMS, billed once at the RCS price.
    assert!(texts_to(&h, &a).is_empty(), "never both");
    let sent = rcs_to(&m, &a);
    assert_eq!(sent.len(), 1);
    let (mid, st, pid, cost) = message_to(&h, &a).await;
    assert_eq!(sent[0]["agent_id"], AGENT);
    assert_eq!(sent[0]["messaging_profile_id"], PROFILE);
    assert_eq!(sent[0]["type"], "RCS");
    assert_eq!(sent[0]["agent_message"]["content_message"]["text"], format!("Acme Goods: {body} Reply STOP to opt out."));
    assert!(sent[0]["webhook_url"].as_str().unwrap().ends_with(&format!("/webhooks/rcs/{}", token(&h).await)));
    assert_eq!(m.lock().unwrap().sends[0].0, mid.simple().to_string(), "keyed by the message id");
    assert_eq!((st.as_str(), cost), ("sent", Some(20_000)));
    assert_eq!(pid.as_deref(), Some(format!("rcs:{}", rcs_id(&m, &a)).as_str()));
    assert_eq!(usage(&h, "rcs").await, (1, 20_000));
    assert_eq!(usage(&h, "sms").await, (1, 9900));
    assert_eq!(before - balance(&h).await, 20_000 + 9900);
    // The text is in the inbox thread like any text, and "Received SMS" says it went as RCS.
    let rcs_flag: Option<bool> = sqlx::query_scalar("SELECT (e.properties->>'rcs')::boolean FROM events e JOIN metrics x ON x.id = e.metric_id WHERE x.name = 'Received SMS' AND e.message_id = $1")
        .bind(mid)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(rcs_flag, Some(true));

    // A capability answer is kept: the next text to A doesn't ask again.
    let lookups = lookups_made(&m);
    send_campaign(&mut h, &list, json!({"body": "Last day!"})).await;
    assert_eq!(lookups_made(&m), lookups);
    assert_eq!(rcs_to(&m, &a).len(), 2);

    // Delivered and read: forward only, one "Read SMS".
    let tok = token(&h).await;
    let first = format!("rcs-{}", m.lock().unwrap().sends.iter().position(|(_, b)| b["to"] == a.as_str()).unwrap() + 1);
    assert_eq!(hook(&h, &tok, &status_event("message.finalized", &first, &a, "delivered"), true).await, StatusCode::OK);
    for _ in 0..2 {
        assert_eq!(hook(&h, &tok, &status_event("message.read", &first, &a, "read"), true).await, StatusCode::OK);
    }
    assert_eq!(hook(&h, &tok, &status_event("message.finalized", &first, &a, "delivery_failed"), true).await, StatusCode::OK);
    h.drain().await;
    let st: String = sqlx::query_scalar("SELECT status FROM messages WHERE id = $1").bind(mid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "delivered");
    let reads: i64 = sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics x ON x.id = e.metric_id WHERE x.name = 'Read SMS' AND e.message_id = $1").bind(mid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(reads, 1);
    assert!(texts_to(&h, &a).is_empty(), "a failure after delivery never falls back");

    // The report splits RCS and SMS.
    let cid: String = sqlx::query_scalar("SELECT campaign_id::text FROM messages WHERE id = $1").bind(mid).fetch_one(&h.state.db).await.unwrap();
    let r = h.ok("GET", &format!("/api/rcs/report?campaign_id={cid}"), None).await;
    assert_eq!(r["rcs"], json!({"sent": 1, "delivered": 1, "read": 1, "clicked": 0}), "{r}");
    assert_eq!(r["sms"], json!({"sent": 1, "delivered": 0, "clicked": 0}), "{r}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_failed_rcs_falls_back_to_sms_once_and_is_billed_once() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let a = daytime_number(4);
    let c = daytime_number(5);
    capable(&m, &a, FULL);
    capable(&m, &c, FULL);
    let before = balance(&h).await;
    let list = list_with(&mut h, &[&a, &c]).await;
    send_campaign(&mut h, &list, json!({"body": "Sale https://shop.test/sale", "rcs": {"card": {"image_url": "https://shop.test/a.jpg", "title": "Sale"}}})).await;
    let tok = token(&h).await;
    let id_a = rcs_id(&m, &a);
    let id_c = rcs_id(&m, &c);
    let claimed = rcs_to(&m, &a)[0]["agent_message"]["content_message"]["rich_card"]["standalone_card"]["card_content"]["description"].as_str().unwrap().to_string();
    assert!(claimed.contains("/s/") || claimed.contains("/t/c/"), "the link is tracked: {claimed}");

    // Telnyx says delivery failed, twice (it retries): one SMS, the same text, from our number.
    for _ in 0..2 {
        assert_eq!(hook(&h, &tok, &status_event("message.finalized", &id_a, &a, "delivery_failed"), true).await, StatusCode::OK);
        h.drain().await;
    }
    let sms: Vec<_> = h.state.sms.sent().into_iter().filter(|s| s.to == a).collect();
    assert_eq!(sms.len(), 1, "fell back once");
    assert_eq!(sms[0].body, claimed, "the claimed text");
    assert_eq!(sms[0].from, OURS);
    let (_, st, pid, cost) = message_to(&h, &a).await;
    assert_eq!(st, "sent");
    assert!(!pid.unwrap().starts_with("rcs:"), "now the SMS's id");
    let segments = dumbmail::providers::sms::segment_count(&claimed).1 as i64;
    assert_eq!(cost, Some(9900 * segments));
    // A late "delivered" for the RCS changes nothing now.
    assert_eq!(hook(&h, &tok, &status_event("message.finalized", &id_a, &a, "delivered"), true).await, StatusCode::OK);
    h.drain().await;
    assert_eq!(h.state.sms.sent().iter().filter(|s| s.to == a).count(), 1);

    // C: delivery_unconfirmed may have arrived: nothing is resent.
    assert_eq!(hook(&h, &tok, &status_event("message.finalized", &id_c, &c, "delivery_unconfirmed"), true).await, StatusCode::OK);
    h.fast_forward().await;
    assert!(texts_to(&h, &c).is_empty());

    // Billed once each: A as SMS (its RCS charge refunded), C as RCS.
    assert_eq!(before - balance(&h).await, 9900 * segments + 20_000);
    let refunds: i64 = sqlx::query_scalar("SELECT count(*) FROM ledger WHERE kind = 'rcs_refund'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(refunds, 1);
    // The report counts A as SMS now.
    let cid: String = sqlx::query_scalar("SELECT campaign_id::text FROM messages WHERE to_address = $1").bind(&a).fetch_one(&h.state.db).await.unwrap();
    let r = h.ok("GET", &format!("/api/rcs/report?campaign_id={cid}"), None).await;
    assert_eq!((r["rcs"]["sent"].as_i64(), r["sms"]["sent"].as_i64()), (Some(1), Some(1)), "{r}");
    h.cleanup().await;
}

#[tokio::test]
async fn telnyx_refusals_go_as_sms_now_and_unclear_answers_are_never_resent() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let refused = daytime_number(6);
    let unclear = daytime_number(7);
    let processing = daytime_number(8);
    for p in [&refused, &unclear, &processing] {
        capable(&m, p, FULL);
    }
    {
        let mut s = m.lock().unwrap();
        s.answers.insert(refused.clone(), vec![(422, json!({"errors": [{"code": "40001", "title": "Not routable", "detail": "secret detail"}]}))]);
        s.answers.insert(unclear.clone(), vec![(503, json!({"message": "upstream"}))]);
        s.answers.insert(processing.clone(), vec![(409, json!({"errors": [{"code": "10036", "title": "Resource is being processed"}]}))]);
    }
    let before = balance(&h).await;
    let list = list_with(&mut h, &[&refused, &unclear, &processing]).await;
    send_campaign(&mut h, &list, json!({"body": "Hello"})).await;
    assert_eq!(texts_to(&h, &refused).len(), 1, "refused: nothing went out, so SMS now");
    for p in [&unclear, &processing] {
        assert!(texts_to(&h, p).is_empty(), "{p}: never sent again by either route");
        assert_eq!(rcs_to(&m, p).len(), 1);
        let (_, st, _, cost) = message_to(&h, p).await;
        assert_eq!(st, "failed");
        assert_eq!(cost.unwrap_or(0), 0, "nothing charged");
    }
    let err: String = sqlx::query_scalar("SELECT error FROM messages WHERE to_address = $1").bind(&unclear).fetch_one(&h.state.db).await.unwrap();
    assert!(err.contains("not sent again"), "{err}");
    let all: Vec<Option<String>> = sqlx::query_scalar("SELECT fallback_reason FROM rcs_messages").fetch_all(&h.state.db).await.unwrap();
    assert!(!format!("{all:?}").contains("secret detail"), "Telnyx's own text isn't kept");
    assert_eq!(before - balance(&h).await, 9900, "only the SMS that went out");
    h.cleanup().await;
}

#[tokio::test]
async fn stop_over_rcs_stops_all_texts_and_a_queued_fallback() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let a = daytime_number(9);
    capable(&m, &a, FULL);
    let list = list_with(&mut h, &[&a]).await;
    send_campaign(&mut h, &list, json!({"body": "Hello"})).await;
    let tok = token(&h).await;
    let id = rcs_id(&m, &a);
    // The failure queues the fallback; STOP lands before it runs.
    assert_eq!(hook(&h, &tok, &status_event("message.finalized", &id, &a, "delivery_failed"), true).await, StatusCode::OK);
    assert_eq!(hook(&h, &tok, &inbound_event("in-1", &a, AGENT, json!({"text": "STOP"})), true).await, StatusCode::OK);
    h.drain().await;
    let opted: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_opt_outs WHERE phone_number = $1)").bind(&a).fetch_one(&h.state.db).await.unwrap();
    assert!(opted);
    let consent: String = sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE phone_number = $1").bind(&a).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "unsubscribed");
    // Only STOP's confirmation (an SMS, the opt-out answer), never the fallback marketing text.
    assert!(texts_to(&h, &a).iter().all(|t| !t.contains("Hello")), "{:?}", texts_to(&h, &a));
    let state: String = sqlx::query_scalar("SELECT state FROM rcs_messages WHERE rcs_provider_id = $1").bind(&id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(state, "fallback_skipped");
    // And no later text, RCS or SMS.
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE phone_number = $1").bind(&a).execute(&h.state.db).await.unwrap();
    send_campaign(&mut h, &list, json!({"body": "Again"})).await;
    assert_eq!(rcs_to(&m, &a).len(), 1);
    assert!(texts_to(&h, &a).iter().all(|t| !t.contains("Again")));
    // A STOP sent to an agent that isn't ours still opts out, and stores nothing else.
    let b = daytime_number(10);
    assert_eq!(hook(&h, &tok, &inbound_event("in-2", &b, "someone_else", json!({"text": "stop"})), true).await, StatusCode::OK);
    assert_eq!(hook(&h, &tok, &inbound_event("in-3", &b, "someone_else", json!({"text": "hello there"})), true).await, StatusCode::OK);
    let (opted, stored): (bool, i64) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM sms_opt_outs WHERE phone_number = $1), (SELECT count(*) FROM sms_inbound WHERE from_number = $1)")
        .bind(&b)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!((opted, stored), (true, 0));
    h.cleanup().await;
}

#[tokio::test]
async fn quiet_hours_hold_rcs_texts_and_their_fallback() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let night = nighttime_number(11);
    let day = daytime_number(12);
    capable(&m, &night, FULL);
    capable(&m, &day, FULL);
    let list = list_with(&mut h, &[&night, &day]).await;
    send_campaign(&mut h, &list, json!({"body": "Hello"})).await;
    assert!(rcs_to(&m, &night).is_empty() && texts_to(&h, &night).is_empty(), "held for their morning");
    let held: String = sqlx::query_scalar("SELECT status FROM messages WHERE to_address = $1").bind(&night).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(held, "queued");
    // A fallback due while it's night for the person waits too: their zone is now night.
    let id = rcs_id(&m, &day);
    let night_tz = ["Pacific/Honolulu", "America/Los_Angeles", "America/New_York", "Europe/London", "Asia/Kolkata", "Asia/Tokyo", "Australia/Sydney", "Pacific/Auckland"]
        .into_iter()
        .find(|z| !(7..22).contains(&chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&z.parse::<chrono_tz::Tz>().unwrap()))))
        .expect("some zone is at night");
    sqlx::query("UPDATE profiles SET properties = jsonb_build_object('timezone', $2::text) WHERE phone_number = $1").bind(&day).bind(night_tz).execute(&h.state.db).await.unwrap();
    assert_eq!(hook(&h, &token(&h).await, &status_event("message.finalized", &id, &day, "sending_failed"), true).await, StatusCode::OK);
    h.drain().await;
    assert!(texts_to(&h, &day).is_empty(), "the fallback waits for the window");
    let (state, due): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT r.state, (SELECT max(run_at) FROM jobs WHERE kind = 'rcs_fallback') FROM rcs_messages r WHERE r.rcs_provider_id = $1").bind(&id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(state, "fallback_queued");
    assert!(due.unwrap() > chrono::Utc::now());
    h.cleanup().await;
}

#[tokio::test]
async fn a_suggested_reply_tap_ends_a_wait_for_a_reply() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let a = daytime_number(13);
    capable(&m, &a, FULL);
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Asked"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Ask", "trigger": {"type": "list", "list_id": list_id}, "definition": {"entry": "t", "nodes": [
        {"id": "t", "type": "sms", "content": {"body": "Want early access?", "rcs": {"suggestions": [{"type": "reply", "text": "Yes please"}, {"type": "reply", "text": "No"}]}}, "next": "w"},
        {"id": "w", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": "y", "no": "n"},
        {"id": "y", "type": "update_profile", "properties": {"replied": true}, "next": null},
        {"id": "n", "type": "update_profile", "properties": {"replied": false}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"phone_number": a, "sms_consent": "subscribed", "list_ids": [list_id]}))).await;
    h.drain().await;
    let sent = rcs_to(&m, &a);
    assert_eq!(sent.len(), 1, "the flow text went as RCS");
    assert_eq!(sent[0]["agent_message"]["content_message"]["suggestions"][0]["reply"]["text"], "Yes please");
    let tap = inbound_event("tap-1", &a, AGENT, json!({"suggestion_response": {"postback_data": "ZHVtYm1haWw6cmVwbHk6MA==", "text": "Yes please"}}));
    assert_eq!(hook(&h, &token(&h).await, &tap, true).await, StatusCode::OK);
    h.drain().await;
    let replied: Option<bool> = sqlx::query_scalar("SELECT (properties->>'replied')::boolean FROM profiles WHERE id = $1::uuid").bind(p["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(replied, Some(true));
    // It's in the inbox as a text from them.
    let body: String = sqlx::query_scalar("SELECT body FROM sms_inbound WHERE from_number = $1").bind(&a).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(body, "Yes please");
    h.cleanup().await;
}

#[tokio::test]
async fn the_rich_version_goes_only_to_phones_that_can_show_it() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let full = daytime_number(14);
    let basic = daytime_number(15);
    capable(&m, &full, FULL);
    capable(&m, &basic, &["GENERIC_RCS_FEATURE"]);
    let list = list_with(&mut h, &[&full, &basic]).await;
    let content = json!({"body": "Spring sale", "rcs": {"card": {"image_url": "https://shop.test/spring.jpg", "title": "Hi {{ first_name|default('friend') }}"},
        "suggestions": [{"type": "url", "text": "Shop now", "url": "https://shop.test/sale"}, {"type": "reply", "text": "Not for me"}]}});
    // The preview shows the rich version; an invalid one blocks the launch.
    let p = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": content}))).await;
    assert_eq!(p["rcs"]["card"]["title"], "Hi Alex", "{p}");
    let bad = json!({"body": "x", "rcs": {"suggestions": [{"type": "url", "text": "Go", "url": "javascript:alert(1)"}]}});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Bad", "channel": "sms", "audience": {"include": [{"type": "list", "id": list}]}, "content": bad}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    send_campaign(&mut h, &list, content).await;
    let sent = rcs_to(&m, &full);
    assert_eq!(sent.len(), 1);
    let card = &sent[0]["agent_message"]["content_message"]["rich_card"]["standalone_card"]["card_content"];
    assert_eq!(card["title"], "Hi friend");
    assert_eq!(card["media"]["content_info"]["file_url"], "https://shop.test/spring.jpg");
    assert_eq!(card["description"], "Acme Goods: Spring sale Reply STOP to opt out.");
    let link = card["suggestions"][0]["action"]["open_url_action"]["url"].as_str().unwrap();
    assert!(link.starts_with("https://pl.test/t/c/") && link.contains("shop.test%2Fsale"), "tracked: {link}");
    // A phone with only basic RCS can't show a card or a link button: it gets the SMS.
    assert!(rcs_to(&m, &basic).is_empty());
    assert_eq!(texts_to(&h, &basic), vec!["Acme Goods: Spring sale Reply STOP to opt out.".to_string()]);
    // Following the tracked link counts as a click on the text.
    let path = link.trim_start_matches("https://pl.test");
    let (s, _, _) = h.raw("GET", path, &[], "").await;
    assert!(s.is_redirection(), "{s}");
    let (mid, ..) = message_to(&h, &full).await;
    let clicked: bool = sqlx::query_scalar("SELECT clicked_at IS NOT NULL FROM messages WHERE id = $1").bind(mid).fetch_one(&h.state.db).await.unwrap();
    assert!(clicked);
    h.cleanup().await;
}

#[tokio::test]
async fn webhooks_are_signed_and_scoped_to_the_account_and_agent() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let a = daytime_number(16);
    capable(&m, &a, FULL);
    let list = list_with(&mut h, &[&a]).await;
    send_campaign(&mut h, &list, json!({"body": "Hello"})).await;
    let tok = token(&h).await;
    let id = rcs_id(&m, &a);
    let failed = status_event("message.finalized", &id, &a, "delivery_failed");
    // Unsigned or badly signed: refused before anything is read.
    assert_eq!(hook(&h, &tok, &failed, false).await, StatusCode::FORBIDDEN);
    let body = failed.to_string();
    let ts = chrono::Utc::now().timestamp().to_string();
    let forged = b64(&SigningKey::from_bytes(&[7u8; 32]).sign(format!("{ts}|{body}").as_bytes()).to_bytes());
    let (s, _, _) = h.raw("POST", &format!("/webhooks/rcs/{tok}"), &[("content-type", "application/json"), ("telnyx-timestamp", &ts), ("telnyx-signature-ed25519", &forged)], &body).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(hook(&h, "not-a-token", &failed, true).await, StatusCode::NOT_FOUND);
    // Another account's address can't touch this account's messages.
    let other = Uuid::now_v7();
    sqlx::query("INSERT INTO accounts (id, name, public_key, webhook_token) VALUES ($1, 'Other', 'pk_other', 'othertoken')").bind(other).execute(&h.state.db).await.unwrap();
    assert_ne!(hook(&h, "othertoken", &failed, true).await, StatusCode::FORBIDDEN, "signed, but it finds no message there");
    // A status naming a different agent than the message's is ignored.
    let mut wrong = failed.clone();
    wrong["data"]["payload"]["from"]["agent_id"] = json!("someone_else");
    assert_eq!(hook(&h, &tok, &wrong, true).await, StatusCode::OK);
    h.drain().await;
    assert!(texts_to(&h, &a).is_empty(), "nothing fell back");
    // A failure for a message not recorded yet (racing the send) asks Telnyx to retry.
    assert_eq!(hook(&h, &tok, &status_event("message.finalized", "rcs-unknown", &a, "delivery_failed"), true).await, StatusCode::SERVICE_UNAVAILABLE);
    // Browsers can't record read receipts.
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Read SMS", "profile": {"phone_number": a}}).to_string()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // The account switches to its own Telnyx account and signs a failure with its own key: that key
    // doesn't speak for a message the platform sent (no refund, no free fallback).
    let own = SigningKey::from_bytes(&[9u8; 32]);
    let creds = json!({"provider": "telnyx", "api_key": "KEYown", "messaging_profile_id": null, "public_key": b64(&own.verifying_key().to_bytes())}).to_string();
    let enc = dumbmail::crypto::encrypt(&h.state.cfg.secret_key, &creds).unwrap();
    let acct = account_id(&h).await;
    sqlx::query("UPDATE accounts SET sms_mode = 'byo', sms_byo_provider = 'telnyx', sms_byo_credentials = $2 WHERE id = $1").bind(acct).bind(enc).execute(&h.state.db).await.unwrap();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = b64(&own.sign(format!("{ts}|{body}").as_bytes()).to_bytes());
    let (s, _, _) = h.raw("POST", &format!("/webhooks/rcs/{tok}"), &[("content-type", "application/json"), ("telnyx-timestamp", &ts), ("telnyx-signature-ed25519", &sig)], &body).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let (state, refunds): (String, i64) = sqlx::query_as("SELECT state, (SELECT count(*) FROM ledger WHERE kind = 'rcs_refund') FROM rcs_messages WHERE rcs_provider_id = $1").bind(&id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((state.as_str(), refunds), ("sent", 0));
    assert!(texts_to(&h, &a).is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn the_check_proves_the_agent_is_this_accounts_and_rcs_stays_off_until_then() {
    let (m, url) = mock().await;
    let mut h = rcs_harness!(url);
    let acct = setup(&mut h).await;
    // Can't turn on without a confirmed agent.
    let (s, _) = h.req("PATCH", "/api/rcs/settings", Some(json!({"enabled": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.req("PATCH", "/api/rcs/settings", Some(json!({"agent_id": "../etc"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let saved = h.ok("PATCH", "/api/rcs/settings", Some(json!({"agent_id": AGENT}))).await;
    // The agent points somewhere else: not confirmed, still off.
    m.lock().unwrap().webhook_url = "https://elsewhere.test/hook".into();
    let c = h.ok("POST", "/api/rcs/settings/check", None).await;
    assert_eq!(c["confirmed"], false);
    assert!(c["check_problem"].as_str().unwrap().contains(saved["webhook_url"].as_str().unwrap()), "{c}");
    assert_eq!(h.req("PATCH", "/api/rcs/settings", Some(json!({"enabled": true}))).await.0, StatusCode::BAD_REQUEST);
    // Disabled at Telnyx: not confirmed.
    m.lock().unwrap().webhook_url = saved["webhook_url"].as_str().unwrap().to_string();
    m.lock().unwrap().agent_enabled = false;
    assert_eq!(h.ok("POST", "/api/rcs/settings/check", None).await["confirmed"], false);
    m.lock().unwrap().agent_enabled = true;
    rcs_on(&mut h, &m).await;
    // A second account can't confirm the same agent (even pointing it at itself).
    let other = Uuid::now_v7();
    sqlx::query("INSERT INTO accounts (id, name, public_key, webhook_token, sms_from) VALUES ($1, 'Other', 'pk_other', 'othertoken', '+18885550199')").bind(other).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO rcs_settings (account_id, agent_id) VALUES ($1, $2)").bind(other).bind(AGENT).execute(&h.state.db).await.unwrap();
    m.lock().unwrap().webhook_url = "https://pl.test/webhooks/rcs/othertoken".into();
    let out = dumbmail::features::rcs::settings::check(&h.state, other).await.unwrap();
    assert_eq!(out["confirmed"], false);
    assert!(out["check_problem"].as_str().unwrap().contains("another DumbMail account"), "{out}");
    // A new agent id turns RCS off until it's checked.
    let changed = h.ok("PATCH", "/api/rcs/settings", Some(json!({"agent_id": "acme_agent_2"}))).await;
    assert_eq!((changed["enabled"].as_bool(), changed["sending"].as_bool()), (Some(false), Some(false)));
    let a = daytime_number(17);
    capable(&m, &a, FULL);
    let list = list_with(&mut h, &[&a]).await;
    send_campaign(&mut h, &list, json!({"body": "Hi"})).await;
    assert!(rcs_to(&m, &a).is_empty());
    assert_eq!(texts_to(&h, &a).len(), 1);
    // Every change is in the audit log, and the Check is too.
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log WHERE account_id = $1 AND action LIKE 'rcs.%' ORDER BY created_at").bind(acct).fetch_all(&h.state.db).await.unwrap();
    assert!(actions.contains(&"rcs.settings_update".to_string()) && actions.contains(&"rcs.agent_check".to_string()), "{actions:?}");
    assert_eq!(m.lock().unwrap().bad_auth, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn rcs_needs_the_signing_key_and_permissions_and_mcp_work() {
    let (m, url) = mock().await;
    // No signing key: RCS can't be switched on, and events are refused.
    let mut h = match common::Harness::with(|c| {
        c.telnyx_api_key = Some(KEY.into());
        c.rcs.api_base = Some(url.clone());
    })
    .await
    {
        Some(h) => h,
        None => return,
    };
    let acct = setup(&mut h).await;
    let s = h.ok("PATCH", "/api/rcs/settings", Some(json!({"agent_id": AGENT}))).await;
    assert_eq!(s["signing_key_ready"], false);
    m.lock().unwrap().webhook_url = s["webhook_url"].as_str().unwrap().to_string();
    assert_eq!(h.ok("POST", "/api/rcs/settings/check", None).await["confirmed"], true);
    let (st, v) = h.req("PATCH", "/api/rcs/settings", Some(json!({"enabled": true}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(hook(&h, &token(&h).await, &inbound_event("x", "+15035550001", AGENT, json!({"text": "hi"})), true).await, StatusCode::FORBIDDEN);

    // Roles: an analyst reads settings and the report but can't change or check them.
    sqlx::query("UPDATE memberships SET role = 'analyst' WHERE account_id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    h.ok("GET", "/api/rcs/settings", None).await;
    assert_eq!(h.req("PATCH", "/api/rcs/settings", Some(json!({"enabled": false}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(h.req("POST", "/api/rcs/settings/check", None).await.0, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE memberships SET role = 'owner' WHERE account_id = $1").bind(acct).execute(&h.state.db).await.unwrap();

    // MCP: the tools are listed and work like the routes.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Agent"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let rpc = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let (_, _, list) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string()).await;
    for t in ["get_rcs_settings", "update_rcs_settings", "check_rcs_agent", "get_rcs_report"] {
        assert!(list.contains(&format!("\"{t}\"")), "{t} missing");
    }
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("get_rcs_settings", json!({}))).await;
    assert!(out.contains(AGENT), "{out}");
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("update_rcs_settings", json!({"agent_id": null}))).await;
    assert!(!out.contains(AGENT), "{out}");
    let secret = format!("{:?}", h.ok("GET", "/api/rcs/settings", None).await);
    assert!(!secret.contains(KEY), "the API key is never shown");
    h.cleanup().await;
}

#[tokio::test]
async fn launch_checks_use_the_costlier_route_and_campaigns_are_looked_up_in_bulk() {
    let (m, url) = mock().await;
    let mut h = match common::Harness::with(|c| {
        c.telnyx_api_key = Some(KEY.into());
        c.telnyx_public_key = Some(b64(&signing_key().verifying_key().to_bytes()));
        c.rcs.api_base = Some(url.clone());
        c.rcs.price_rcs = Some(1.0);
    })
    .await
    {
        Some(h) => h,
        None => return,
    };
    let acct = setup(&mut h).await;
    rcs_on(&mut h, &m).await;
    let people: Vec<String> = (0..3).map(|i| daytime_number(20 + i)).collect();
    let refs: Vec<&str> = people.iter().map(String::as_str).collect();
    let list = list_with(&mut h, &refs).await;
    // $5 covers 3 texts as SMS but not 3 RCS messages at $1 each... it does; drain the balance to $2.
    sqlx::query("UPDATE accounts SET balance_micros = 2000000 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "sms", "audience": {"include": [{"type": "list", "id": list}]}, "content": {"body": "Hi"}}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "the RCS price counts: {v}");
    sqlx::query("UPDATE accounts SET balance_micros = 10000000 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    for p in &people {
        capable(&m, p, FULL);
    }
    // The campaign's numbers are looked up in one bulk request when it starts sending.
    let cid = c["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, cid.parse().unwrap()).await.unwrap();
    dumbmail::features::rcs::capability::start_prefetches(&h.state).await.unwrap();
    dumbmail::features::rcs::capability::start_prefetches(&h.state).await.unwrap();
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'rcs_prefetch'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(jobs, 1, "once per campaign");
    // (Here the lookup runs before the campaign's batches; on a server they run side by side.)
    dumbmail::features::rcs::capability::prefetch(&h.state, cid.parse().unwrap()).await.unwrap();
    h.drain().await;
    {
        let s = m.lock().unwrap();
        assert_eq!(s.bulk_lookups.len(), 1, "{:?}", s.bulk_lookups);
        assert_eq!(s.bulk_lookups[0].len(), 3);
        assert!(s.lookups.is_empty(), "every text found its answer ready");
        assert_eq!(s.sends.len(), 3);
    }
    h.cleanup().await;
}

#[tokio::test]
async fn rcs_is_unavailable_without_telnyx() {
    let mut h = harness!();
    h.signup().await;
    let s = h.ok("GET", "/api/rcs/settings", None).await;
    assert_eq!(s["available"], false, "{s}");
    h.ok("PATCH", "/api/rcs/settings", Some(json!({"agent_id": AGENT}))).await;
    assert_eq!(h.req("POST", "/api/rcs/settings/check", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(h.req("PATCH", "/api/rcs/settings", Some(json!({"enabled": true}))).await.0, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

//! WhatsApp end to end, against a local stand-in for Meta's Graph API (Cloud API sends, template
//! listing and creation, the phone number) and signed webhook deliveries, built from Meta's
//! published documentation. Nothing here reaches Meta.

#[macro_use]
mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Json;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

use common::*;

const TOKEN: &str = "EAAGtestsystemusertoken0123456789";
const SECRET: &str = "appsecret0123456789abcdef";
const WABA: &str = "1001";
const PHONE_ID: &str = "2002";
const TEST_NUMBER: &str = "+15035550199";

#[derive(Default)]
struct MockState {
    /// Every send: (recipient, body).
    sends: Vec<(String, Value)>,
    /// Answers to give the next sends to a recipient, in order: (HTTP status, body).
    answers: HashMap<String, Vec<(u16, Value)>>,
    templates: Vec<Value>,
    created: Vec<Value>,
    bad_auth: usize,
}

type Mock = Arc<Mutex<MockState>>;

fn authorized(m: &Mock, headers: &HeaderMap) -> bool {
    let ok = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"));
    if !ok {
        m.lock().unwrap().bad_auth += 1;
    }
    ok
}

fn meta_error(code: i64) -> (u16, Value) {
    (400, json!({"error": {"message": "(#x) something", "type": "OAuthException", "code": code, "error_data": {"messaging_product": "whatsapp", "details": "d"}, "fbtrace_id": "A1"}}))
}

async fn mock() -> (Mock, String) {
    let m: Mock = Default::default();
    m.lock().unwrap().templates = vec![
        json!({"id": "9001", "name": "spring_sale", "language": "en_US", "status": "APPROVED", "category": "MARKETING",
               "components": [{"type": "BODY", "text": "Hi {{1}}, 20% off everything today."}]}),
        json!({"id": "9002", "name": "order_update", "language": "en_US", "status": "APPROVED", "category": "UTILITY",
               "components": [{"type": "HEADER", "format": "TEXT", "text": "Order {{1}}"}, {"type": "BODY", "text": "Hi {{1}}, your order {{2}} has shipped."},
                              {"type": "BUTTONS", "buttons": [{"type": "URL", "text": "Track", "url": "https://shop.test/track/{{1}}"}]}]}),
        json!({"id": "9003", "name": "new_arrivals", "language": "en", "status": "PENDING", "category": "MARKETING", "parameter_format": "NAMED",
               "components": [{"type": "HEADER", "format": "IMAGE"}, {"type": "BODY", "text": "New in for you, {{first_name}}!"}]}),
    ];
    let app = axum::Router::new()
        .route(
            "/v27.0/{id}",
            get(|State(m): State<Mock>, headers: HeaderMap, Path(id): Path<String>| async move {
                if !authorized(&m, &headers) {
                    let (s, b) = meta_error(190);
                    return (StatusCode::from_u16(s).unwrap(), Json(b));
                }
                (StatusCode::OK, Json(json!({"id": id, "display_phone_number": "+1 503-555-0100", "verified_name": "Acme Goods", "quality_rating": "GREEN", "whatsapp_business_manager_messaging_limit": "TIER_250"})))
            }),
        )
        .route(
            "/v27.0/{id}/message_templates",
            get(|State(m): State<Mock>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                if !authorized(&m, &headers) {
                    return Json(meta_error(190).1);
                }
                let all = m.lock().unwrap().templates.clone();
                // Two per page, with Meta's cursors.
                let start: usize = q.get("after").and_then(|a| a.strip_prefix('c')).and_then(|n| n.parse().ok()).unwrap_or(0);
                let page: Vec<Value> = all.iter().skip(start).take(2).cloned().collect();
                let mut out = json!({"data": page, "paging": {"cursors": {"before": "b", "after": format!("c{}", start + 2)}}});
                if start + 2 < all.len() {
                    out["paging"]["next"] = json!("https://graph.facebook.com/v27.0/next");
                }
                Json(out)
            })
            .post(|State(m): State<Mock>, headers: HeaderMap, Json(b): Json<Value>| async move {
                if !authorized(&m, &headers) {
                    return Json(meta_error(190).1);
                }
                let mut s = m.lock().unwrap();
                s.created.push(b.clone());
                let id = format!("{}", 9100 + s.created.len());
                Json(json!({"id": id, "status": "PENDING", "category": b["category"]}))
            }),
        )
        .route(
            "/v27.0/{id}/messages",
            post(|State(m): State<Mock>, headers: HeaderMap, Json(b): Json<Value>| async move {
                if !authorized(&m, &headers) {
                    let (s, b) = meta_error(190);
                    return (StatusCode::from_u16(s).unwrap(), Json(b));
                }
                let to = b["to"].as_str().unwrap_or_default().to_string();
                let mut s = m.lock().unwrap();
                s.sends.push((to.clone(), b.clone()));
                let n = s.sends.len();
                if let Some(q) = s.answers.get_mut(&to)
                    && !q.is_empty()
                {
                    let (code, body) = q.remove(0);
                    return (StatusCode::from_u16(code).unwrap(), Json(body));
                }
                let wa_id: String = to.chars().filter(char::is_ascii_digit).collect();
                (StatusCode::OK, Json(json!({"messaging_product": "whatsapp", "contacts": [{"input": to, "wa_id": wa_id}], "messages": [{"id": format!("wamid.TEST{n}")}]})))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, base)
}

macro_rules! wa_harness {
    ($price:expr) => {{
        let (m, base) = mock().await;
        let h = match common::Harness::with(|c| {
            c.whatsapp.whatsapp_api_base = Some(base.clone());
            c.whatsapp.price_whatsapp = $price;
        })
        .await
        {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        };
        (h, m)
    }};
}

fn sends(m: &Mock) -> Vec<(String, Value)> {
    m.lock().unwrap().sends.clone()
}

fn answer(m: &Mock, to: &str, a: (u16, Value)) {
    m.lock().unwrap().answers.entry(to.to_string()).or_default().push(a);
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Acme Goods'").fetch_one(&h.state.db).await.unwrap()
}

/// Sign up, connect WhatsApp and sync the stand-in's templates.
async fn setup(h: &mut Harness) -> Uuid {
    h.signup().await;
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"waba_id": WABA, "phone_number_id": PHONE_ID, "access_token": TOKEN, "app_secret": SECRET}))).await;
    let t = h.ok("POST", "/api/whatsapp/templates/sync", None).await;
    assert_eq!(t["read"], 3, "{t}");
    // Connecting also queued a read of the templates: done now, not in the middle of a test.
    h.drain().await;
    account_id(h).await
}

async fn template(h: &Harness, name: &str) -> String {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM whatsapp_templates WHERE name = $1").bind(name).fetch_one(&h.state.db).await.unwrap();
    id.to_string()
}

fn sign(body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn hook_with(h: &Harness, body: &Value, signature: &str) -> StatusCode {
    let key: String = sqlx::query_scalar("SELECT webhook_key FROM whatsapp_settings s JOIN accounts a ON a.id = s.account_id WHERE a.name = 'Acme Goods'").fetch_one(&h.state.db).await.unwrap();
    h.raw("POST", &format!("/webhooks/whatsapp/{key}"), &[("x-hub-signature-256", signature), ("content-type", "application/json")], &body.to_string()).await.0
}

async fn hook(h: &Harness, body: &Value) -> StatusCode {
    let s = hook_with(h, body, &sign(&body.to_string())).await;
    assert!(s == StatusCode::OK || s == StatusCode::SERVICE_UNAVAILABLE, "webhook answered {s}");
    s
}

fn envelope(field: &str, value: Value) -> Value {
    json!({"object": "whatsapp_business_account", "entry": [{"id": WABA, "changes": [{"field": field, "value": value}]}]})
}

/// An envelope with Meta's time for the event (`entry[].time`).
fn envelope_at(field: &str, value: Value, time: i64) -> Value {
    json!({"object": "whatsapp_business_account", "entry": [{"id": WABA, "time": time, "changes": [{"field": field, "value": value}]}]})
}

/// The signed-in owner's own phone as a verified test number (the code sent from it on WhatsApp).
async fn verify_tester(h: &mut Harness, phone: &str) {
    let r = h.ok("POST", "/api/whatsapp/test-numbers", Some(json!({"phone_number": phone}))).await;
    let code = r["code"].as_str().unwrap().to_string();
    assert_eq!(hook(h, &inbound(phone, &format!("wamid.V{}", digits(phone)), &code)).await, StatusCode::OK);
    let ok: bool = sqlx::query_scalar("SELECT verified_at IS NOT NULL FROM whatsapp_test_numbers WHERE phone_number = $1").bind(phone).fetch_one(&h.state.db).await.unwrap();
    assert!(ok, "verified");
}

async fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn digits(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

fn inbound_at(from: &str, wamid: &str, text: &str, at: i64) -> Value {
    envelope("messages", json!({
        "messaging_product": "whatsapp",
        "metadata": {"display_phone_number": "15035550100", "phone_number_id": PHONE_ID},
        "contacts": [{"profile": {"name": "Sam"}, "wa_id": digits(from)}],
        "messages": [{"from": digits(from), "id": wamid, "timestamp": at.to_string(), "type": "text", "text": {"body": text}}]
    }))
}

fn inbound(from: &str, wamid: &str, text: &str) -> Value {
    inbound_at(from, wamid, text, chrono::Utc::now().timestamp())
}

fn status(wamid: &str, st: &str, extra: Value) -> Value {
    let mut s = json!({"id": wamid, "status": st, "timestamp": chrono::Utc::now().timestamp().to_string(), "recipient_id": "15035550100"});
    if let (Some(o), Some(e)) = (s.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    envelope("messages", json!({"messaging_product": "whatsapp", "metadata": {"display_phone_number": "15035550100", "phone_number_id": PHONE_ID}, "statuses": [s]}))
}

/// A number whose own time zone is in the middle of the day now (quiet hours never hold it).
fn daytime_number(n: u32) -> String {
    zone_number(n, |h| (11..17).contains(&h))
}

/// Outside texting hours there (they're never wider than 08:00-21:00).
fn night_number(n: u32) -> String {
    zone_number(n, |h| !(7..22).contains(&h))
}

fn zone_number(n: u32, hours: impl Fn(u32) -> bool) -> String {
    let candidates = [
        format!("+1808555{:04}", n), format!("+1503555{:04}", n), format!("+1303555{:04}", n), format!("+1312555{:04}", n), format!("+1212555{:04}", n),
        format!("+4420794{:05}", n), format!("+9198123{:05}", n), format!("+97798123{:04}", n), format!("+8190123{:05}", n), format!("+6129123{:04}", n),
    ];
    candidates
        .into_iter()
        .find(|c| dumbmail::features::sendtime::zone::phone_zone(c).is_some_and(|tz| hours(chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&tz)))))
        .expect("some zone is at that hour")
}

async fn person(h: &mut Harness, phone: &str, sms: Option<&str>) -> String {
    let mut body = json!({"phone_number": phone, "first_name": "Sam"});
    if let Some(s) = sms {
        body["sms_consent"] = json!(s);
    }
    h.ok("POST", "/api/profiles", Some(body)).await["id"].as_str().unwrap().to_string()
}

async fn subscribe(h: &mut Harness, profile: &str) {
    h.ok("PUT", &format!("/api/whatsapp/profiles/{profile}/consent"), Some(json!({"consent": "subscribed", "evidence": "checkout checkbox naming WhatsApp"}))).await;
}

async fn consent_of(h: &Harness, phone: &str) -> String {
    sqlx::query_scalar("SELECT consent FROM whatsapp_contacts WHERE phone_number = $1").bind(phone).fetch_optional(&h.state.db).await.unwrap().unwrap_or_else(|| "never_subscribed".into())
}

async fn messages_to(h: &Harness, phone: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT status, error FROM messages WHERE channel = 'whatsapp' AND to_address = $1 ORDER BY created_at").bind(phone).fetch_all(&h.state.db).await.unwrap()
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

/// A live flow on metric `metric` with one WhatsApp step.
async fn flow(h: &mut Harness, metric: &str, content: Value, service: bool) -> String {
    let mut def = json!({"entry": "w", "nodes": [{"id": "w", "type": "whatsapp", "content": content, "next": null}]});
    if service {
        def["settings"] = json!({"service_messages": true});
    }
    let f = h.ok("POST", "/api/flows", Some(json!({"name": metric, "trigger": {"type": "metric", "metric": metric}, "definition": def}))).await;
    let id = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
    id
}

async fn trigger(h: &mut Harness, metric: &str, phone: &str) {
    h.ok("POST", "/api/events", Some(json!({"metric": metric, "profile": {"phone_number": phone}, "properties": {"order_id": "A-100"}}))).await;
    h.drain().await;
}

fn sale(t: &str) -> Value {
    json!({"template_id": t, "body": {"1": "{{ first_name|default('there') }}"}})
}

fn order(t: &str) -> Value {
    json!({"template_id": t, "header": "{{ event.properties.order_id }}", "body": {"1": "{{ first_name }}", "2": "{{ event.properties.order_id }}"}, "buttons": {"0": "{{ event.properties.order_id }}"}})
}

#[tokio::test]
async fn connects_checks_and_never_shows_secrets() {
    let (mut h, m) = wa_harness!(0.0);
    h.signup().await;
    let s = h.ok("GET", "/api/whatsapp/settings", None).await;
    assert_eq!(s["connected"], false);
    let url = s["webhook_url"].as_str().unwrap().to_string();
    assert!(url.starts_with("https://pl.test/webhooks/whatsapp/"), "{s}");
    let verify = s["verify_token"].as_str().unwrap().to_string();
    let bad = h.req("PATCH", "/api/whatsapp/settings", Some(json!({"waba_id": "../1"}))).await;
    assert_eq!(bad.0, StatusCode::BAD_REQUEST);
    // Without the app secret opt-outs couldn't be checked, so sending stays off.
    let s = h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"waba_id": WABA, "phone_number_id": PHONE_ID, "access_token": TOKEN}))).await;
    assert_eq!((s["connected"].as_bool(), s["missing"].clone()), (Some(false), json!(["app secret"])), "{s}");
    let s = h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"app_secret": SECRET}))).await;
    let text = s.to_string();
    assert!(!text.contains(TOKEN) && !text.contains(SECRET), "{s}");
    assert_eq!((s["connected"].as_bool(), s["access_token_hint"].as_str()), (Some(true), Some("…6789")));
    let stored: String = sqlx::query_scalar("SELECT access_token_enc FROM whatsapp_settings").fetch_one(&h.state.db).await.unwrap();
    assert!(!stored.contains(TOKEN));

    let s = h.ok("POST", "/api/whatsapp/settings/check", None).await;
    assert_eq!((s["phone"]["verified_name"].as_str(), s["phone"]["quality_rating"].as_str(), s["phone"]["messaging_limit"].as_str()), (Some("Acme Goods"), Some("GREEN"), Some("TIER_250")), "{s}");

    // Meta's webhook check: only the right verify token gets the challenge back.
    let path = url.trim_start_matches("https://pl.test");
    let (st, _, body) = h.raw("GET", &format!("{path}?hub.mode=subscribe&hub.verify_token={verify}&hub.challenge=1158201444"), &[], "").await;
    assert_eq!((st, body.as_str()), (StatusCode::OK, "1158201444"));
    let (st, _, _) = h.raw("GET", &format!("{path}?hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=1"), &[], "").await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Templates sync across Meta's pages; the audit log has the change, never the token.
    let t = h.ok("POST", "/api/whatsapp/templates/sync", None).await;
    assert_eq!(t["templates"].as_array().unwrap().len(), 3, "{t}");
    let audit: Vec<Value> = sqlx::query_scalar("SELECT details FROM audit_log WHERE action = 'whatsapp.settings_update'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(audit.len(), 2);
    assert!(!audit[0].to_string().contains(TOKEN));

    // A wrong token shows as a connection problem.
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"access_token": "EAAGwrongwrongwrongwrongwrong"}))).await;
    let s = h.ok("POST", "/api/whatsapp/settings/check", None).await;
    assert!(s["error"].as_str().unwrap().contains("reconnect"), "{s}");
    assert!(m.lock().unwrap().bad_auth > 0);
    h.cleanup().await;
}

#[tokio::test]
async fn a_forged_webhook_is_refused() {
    let (mut h, _m) = wa_harness!(0.0);
    setup(&mut h).await;
    let them = daytime_number(101);
    let body = inbound(&them, "wamid.IN1", "STOP");
    assert_eq!(hook_with(&h, &body, "sha256=00").await, StatusCode::UNAUTHORIZED);
    assert_eq!(hook_with(&h, &body, "").await, StatusCode::UNAUTHORIZED);
    // Signed with another app secret.
    let mut mac = Hmac::<Sha256>::new_from_slice(b"someoneelse0123456789").unwrap();
    mac.update(body.to_string().as_bytes());
    assert_eq!(hook_with(&h, &body, &format!("sha256={}", hex::encode(mac.finalize().into_bytes()))).await, StatusCode::UNAUTHORIZED);
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_inbound").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_contacts").await, 0);
    // Another business account's entry in a genuine delivery is ignored.
    let mut other = inbound(&them, "wamid.IN2", "hello");
    other["entry"][0]["id"] = json!("777");
    assert_eq!(hook(&h, &other).await, StatusCode::OK);
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_inbound").await, 0);
    // The genuine one is stored, once, however often Meta delivers it.
    assert_eq!(hook(&h, &body).await, StatusCode::OK);
    assert_eq!(hook(&h, &body).await, StatusCode::OK);
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_inbound").await, 1);
    assert_eq!(consent_of(&h, &them).await, "unsubscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_consent_log WHERE consent = 'unsubscribed'").await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn statuses_move_forward_once() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(201);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    flow(&mut h, "Joined", sale(&t), false).await;
    trigger(&mut h, "Joined", &them).await;
    let s = sends(&m);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].1["template"]["components"][0]["parameters"][0]["text"], "Sam", "{:?}", s[0].1);
    assert_eq!(s[0].1["template"]["name"], "spring_sale");
    let wamid = "wamid.TEST1";
    assert_eq!(messages_to(&h, &them).await, vec![("sent".into(), None)]);

    assert_eq!(hook(&h, &status(wamid, "delivered", json!({"pricing": {"category": "marketing", "type": "regular"}}))).await, StatusCode::OK);
    assert_eq!(hook(&h, &status(wamid, "delivered", json!({}))).await, StatusCode::OK);
    // A late "sent" changes nothing; read moves it on; a repeated read records one event.
    assert_eq!(hook(&h, &status(wamid, "sent", json!({}))).await, StatusCode::OK);
    let (st, wa): (String, Option<String>) = sqlx::query_as("SELECT m.status, w.wa_status FROM messages m JOIN whatsapp_sent w ON w.message_id = m.id").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((st.as_str(), wa.as_deref()), ("delivered", Some("delivered")));
    hook(&h, &status(wamid, "read", json!({}))).await;
    hook(&h, &status(wamid, "read", json!({}))).await;
    // A failure after it was read is ignored.
    hook(&h, &status(wamid, "failed", json!({"errors": [{"code": 131026, "title": "t", "message": "t"}]}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics mt ON mt.id = e.metric_id WHERE mt.name = 'Read WhatsApp'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE opened_at IS NOT NULL AND status = 'delivered'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_contacts WHERE unreachable_at IS NOT NULL").await, 0);
    let pricing: Option<String> = sqlx::query_scalar("SELECT pricing_category FROM whatsapp_sent").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(pricing.as_deref(), Some("marketing"));

    // A status for a message DumbMail hasn't recorded yet, just now: Meta is asked to deliver it again.
    assert_eq!(hook(&h, &status("wamid.UNKNOWN", "delivered", json!({}))).await, StatusCode::SERVICE_UNAVAILABLE);
    let mut old = status("wamid.UNKNOWN", "delivered", json!({}));
    old["entry"][0]["changes"][0]["value"]["statuses"][0]["timestamp"] = json!((chrono::Utc::now().timestamp() - 3600).to_string());
    assert_eq!(hook(&h, &old).await, StatusCode::OK);
    h.cleanup().await;
}

#[tokio::test]
async fn marketing_needs_a_whatsapp_opt_in_not_an_sms_one() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(301);
    let p = person(&mut h, &them, Some("subscribed")).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIP"}))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [p]}))).await;
    let audience = json!({"include": [{"type": "list", "id": list}]});
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "whatsapp", "audience": audience}))).await;
    assert_eq!(est["count"], 0, "subscribed to texts only: {est}");
    let seg = json!({"definition": {"match": "all", "conditions": [{"type": "can_receive", "channel": "whatsapp"}]}});
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg.clone())).await["count"], 0);
    // A flow sending the marketing template skips them, without calling Meta.
    flow(&mut h, "Joined", sale(&t), false).await;
    trigger(&mut h, "Joined", &them).await;
    assert!(sends(&m).is_empty());
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "whatsapp", "audience": audience, "content": sale(&t)}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // After a WhatsApp opt-in they get it, and texts are untouched.
    subscribe(&mut h, &p).await;
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg)).await["count"], 1);
    let seg2 = json!({"definition": {"match": "all", "conditions": [{"type": "consent", "channel": "whatsapp", "status": "subscribed"}]}});
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg2)).await["count"], 1);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(sends(&m).len(), 1);
    assert_eq!(sends(&m)[0].0, them);
    let sms: String = sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE id = $1::uuid").bind(&p).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sms, "subscribed");
    // Subscribing needs evidence of a WhatsApp opt-in.
    let (s, _) = h.req("PUT", &format!("/api/whatsapp/profiles/{p}/consent"), Some(json!({"consent": "subscribed"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let report = h.ok("GET", &format!("/api/whatsapp/report?campaign_id={cid}"), None).await;
    assert_eq!(report["sent"], 1, "{report}");
    h.cleanup().await;
}

#[tokio::test]
async fn stop_on_whatsapp_stops_whatsapp_at_once_and_leaves_texts_alone() {
    use dumbmail::features::whatsapp::send::hook::Pause;
    let (mut h, m) = wa_harness!(0.0);
    let acct = setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(401);
    let p = person(&mut h, &them, Some("subscribed")).await;
    subscribe(&mut h, &p).await;
    flow(&mut h, "Joined", sale(&t), false).await;

    sqlx::query("UPDATE whatsapp_contacts SET consent_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    // A message held right before its final claim while STOP arrives on WhatsApp: it never goes.
    let pause = Pause::hold(acct);
    let h2 = h.state.clone();
    let phone = them.clone();
    let task = tokio::spawn(async move {
        dumbmail::audience::record_api_event(
            &h2.db,
            dumbmail::audience::NewEvent { account_id: acct, profile_id: p.parse().unwrap(), metric: "Joined", properties: json!({}), value: None, occurred_at: None, unique_id: None, message_id: None },
        )
        .await
        .unwrap();
        dumbmail::jobs::drain(&h2).await.unwrap();
        phone
    });
    pause.arrived().await;
    assert_eq!(hook(&h, &inbound_at(&them, "wamid.STOP1", "STOP", now_secs().await - 10)).await, StatusCode::OK);
    pause.release(acct);
    task.await.unwrap();
    h.drain().await;
    // Only the confirmation of the opt-out went (as words, inside the window), never the template.
    let s = sends(&m);
    assert!(s.iter().all(|(_, b)| b["type"] == "text"), "{s:?}");
    assert_eq!(s.len(), 1, "{s:?}");
    assert!(s[0].1["text"]["body"].as_str().unwrap().contains("won't get marketing messages"));
    let statuses = messages_to(&h, &them).await;
    assert!(statuses.contains(&("skipped".into(), Some("opted out of WhatsApp".into()))), "{statuses:?}");
    assert_eq!(consent_of(&h, &them).await, "unsubscribed");
    let sms: String = sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE phone_number = $1").bind(&them).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sms, "subscribed", "STOP on WhatsApp leaves texts alone");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics mt ON mt.id = e.metric_id WHERE mt.name = 'Unsubscribed from WhatsApp'").await, 1);
    // Later sends skip them.
    trigger(&mut h, "Joined", &them).await;
    assert_eq!(sends(&m).len(), 1);
    // START brings WhatsApp back.
    hook(&h, &inbound(&them, "wamid.START1", "START")).await;
    h.drain().await;
    assert_eq!(consent_of(&h, &them).await, "subscribed");
    h.cleanup().await;
}

#[tokio::test]
async fn metas_own_marketing_opt_out_wins_and_resume_subscribes_nobody() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(451);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    let pref = |v: &str| envelope("user_preferences", json!({"messaging_product": "whatsapp", "metadata": {"phone_number_id": PHONE_ID}, "contacts": [{"wa_id": digits(&them)}],
        "user_preferences": [{"wa_id": digits(&them), "detail": "User requested to stop marketing messages", "category": "marketing_messages", "value": v, "timestamp": chrono::Utc::now().timestamp()}]}));
    assert_eq!(hook(&h, &pref("stop")).await, StatusCode::OK);
    assert_eq!(consent_of(&h, &them).await, "unsubscribed");
    assert_eq!(hook(&h, &pref("resume")).await, StatusCode::OK);
    assert_eq!(consent_of(&h, &them).await, "unsubscribed");
    flow(&mut h, "Joined", sale(&t), false).await;
    trigger(&mut h, "Joined", &them).await;
    assert!(sends(&m).is_empty());

    // Error 131050 on a send records the opt-out too.
    let other = daytime_number(452);
    let q = person(&mut h, &other, None).await;
    subscribe(&mut h, &q).await;
    answer(&m, &other, meta_error(131050));
    trigger(&mut h, "Joined", &other).await;
    assert_eq!(consent_of(&h, &other).await, "unsubscribed");
    assert_eq!(messages_to(&h, &other).await[0].0, "failed");
    h.cleanup().await;
}

#[tokio::test]
async fn free_form_replies_only_inside_the_window() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let them = daytime_number(501);
    // They wrote 25 hours ago.
    assert_eq!(hook(&h, &inbound_at(&them, "wamid.OLD", "Do you ship to Canada?", chrono::Utc::now().timestamp() - 25 * 3600)).await, StatusCode::OK);
    let list = h.ok("GET", "/api/whatsapp/conversations", None).await;
    let c = list["conversations"][0].clone();
    assert_eq!((c["unread_count"].as_i64(), c["window_open"].as_bool(), c["channel"].as_str()), (Some(1), Some(false), Some("whatsapp")), "{list}");
    let cid = c["id"].as_str().unwrap().to_string();
    let conv = h.ok("GET", &format!("/api/whatsapp/conversations/{cid}"), None).await;
    assert_eq!(conv["reply"]["free_form"], false);
    assert!(conv["reply"]["why"].as_str().unwrap().contains("24 hours"));
    let (s, v) = h.req("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"text": "Yes we do!", "confirm": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(sends(&m).is_empty());
    // A marketing template needs their opt-in; a utility one can go.
    let sale_t = template(&h, "spring_sale").await;
    let (s, _) = h.req("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"template": sale(&sale_t), "confirm": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let order_t = template(&h, "order_update").await;
    let tpl = json!({"template_id": order_t, "header": "A-1", "body": {"1": "Sam", "2": "A-1"}, "buttons": {"0": "A-1"}});
    let out = h.ok("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"template": tpl, "confirm": true}))).await;
    assert_eq!(out["sent"], true, "{out}");
    assert_eq!(sends(&m)[0].1["type"], "template");

    // The window being closed is checked again right before the hand-off, not only when asked.
    sqlx::query("UPDATE whatsapp_contacts SET last_inbound_at = now() WHERE phone_number = $1").bind(&them).execute(&h.state.db).await.unwrap();
    let dry = h.ok("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"text": "Hi {{ first_name|default('there') }}"}))).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["text"].as_str()), (Some(true), Some("Hi there")));
    assert_eq!(sends(&m).len(), 1, "a dry run sends nothing");
    let out = h.ok("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"text": "Yes we do!", "confirm": true}))).await;
    assert_eq!(out["sent"], true, "{out}");
    let conv = h.ok("GET", &format!("/api/whatsapp/conversations/{cid}"), None).await;
    assert_eq!(conv["messages"].as_array().unwrap().len(), 3, "{conv}");
    assert_eq!(conv["messages"][2]["body"], "Yes we do!");
    // A stored reply whose window closed before the claim is skipped there.
    let mid: Uuid = {
        let mut tx = h.state.db.begin().await.unwrap();
        let pid: Uuid = sqlx::query_scalar("SELECT profile_id FROM whatsapp_conversations WHERE id = $1::uuid").bind(&cid).fetch_one(&mut *tx).await.unwrap();
        let acct = account_id(&h).await;
        let mid = dumbmail::features::whatsapp::send::prepare_conversational(&mut tx, acct, pid, &them, "reply", None, "late", None).await.unwrap();
        tx.commit().await.unwrap();
        mid
    };
    sqlx::query("UPDATE whatsapp_contacts SET last_inbound_at = now() - interval '25 hours' WHERE phone_number = $1").bind(&them).execute(&h.state.db).await.unwrap();
    let out = dumbmail::features::whatsapp::send::hand_off(&h.state, mid, false).await.unwrap();
    assert!(matches!(out, dumbmail::sending::Outcome::Skipped("24-hour window closed")), "{out:?}");
    assert_eq!(sends(&m).len(), 2);
    h.cleanup().await;
}

#[tokio::test]
async fn undeliverable_is_retried_a_few_times_then_the_number_counts_as_off_whatsapp() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(601);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    for _ in 0..3 {
        answer(&m, &them, meta_error(131026));
    }
    flow(&mut h, "Joined", sale(&t), false).await;
    trigger(&mut h, "Joined", &them).await;
    // One 131026 isn't proof they're off WhatsApp (an old app, terms not accepted): it waits and tries again.
    assert_eq!(messages_to(&h, &them).await[0].0, "retrying");
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_contacts WHERE unreachable_at IS NOT NULL").await, 0);
    h.fast_forward().await;
    assert_eq!(sends(&m).len(), 2);
    h.fast_forward().await;
    assert_eq!(sends(&m).len(), 3);
    h.fast_forward().await;
    assert_eq!(sends(&m).len(), 3, "a bounded number of tries");
    let msgs = messages_to(&h, &them).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0, "failed");
    assert!(msgs[0].1.as_deref().unwrap().contains("131026"), "{msgs:?}");
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_contacts WHERE unreachable_at IS NOT NULL").await, 1);
    // Not in audiences, and the next flow message isn't even tried.
    let seg = json!({"definition": {"match": "all", "conditions": [{"type": "can_receive", "channel": "whatsapp"}]}});
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg)).await["count"], 0);
    flow(&mut h, "Joined again", sale(&t), false).await;
    trigger(&mut h, "Joined again", &them).await;
    assert_eq!(sends(&m).len(), 3);
    // Writing in shows they're on WhatsApp after all.
    hook(&h, &inbound(&them, "wamid.HI", "hi")).await;
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_contacts WHERE unreachable_at IS NOT NULL").await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn a_held_person_gets_only_service_templates_from_a_service_flow() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let utility = template(&h, "order_update").await;
    let marketing = template(&h, "spring_sale").await;
    let them = daytime_number(701);
    person(&mut h, &them, None).await;
    sqlx::query("UPDATE profiles SET marketing_hold = true WHERE phone_number = $1").bind(&them).execute(&h.state.db).await.unwrap();
    flow(&mut h, "Shipped", order(&utility), false).await;
    flow(&mut h, "ShippedService", order(&utility), true).await;
    flow(&mut h, "PromoService", sale(&marketing), true).await;
    trigger(&mut h, "Shipped", &them).await;
    assert!(sends(&m).is_empty(), "not a service flow");
    trigger(&mut h, "PromoService", &them).await;
    assert!(sends(&m).is_empty(), "a marketing template never goes without an opt-in");
    trigger(&mut h, "ShippedService", &them).await;
    let s = sends(&m);
    assert_eq!(s.len(), 1);
    let comps = &s[0].1["template"]["components"];
    assert_eq!(comps[0], json!({"type": "header", "parameters": [{"type": "text", "text": "A-100"}]}));
    assert_eq!(comps[1]["parameters"][1]["text"], "A-100");
    assert_eq!(comps[2], json!({"type": "button", "sub_type": "url", "index": "0", "parameters": [{"type": "text", "text": "A-100"}]}));
    h.cleanup().await;
}

#[tokio::test]
async fn quiet_hours_defer_whatsapp() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = night_number(801);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    flow(&mut h, "Joined", sale(&t), false).await;
    trigger(&mut h, "Joined", &them).await;
    assert!(sends(&m).is_empty(), "it's the middle of the night there");
    let waiting = count(&h, "SELECT count(*) FROM jobs WHERE kind = 'flow_step' AND run_at > now() + interval '1 hour'").await;
    assert_eq!(waiting, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn rate_limits_wait_and_unclear_answers_never_resend() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(901);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    flow(&mut h, "Joined", sale(&t), false).await;
    answer(&m, &them, meta_error(130429));
    trigger(&mut h, "Joined", &them).await;
    assert_eq!(sends(&m).len(), 1);
    assert_eq!(messages_to(&h, &them).await[0].0, "retrying");
    // The sender backs off: nothing goes until it's over.
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_backoff WHERE scope = 'sender' AND until > now()").await, 1);
    h.fast_forward().await;
    assert_eq!(sends(&m).len(), 1, "still backing off");
    sqlx::query("UPDATE whatsapp_backoff SET until = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sends(&m).len(), 2);
    assert_eq!(messages_to(&h, &them).await, vec![("sent".into(), None)], "one message, sent on the retry");
    h.fast_forward().await;
    assert_eq!(sends(&m).len(), 2);

    // An answer without a message id: it may have gone, so it's never sent again.
    let other = daytime_number(902);
    let q = person(&mut h, &other, None).await;
    subscribe(&mut h, &q).await;
    answer(&m, &other, (200, json!({"messaging_product": "whatsapp"})));
    trigger(&mut h, "Joined", &other).await;
    h.fast_forward().await;
    assert_eq!(sends(&m).iter().filter(|(to, _)| *to == other).count(), 1);
    assert_eq!(messages_to(&h, &other).await, vec![("failed".into(), Some("no answer from Meta; not sent again".into()))]);

    // A gateway error, or an error without Meta's code, may have gone too: never sent again.
    for (i, (code, body)) in [(503u16, json!({"message": "Service Unavailable"})), (502, json!({"error": {"message": "x", "code": 2}})), (400, json!({"oops": true}))].into_iter().enumerate() {
        let n = daytime_number(903 + i as u32);
        let q = person(&mut h, &n, None).await;
        subscribe(&mut h, &q).await;
        answer(&m, &n, (code, body));
        trigger(&mut h, "Joined", &n).await;
        h.fast_forward().await;
        assert_eq!(sends(&m).iter().filter(|(to, _)| *to == n).count(), 1, "HTTP {code} isn't retried");
        assert_eq!(messages_to(&h, &n).await, vec![("failed".into(), Some("no answer from Meta; not sent again".into()))], "HTTP {code}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn a_paused_template_stops_and_the_owner_hears() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let a = daytime_number(1001);
    let b = daytime_number(1002);
    for n in [&a, &b] {
        let p = person(&mut h, n, None).await;
        subscribe(&mut h, &p).await;
    }
    flow(&mut h, "Joined", sale(&t), false).await;
    h.state.email.clear_sent();
    answer(&m, &a, meta_error(132015));
    trigger(&mut h, "Joined", &a).await;
    let status: String = sqlx::query_scalar("SELECT status FROM whatsapp_templates WHERE name = 'spring_sale'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "PAUSED");
    assert!(h.state.email.sent().iter().any(|e| e.to == "owner@acme.test" && e.subject.contains("spring_sale")), "the owner is told");
    trigger(&mut h, "Joined", &b).await;
    assert_eq!(sends(&m).len(), 1, "the paused template isn't used again");
    assert_eq!(messages_to(&h, &b).await.len(), 0);
    // Meta's webhook approving it again puts it back to work.
    // Meta's own pause webhook for the same pause doesn't alert again.
    let paused = envelope_at("message_template_status_update", json!({"event": "PAUSED", "message_template_id": 9001, "message_template_name": "spring_sale", "message_template_language": "en_US", "reason": "NONE"}), now_secs().await);
    assert_eq!(hook(&h, &paused).await, StatusCode::OK);
    assert_eq!(h.state.email.sent().iter().filter(|e| e.subject.contains("spring_sale")).count(), 1, "one alert");
    // An approval Meta sent before the pause (delivered late) doesn't reopen it, nor does a read that
    // started before the pause.
    let old = envelope_at("message_template_status_update", json!({"event": "APPROVED", "message_template_id": 9001, "message_template_name": "spring_sale", "message_template_language": "en_US", "reason": "NONE"}), now_secs().await - 3600);
    assert_eq!(hook(&h, &old).await, StatusCode::OK);
    sqlx::query("UPDATE whatsapp_templates SET paused_at = now() + interval '1 hour' WHERE name = 'spring_sale'").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/whatsapp/templates/sync", None).await;
    let status: String = sqlx::query_scalar("SELECT status FROM whatsapp_templates WHERE name = 'spring_sale'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "PAUSED", "still paused");
    // A read made after the pause shows it approved: back to work.
    sqlx::query("UPDATE whatsapp_templates SET paused_at = now() - interval '1 minute', status_at = now() - interval '1 minute' WHERE name = 'spring_sale'").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/whatsapp/templates/sync", None).await;
    flow(&mut h, "Joined again", sale(&t), false).await;
    trigger(&mut h, "Joined again", &b).await;
    assert_eq!(sends(&m).len(), 2);
    h.cleanup().await;
}

#[tokio::test]
async fn templates_created_tracked_clicked_and_billed_once() {
    let (mut h, m) = wa_harness!(0.01);
    let acct = setup(&mut h).await;
    h.credit(5.0).await;
    let body = json!({"name": "welcome_offer", "language": "en_US", "category": "MARKETING", "body": "Hi {{1}}, welcome to Acme!", "body_examples": ["Sam"],
        "buttons": [{"type": "url", "text": "Shop now", "url": "https://acme.test/new"}]});
    let dry = h.ok("POST", "/api/whatsapp/templates", Some(body.clone())).await;
    assert_eq!(dry["dry_run"], true);
    assert!(m.lock().unwrap().created.is_empty());
    let mut confirmed = body.clone();
    confirmed["confirm"] = json!(true);
    let t = h.ok("POST", "/api/whatsapp/templates", Some(confirmed)).await;
    assert_eq!((t["status"].as_str(), t["category"].as_str()), (Some("PENDING"), Some("MARKETING")), "{t}");
    let submitted = m.lock().unwrap().created[0].clone();
    assert_eq!(submitted["components"][1]["buttons"][0]["url"], "https://pl.test/wa/{{1}}", "{submitted}");
    // Not sendable until Meta approves it (the webhook says so).
    let tid = t["id"].as_str().unwrap().to_string();
    let them = daytime_number(1101);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [p]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Welcome", "channel": "whatsapp", "audience": {"include": [{"type": "list", "id": list}]}, "content": sale(&tid)}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert!(s == StatusCode::BAD_REQUEST && v.to_string().contains("pending"), "{v}");
    let meta_id = t["meta_id"].as_str().unwrap().parse::<i64>().unwrap();
    hook(&h, &envelope("message_template_status_update", json!({"event": "APPROVED", "message_template_id": meta_id, "message_template_name": "welcome_offer", "message_template_language": "en_US", "reason": "NONE"}))).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let s = sends(&m);
    assert_eq!(s.len(), 1);
    let button = &s[0].1["template"]["components"][1];
    assert_eq!((button["sub_type"].as_str(), button["index"].as_str()), (Some("url"), Some("0")), "{button}");
    let token = button["parameters"][0]["text"].as_str().unwrap().to_string();
    // Charged once, when Meta accepted it; its statuses don't charge it again.
    let db = h.state.db.clone();
    let charged = || {
        let db = db.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT COALESCE(sum(quantity), 0)::bigint FROM ledger WHERE kind = 'whatsapp'").fetch_one(&db).await.unwrap() }
    };
    hook(&h, &status("wamid.TEST1", "sent", json!({}))).await;
    hook(&h, &status("wamid.TEST1", "delivered", json!({}))).await;
    hook(&h, &status("wamid.TEST1", "sent", json!({}))).await;
    assert_eq!(charged().await, 1);
    // The tap goes through DumbMail: recorded, then on to the button's own link.
    let (st, headers, _) = h.raw("GET", &format!("/wa/{token}"), &[], "").await;
    assert!(st.is_redirection(), "{st}");
    assert!(headers.get("location").unwrap().to_str().unwrap().starts_with("https://acme.test/new"));
    assert_eq!(h.raw("GET", &format!("/wa/{}x", &token[..token.len() - 1]), &[], "").await.0, StatusCode::BAD_REQUEST);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics mt ON mt.id = e.metric_id WHERE mt.name = 'Clicked WhatsApp'").await, 1);
    let report = h.ok("GET", &format!("/api/whatsapp/report?campaign_id={cid}"), None).await;
    assert_eq!((report["sent"].as_i64(), report["clicked"].as_i64()), (Some(1), Some(1)), "{report}");
    // An order after the tap is credited like an order after a text click.
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"phone_number": them}, "value": 30.0}))).await;
    let camp = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(camp["stats"]["revenue"], 30.0, "{camp}");
    // The browser can't forge WhatsApp's events.
    let key: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let (s, _, _) = h.raw("POST", &format!("/client/events?key={key}"), &[("content-type", "application/json")], &json!({"metric": "Clicked WhatsApp", "profile": {"phone_number": them}}).to_string()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn only_whatsapps_own_signup_words_and_buttons_subscribe() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Club"}))).await["id"].as_str().unwrap().to_string();
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"program_name": "Acme", "message_frequency": "Up to 4 msgs/month", "support_contact": "help@acme.test"}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true, "reply": "Welcome to texts!", "enabled": true}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "HOURS", "reply": "Open 9-5.", "list_id": list, "enabled": true}))).await;
    let them = daytime_number(1201);
    // A texting sign-up keyword isn't WhatsApp consent.
    hook(&h, &inbound(&them, "wamid.K0", "join")).await;
    h.drain().await;
    assert_eq!(consent_of(&h, &them).await, "never_subscribed");
    assert!(sends(&m).is_empty(), "no texting confirmation on WhatsApp");
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"signup_keywords": ["JOIN"], "signup_reply": "Welcome to the club!", "optin_buttons": ["YES_WA"]}))).await;
    // A media caption or a generic button label that happens to be the word isn't consent either.
    let mut caption = inbound(&them, "wamid.K1", "");
    caption["entry"][0]["changes"][0]["value"]["messages"][0]["type"] = json!("image");
    caption["entry"][0]["changes"][0]["value"]["messages"][0]["image"] = json!({"caption": "join", "id": "media1"});
    hook(&h, &caption).await;
    let mut button = inbound(&them, "wamid.K2", "");
    button["entry"][0]["changes"][0]["value"]["messages"][0]["type"] = json!("button");
    button["entry"][0]["changes"][0]["value"]["messages"][0]["button"] = json!({"text": "join", "payload": "join"});
    hook(&h, &button).await;
    h.drain().await;
    assert_eq!(consent_of(&h, &them).await, "never_subscribed");
    // The configured word, typed, is.
    hook(&h, &inbound(&them, "wamid.K3", "join")).await;
    h.drain().await;
    assert_eq!(consent_of(&h, &them).await, "subscribed");
    let sms: String = sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE phone_number = $1").bind(&them).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sms, "never_subscribed", "a WhatsApp sign-up isn't a texting sign-up");
    // So is the configured opt-in button.
    let other = daytime_number(1202);
    let mut yes = inbound(&other, "wamid.K4", "");
    yes["entry"][0]["changes"][0]["value"]["messages"][0]["type"] = json!("button");
    yes["entry"][0]["changes"][0]["value"]["messages"][0]["button"] = json!({"text": "Yes please", "payload": "YES_WA"});
    hook(&h, &yes).await;
    assert_eq!(consent_of(&h, &other).await, "subscribed");
    // Texting keywords that don't subscribe still work on WhatsApp text.
    hook(&h, &inbound(&them, "wamid.K5", "hours")).await;
    h.drain().await;
    assert_eq!(count(&h, &format!("SELECT count(*) FROM list_members WHERE list_id = '{list}'")).await, 1);
    let bodies: Vec<String> = sends(&m).iter().filter(|(to, _)| *to == them).map(|(_, b)| b["text"]["body"].as_str().unwrap_or_default().to_string()).collect();
    assert!(bodies[0].starts_with("Welcome to the club!") && bodies[0].contains("Reply STOP"), "{bodies:?}");
    assert_eq!(bodies[1], "Open 9-5.");
    // Automatic answers are capped per person per hour.
    for i in 0..8 {
        hook(&h, &inbound(&them, &format!("wamid.K6{i}"), "hours")).await;
    }
    h.drain().await;
    assert_eq!(sends(&m).iter().filter(|(to, _)| *to == them).count(), 5);
    // Replies don't count for smart sending.
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = $1").bind(&them).fetch_one(&h.state.db).await.unwrap();
    assert!(!dumbmail::features::sendtime::recently_messaged(&h.state.db, pid, "whatsapp", 24).await.unwrap());
    h.cleanup().await;
}

#[tokio::test]
async fn tests_go_to_team_numbers_and_permissions_and_accounts_hold() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "order_update").await;
    let content = json!({"template_id": t, "header": "A-1", "body": {"1": "{{ first_name|default('there') }}", "2": "A-1"}, "buttons": {"0": "A-1"}});
    let (s, v) = h.req("POST", "/api/whatsapp/test", Some(json!({"to": "+15035550111", "content": content}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // Added but not yet verified: still not a test number.
    h.ok("POST", "/api/whatsapp/test-numbers", Some(json!({"phone_number": TEST_NUMBER}))).await;
    let (s, _) = h.req("POST", "/api/whatsapp/test", Some(json!({"to": TEST_NUMBER, "content": content}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // A wrong code from the phone verifies nothing.
    hook(&h, &inbound(TEST_NUMBER, "wamid.BAD", "DM-000000")).await;
    let (s, _) = h.req("POST", "/api/whatsapp/test", Some(json!({"to": TEST_NUMBER, "content": content}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(sends(&m).is_empty());
    verify_tester(&mut h, TEST_NUMBER).await;
    let out = h.ok("POST", "/api/whatsapp/test", Some(json!({"to": TEST_NUMBER, "content": content}))).await;
    assert_eq!(out["sent"], true, "{out}");
    assert_eq!(sends(&m)[0].0, TEST_NUMBER);
    // A campaign's test, also only to team numbers.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "T", "channel": "whatsapp", "audience": {"include": []}, "content": content}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["+15035550112"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": [TEST_NUMBER]}))).await;
    assert_eq!(sends(&m).len(), 2);
    // A member who leaves while their test waits at the hand-off: it doesn't go.
    let acct = account_id(&h).await;
    let ex = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, 'ex@acme.test', 'x')").bind(ex).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, 'manager')").bind(ex).bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO whatsapp_test_numbers (id, account_id, user_id, phone_number, code, verified_at) VALUES ($1, $2, $3, '+15035550177', 'DM-1', now())")
        .bind(Uuid::now_v7())
        .bind(acct)
        .bind(ex)
        .execute(&h.state.db)
        .await
        .unwrap();
    let pause = dumbmail::features::whatsapp::send::hook::Pause::hold(acct);
    let r = h.router.clone();
    let cookie = h.cookie.clone().unwrap();
    let body = json!({"to": "+15035550177", "content": content}).to_string();
    let task = tokio::spawn(async move {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/whatsapp/test")
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .header("origin", "https://pl.test")
            .header("x-dumbmail-account", acct.to_string())
            .body(axum::body::Body::from(body))
            .unwrap();
        tower::ServiceExt::oneshot(r, req).await.unwrap().status()
    });
    pause.arrived().await;
    sqlx::query("DELETE FROM memberships WHERE user_id = $1").bind(ex).execute(&h.state.db).await.unwrap();
    pause.release(acct);
    assert_eq!(task.await.unwrap(), StatusCode::BAD_REQUEST);
    assert_eq!(sends(&m).len(), 2, "nothing went to the former member");
    // Preview fills the template for a sample person.
    let pv = h.ok("POST", "/api/render", Some(json!({"channel": "whatsapp", "content": content}))).await;
    assert!(pv["text"].as_str().unwrap().contains("Hi Alex, your order A-1 has shipped."), "{pv}");

    // A read-only key reads templates but can't create one or send a test.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let (s, _, body) = h.raw("GET", "/api/whatsapp/templates", &[("authorization", &auth)], "").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _, _) = h.raw("POST", "/api/whatsapp/templates", &[("authorization", &auth), ("content-type", "application/json")], &json!({"name": "x_y", "language": "en", "category": "UTILITY", "body": "Hi there", "confirm": true}).to_string()).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = h.raw("POST", "/api/whatsapp/test", &[("authorization", &auth), ("content-type", "application/json")], &json!({"to": TEST_NUMBER, "content": content}).to_string()).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Another account sees none of it, and can't send this account's template.
    let them = daytime_number(1301);
    hook(&h, &inbound(&them, "wamid.ISO", "hello")).await;
    let conv: Uuid = sqlx::query_scalar("SELECT id FROM whatsapp_conversations").fetch_one(&h.state.db).await.unwrap();
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other Co"}))).await;
    assert_eq!(h.req("GET", &format!("/api/whatsapp/conversations/{conv}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.req("GET", &format!("/api/whatsapp/templates/{t}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.ok("GET", "/api/whatsapp/conversations", None).await["conversations"], json!([]));
    // Its own connection can't claim the first account's phone number.
    let (s, _) = h.req("PATCH", "/api/whatsapp/settings", Some(json!({"waba_id": "3003", "phone_number_id": PHONE_ID, "access_token": TOKEN}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"waba_id": "3003", "phone_number_id": "4004", "access_token": TOKEN, "app_secret": SECRET}))).await;
    let (s, _) = h.req("POST", "/api/whatsapp/test", Some(json!({"to": TEST_NUMBER, "content": content}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the other account's template");
    h.cleanup().await;
}

#[tokio::test]
async fn an_unknown_webhook_address_reads_nothing() {
    let mut h = harness!();
    h.signup().await;
    let body = inbound("+15035550100", "wamid.X", "STOP").to_string();
    let (s, _, _) = h.raw("POST", "/webhooks/whatsapp/nope", &[("x-hub-signature-256", &sign(&body))], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Settings without an app secret can't check a signature, so nothing is read.
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"waba_id": WABA, "phone_number_id": PHONE_ID, "access_token": TOKEN}))).await;
    let key: String = sqlx::query_scalar("SELECT webhook_key FROM whatsapp_settings").fetch_one(&h.state.db).await.unwrap();
    let (s, _, _) = h.raw("POST", &format!("/webhooks/whatsapp/{key}"), &[("x-hub-signature-256", &sign(&body))], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_contacts").await, 0);
    h.cleanup().await;
}

/// Run `metric` for `phone` on a separate task, held right before its final claim; returns the
/// hold and the task.
async fn held_send(h: &Harness, acct: Uuid, metric: &'static str, phone: &str) -> (std::sync::Arc<dumbmail::features::whatsapp::send::hook::Pause>, tokio::task::JoinHandle<()>) {
    let pause = dumbmail::features::whatsapp::send::hook::Pause::hold(acct);
    let st = h.state.clone();
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = $1").bind(phone).fetch_one(&h.state.db).await.unwrap();
    let task = tokio::spawn(async move {
        dumbmail::audience::record_api_event(
            &st.db,
            dumbmail::audience::NewEvent { account_id: acct, profile_id: pid, metric, properties: json!({"order_id": "A-100"}), value: None, occurred_at: None, unique_id: None, message_id: None },
        )
        .await
        .unwrap();
        dumbmail::jobs::drain(&st).await.unwrap();
    });
    pause.arrived().await;
    (pause, task)
}

#[tokio::test]
async fn the_category_at_the_hand_off_decides() {
    let (mut h, m) = wa_harness!(0.0);
    let acct = setup(&mut h).await;
    let utility = template(&h, "order_update").await;
    let them = daytime_number(1401);
    person(&mut h, &them, None).await;
    sqlx::query("UPDATE profiles SET marketing_hold = true WHERE phone_number = $1").bind(&them).execute(&h.state.db).await.unwrap();
    flow(&mut h, "Shipped", order(&utility), true).await;
    // Prepared as a utility message for a held person (allowed from a service flow); Meta then
    // recategorizes the template as marketing before the hand-off.
    let (pause, task) = held_send(&h, acct, "Shipped", &them).await;
    let ev = envelope_at("template_category_update", json!({"message_template_id": 9002, "message_template_name": "order_update", "message_template_language": "en_US", "previous_category": "UTILITY", "new_category": "MARKETING"}), now_secs().await);
    assert_eq!(hook(&h, &ev).await, StatusCode::OK);
    pause.release(acct);
    task.await.unwrap();
    assert!(sends(&m).is_empty(), "a marketing template now: no opt-in, and held");
    assert_eq!(messages_to(&h, &them).await, vec![("skipped".into(), Some("no marketing consent".into()))]);
    // An upcoming change (correct_category) changes nothing yet.
    let upcoming = envelope_at("template_category_update", json!({"message_template_id": 9002, "message_template_name": "order_update", "new_category": "UTILITY", "correct_category": "MARKETING", "category_update_timestamp": now_secs().await + 86400}), now_secs().await);
    hook(&h, &upcoming).await;
    let cat: String = sqlx::query_scalar("SELECT category FROM whatsapp_templates WHERE name = 'order_update'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(cat, "MARKETING");
    h.cleanup().await;
}

#[tokio::test]
async fn the_claim_reads_the_live_number_and_campaign() {
    let (mut h, m) = wa_harness!(0.0);
    let acct = setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(1501);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    flow(&mut h, "Joined", sale(&t), false).await;
    // Their number changes while the message waits at the hand-off: it doesn't go to the old one.
    let (pause, task) = held_send(&h, acct, "Joined", &them).await;
    h.ok("PATCH", &format!("/api/profiles/{p}"), Some(json!({"phone_number": daytime_number(1502)}))).await;
    pause.release(acct);
    task.await.unwrap();
    assert!(sends(&m).is_empty());
    assert_eq!(messages_to(&h, &them).await, vec![("skipped".into(), Some("phone number changed".into()))]);

    // A campaign cancelled while its message waits at the hand-off: nothing goes.
    let other = daytime_number(1503);
    let q = person(&mut h, &other, None).await;
    subscribe(&mut h, &q).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [q]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "whatsapp", "audience": {"include": [{"type": "list", "id": list}]}, "content": sale(&t)}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let pause = dumbmail::features::whatsapp::send::hook::Pause::hold(acct);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let st = h.state.clone();
    let task = tokio::spawn(async move { dumbmail::jobs::drain(&st).await.unwrap() });
    pause.arrived().await;
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    pause.release(acct);
    task.await.unwrap();
    assert!(sends(&m).is_empty());
    assert_eq!(messages_to(&h, &other).await[0].0, "skipped");
    h.cleanup().await;
}

#[tokio::test]
async fn a_new_sender_starts_fresh() {
    let (mut h, m) = wa_harness!(0.0);
    let acct = setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    let them = daytime_number(1601);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    hook(&h, &inbound(&them, "wamid.W1", "hi")).await;
    flow(&mut h, "Joined", sale(&t), false).await;
    // A message waiting at the hand-off when the account switches to another WhatsApp number.
    let (pause, task) = held_send(&h, acct, "Joined", &them).await;
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"phone_number_id": "2999"}))).await;
    pause.release(acct);
    task.await.unwrap();
    assert!(sends(&m).is_empty());
    assert_eq!(messages_to(&h, &them).await, vec![("skipped".into(), Some("WhatsApp sender changed".into()))]);
    // Consent given to the old number doesn't carry over, and its service window closed.
    assert_eq!(consent_of(&h, &them).await, "never_subscribed");
    let window: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT last_inbound_at FROM whatsapp_contacts WHERE phone_number = $1").bind(&them).fetch_one(&h.state.db).await.unwrap();
    assert!(window.is_none());
    // A campaign message its batch already claimed, waiting at the hand-off when the number changes
    // again: prepared for the old sender, it never goes from the new one.
    subscribe(&mut h, &p).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [p]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "whatsapp", "audience": {"include": [{"type": "list", "id": list}]}, "content": sale(&t)}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let pause = dumbmail::features::whatsapp::send::hook::Pause::hold(acct);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let st = h.state.clone();
    let task = tokio::spawn(async move { dumbmail::jobs::drain(&st).await.unwrap() });
    pause.arrived().await;
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"phone_number_id": "2888"}))).await;
    pause.release(acct);
    task.await.unwrap();
    assert!(sends(&m).is_empty());
    let last: (String, Option<String>) = sqlx::query_as("SELECT status, error FROM messages WHERE campaign_id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(last, ("skipped".into(), Some("WhatsApp sender changed".into())));
    // A disconnect stops sends that were already prepared too.
    subscribe(&mut h, &p).await;
    flow(&mut h, "Joined again", sale(&t), false).await;
    let (pause, task) = held_send(&h, acct, "Joined again", &them).await;
    h.ok("DELETE", "/api/whatsapp/settings", None).await;
    pause.release(acct);
    task.await.unwrap();
    assert!(sends(&m).is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn stop_cant_be_lost_or_undone() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    h.ok("PATCH", "/api/whatsapp/settings", Some(json!({"signup_keywords": ["JOIN"]}))).await;
    let them = daytime_number(1701);
    let now = now_secs().await;
    // STOP at t, then a START and a sign-up sent earlier but delivered later: still opted out.
    hook(&h, &inbound_at(&them, "wamid.S1", "STOP", now - 60)).await;
    hook(&h, &inbound_at(&them, "wamid.S0", "START", now - 120)).await;
    hook(&h, &inbound_at(&them, "wamid.S2", "join", now - 90)).await;
    assert_eq!(consent_of(&h, &them).await, "unsubscribed");
    // Meta's preference events apply once, in their own order, STOP winning a tie.
    let pref = |v: &str, ts: i64| envelope("user_preferences", json!({"messaging_product": "whatsapp", "metadata": {"phone_number_id": PHONE_ID},
        "user_preferences": [{"wa_id": digits(&them), "category": "marketing_messages", "value": v, "timestamp": ts}]}));
    hook(&h, &inbound_at(&them, "wamid.S3", "join", now - 30)).await;
    assert_eq!(consent_of(&h, &them).await, "subscribed", "a newer sign-up does subscribe");
    hook(&h, &pref("stop", now - 30)).await;
    assert_eq!(consent_of(&h, &them).await, "unsubscribed", "a stop at the same moment wins");
    hook(&h, &pref("stop", now - 30)).await;
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_consent_log WHERE source = 'meta_opt_out'").await, 1, "applied once");
    // A 131050 failure status commits its opt-out with it.
    let other = daytime_number(1702);
    let p = person(&mut h, &other, None).await;
    subscribe(&mut h, &p).await;
    let t = template(&h, "spring_sale").await;
    flow(&mut h, "Joined", sale(&t), false).await;
    trigger(&mut h, "Joined", &other).await;
    let wamid = format!("wamid.TEST{}", sends(&m).len());
    hook(&h, &status(&wamid, "failed", json!({"errors": [{"code": 131050, "title": "t", "message": "t"}]}))).await;
    assert_eq!(consent_of(&h, &other).await, "unsubscribed");
    assert_eq!(messages_to(&h, &other).await[0].0, "failed");
    h.cleanup().await;
}

#[tokio::test]
async fn only_the_signed_sender_counts_and_business_ids_resolve() {
    let (mut h, _m) = wa_harness!(0.0);
    setup(&mut h).await;
    let them = daytime_number(1801);
    let p = person(&mut h, &them, None).await;
    subscribe(&mut h, &p).await;
    // A preference for another phone number of the same business account does nothing here.
    let other_number = envelope("user_preferences", json!({"messaging_product": "whatsapp", "metadata": {"phone_number_id": "9999"},
        "user_preferences": [{"wa_id": digits(&them), "category": "marketing_messages", "value": "stop", "timestamp": now_secs().await}]}));
    hook(&h, &other_number).await;
    assert_eq!(consent_of(&h, &them).await, "subscribed");
    // A STOP from someone known only by a business-scoped id is kept, and applies once a signed
    // message ties that id to their number.
    let mut bsuid = inbound(&them, "wamid.B1", "STOP");
    let msg = &mut bsuid["entry"][0]["changes"][0]["value"];
    msg["messages"][0].as_object_mut().unwrap().remove("from");
    msg["messages"][0]["from_user_id"] = json!("US.123");
    msg["contacts"] = json!([{"user_id": "US.123", "profile": {"name": "Sam"}}]);
    assert_eq!(hook(&h, &bsuid).await, StatusCode::OK);
    assert_eq!(consent_of(&h, &them).await, "subscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM whatsapp_bsuid_optouts").await, 1);
    let mut tie = inbound_at(&them, "wamid.B2", "hello", now_secs().await - 600);
    tie["entry"][0]["changes"][0]["value"]["messages"][0]["from_user_id"] = json!("US.123");
    hook(&h, &tie).await;
    assert_eq!(consent_of(&h, &them).await, "unsubscribed");
    // A message without Meta's time opens no reply window and signs nobody up.
    let late = daytime_number(1802);
    let mut untimed = inbound(&late, "wamid.B3", "hi");
    untimed["entry"][0]["changes"][0]["value"]["messages"][0].as_object_mut().unwrap().remove("timestamp");
    hook(&h, &untimed).await;
    let window: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT last_inbound_at FROM whatsapp_contacts WHERE phone_number = $1").bind(&late).fetch_one(&h.state.db).await.unwrap();
    assert!(window.is_none());
    h.cleanup().await;
}

#[tokio::test]
async fn credit_is_held_and_charged_only_for_what_went_out() {
    let (mut h, m) = wa_harness!(0.05);
    let acct = setup(&mut h).await;
    h.credit(1.0).await;
    let db = h.state.db.clone();
    let balance = || {
        let db = db.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT balance_micros FROM accounts WHERE name = 'Acme Goods'").fetch_one(&db).await.unwrap() }
    };
    let start = balance().await;
    let t = template(&h, "spring_sale").await;
    flow(&mut h, "Joined", sale(&t), false).await;
    let a = daytime_number(1901);
    let pa = person(&mut h, &a, None).await;
    subscribe(&mut h, &pa).await;
    // Meta holds it for a quality check: credit held, nothing charged yet.
    answer(&m, &a, (200, json!({"messaging_product": "whatsapp", "contacts": [{"wa_id": "1"}], "messages": [{"id": "wamid.HELD", "message_status": "held_for_quality_assessment"}]})));
    trigger(&mut h, "Joined", &a).await;
    assert_eq!(start - balance().await, 50_000, "held");
    assert_eq!(count(&h, "SELECT count(*) FROM ledger WHERE kind = 'whatsapp'").await, 0);
    // Meta drops it: the credit comes back.
    hook(&h, &status("wamid.HELD", "failed", json!({"errors": [{"code": 135000, "title": "t", "message": "t"}]}))).await;
    assert_eq!(balance().await, start);
    // A held one Meta later sends is charged then, once.
    let a2 = daytime_number(1903);
    let pa2 = person(&mut h, &a2, None).await;
    subscribe(&mut h, &pa2).await;
    answer(&m, &a2, (200, json!({"messaging_product": "whatsapp", "contacts": [{"wa_id": "1"}], "messages": [{"id": "wamid.HELD2", "message_status": "held_for_quality_assessment"}]})));
    trigger(&mut h, "Joined", &a2).await;
    assert_eq!(count(&h, "SELECT count(*) FROM ledger WHERE kind = 'whatsapp'").await, 0);
    hook(&h, &status("wamid.HELD2", "sent", json!({}))).await;
    hook(&h, &status("wamid.HELD2", "delivered", json!({}))).await;
    assert_eq!(start - balance().await, 50_000);
    assert_eq!(count(&h, "SELECT COALESCE(sum(quantity), 0)::bigint FROM ledger WHERE kind = 'whatsapp'").await, 1);
    // An inbox reply Meta accepts is charged once; a failure reported later (the webhook is signed
    // with the customer's own secret) doesn't give it back.
    let b = daytime_number(1902);
    hook(&h, &inbound(&b, "wamid.R1", "hello")).await;
    let cid: Uuid = sqlx::query_scalar("SELECT id FROM whatsapp_conversations WHERE phone_number = $1").bind(&b).fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"text": "Hi!", "confirm": true}))).await;
    let wamid = format!("wamid.TEST{}", sends(&m).len());
    assert_eq!(start - balance().await, 100_000);
    hook(&h, &status(&wamid, "failed", json!({"errors": [{"code": 131000, "title": "t", "message": "t"}]}))).await;
    assert_eq!(start - balance().await, 100_000, "charged once, not refunded by a webhook");
    assert_eq!(count(&h, "SELECT COALESCE(sum(quantity), 0)::bigint FROM ledger WHERE kind = 'whatsapp'").await, 2);
    // Without enough credit, nothing goes.
    sqlx::query("UPDATE accounts SET balance_micros = 10 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/whatsapp/conversations/{cid}/reply"), Some(json!({"text": "Again", "confirm": true}))).await;
    assert_eq!(sends(&m).len(), 3);
    h.cleanup().await;
}

#[tokio::test]
async fn pair_limits_back_off_one_person_and_late_failures_retry() {
    let (mut h, m) = wa_harness!(0.0);
    setup(&mut h).await;
    let t = template(&h, "spring_sale").await;
    flow(&mut h, "Joined", sale(&t), false).await;
    flow(&mut h, "Joined 2", sale(&t), false).await;
    let a = daytime_number(2001);
    let b = daytime_number(2002);
    for n in [&a, &b] {
        let p = person(&mut h, n, None).await;
        subscribe(&mut h, &p).await;
    }
    // Meta's pair rate limit for A: A waits, B doesn't.
    answer(&m, &a, meta_error(131056));
    trigger(&mut h, "Joined", &a).await;
    trigger(&mut h, "Joined 2", &a).await;
    trigger(&mut h, "Joined", &b).await;
    let to: Vec<String> = sends(&m).into_iter().map(|(t, _)| t).collect();
    assert_eq!(to, vec![a.clone(), b.clone()], "the second message to A waited");
    // A failure Meta reports later for a rate limit is tried again, on the background lane.
    hook(&h, &status("wamid.TEST2", "failed", json!({"errors": [{"code": 130429, "title": "t", "message": "t"}]}))).await;
    assert_eq!(messages_to(&h, &b).await[0].0, "retrying");
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'whatsapp_send' AND lane = 'background'").await, 1);
    sqlx::query("UPDATE whatsapp_backoff SET until = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sends(&m).iter().filter(|(t, _)| *t == b).count(), 2);
    assert_eq!(messages_to(&h, &b).await[0].0, "sent");
    // A restricted number alerts the owners, once a day.
    h.state.email.clear_sent();
    let c = daytime_number(2003);
    let q = person(&mut h, &c, None).await;
    subscribe(&mut h, &q).await;
    answer(&m, &c, meta_error(131048));
    trigger(&mut h, "Joined", &c).await;
    assert_eq!(h.state.email.sent().iter().filter(|e| e.subject.contains("limited your WhatsApp")).count(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn channels_dont_mix() {
    let (mut h, _m) = wa_harness!(0.0);
    let acct = setup(&mut h).await;
    let them = daytime_number(2101);
    let p = person(&mut h, &them, Some("subscribed")).await;
    let flow_for = |channel: Option<&str>| {
        let mut w = json!({"id": "w", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": null, "no": null});
        if let Some(c) = channel {
            w["channel"] = json!(c);
        }
        json!({"entry": "w", "nodes": [w]})
    };
    let mut ids = Vec::new();
    for (name, channel) in [("Wait sms", None), ("Wait wa", Some("whatsapp"))] {
        let f = h.ok("POST", "/api/flows", Some(json!({"name": name, "trigger": {"type": "metric", "metric": name}, "definition": flow_for(channel)}))).await;
        let id = f["id"].as_str().unwrap().to_string();
        h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
        h.ok("POST", "/api/events", Some(json!({"metric": name, "profile": {"phone_number": them}}))).await;
        ids.push(id);
    }
    h.drain().await;
    // A WhatsApp reply: only the WhatsApp wait takes the "replied" path.
    hook(&h, &inbound(&them, "wamid.C1", "yes")).await;
    h.drain().await;
    let path = |id: &str| {
        let id = id.to_string();
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT status FROM flow_runs WHERE flow_id = $1::uuid").bind(id).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(path(&ids[0]).await, "active", "the texting wait keeps waiting");
    assert_eq!(path(&ids[1]).await, "completed", "the WhatsApp wait got its reply");
    let (s, _) = h.req("POST", "/api/flows", Some(json!({"name": "Bad", "trigger": {"type": "metric", "metric": "x"}, "definition": {"entry": "w", "nodes": [{"id": "w", "type": "wait_for_reply", "channel": "fax", "amount": 1, "unit": "days", "yes": null, "no": null}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Revenue by channel has its own WhatsApp line.
    let settings = dumbmail::features::reports::attribution::Settings::default();
    let att = dumbmail::features::reports::attribution::preview(&h.state, acct, &settings, 30).await.unwrap();
    assert!(att["current"]["totals"]["whatsapp_revenue"].is_number(), "{att}");
    let _ = p;
    h.cleanup().await;
}

#[tokio::test]
async fn template_sync_needs_settings_write_and_is_audited() {
    let (mut h, _m) = wa_harness!(0.0);
    setup(&mut h).await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let (s, _, _) = h.raw("POST", "/api/whatsapp/templates/sync", &[("authorization", &format!("Bearer {key}"))], "").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(count(&h, "SELECT count(*) FROM audit_log WHERE action = 'whatsapp.templates_sync'").await, 1, "the owner's sync in setup");
    h.cleanup().await;
}

#[tokio::test]
async fn a_form_box_naming_whatsapp_opts_in() {
    let (mut h, _m) = wa_harness!(0.0);
    let acct = setup(&mut h).await;
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let mut forms = Vec::new();
    for text in [Some("Also send me offers on WhatsApp."), None] {
        let mut config = json!({"fields": [{"key": "phone_number"}]});
        if let Some(t) = text {
            config["whatsapp_consent_text"] = json!(t);
        }
        let f = h.ok("POST", "/api/forms", Some(json!({"name": "Phone", "config": config}))).await;
        let id = f["id"].as_str().unwrap().to_string();
        h.ok("PATCH", &format!("/api/forms/{id}"), Some(json!({"status": "live"}))).await;
        forms.push(id);
    }
    let submit = |form: String, phone: String, wa: bool| {
        let router = h.router.clone();
        let path = format!("/client/subscribe?k={pk}");
        async move {
            let body = json!({"form_id": form, "phone_number": phone, "sms_consent": true, "whatsapp_consent": wa}).to_string();
            let req = axum::http::Request::builder().method("POST").uri(path).header("content-type", "application/json").body(axum::body::Body::from(body)).unwrap();
            tower::ServiceExt::oneshot(router, req).await.unwrap().status()
        }
    };
    let a = daytime_number(2201);
    let b = daytime_number(2202);
    let c = daytime_number(2203);
    assert!(submit(forms[0].clone(), a.clone(), true).await.is_success());
    assert!(submit(forms[0].clone(), b.clone(), false).await.is_success());
    assert!(submit(forms[1].clone(), c.clone(), true).await.is_success());
    assert_eq!(consent_of(&h, &a).await, "subscribed");
    assert_eq!(consent_of(&h, &b).await, "never_subscribed", "box not ticked");
    assert_eq!(consent_of(&h, &c).await, "never_subscribed", "a form that doesn't name WhatsApp");
    let source: String = sqlx::query_scalar("SELECT consent_source FROM whatsapp_contacts WHERE phone_number = $1").bind(&a).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(source, "form");
    // A public form can't undo a STOP.
    hook(&h, &inbound(&a, "wamid.F1", "STOP")).await;
    assert!(submit(forms[0].clone(), a.clone(), true).await.is_success());
    assert_eq!(consent_of(&h, &a).await, "unsubscribed");
    h.cleanup().await;
}

#[test]
fn content_is_checked_when_saved() {
    let _ = TEST_NUMBER;
    use dumbmail::features::whatsapp::content;
    assert!(content::parse(&json!({"template_id": null, "body": {"1": "{% coupon_code 'SPRING' %}"}})).unwrap_err().public_message().contains("coupon"));
    assert!(content::parse(&json!("text")).is_err());
}

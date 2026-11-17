//! Outgoing webhooks: delivery, signatures, topics, retries, account isolation, REST hooks and
//! signed flow webhook steps, against a local receiver.

#[macro_use]
mod common;
use common::*;

use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

#[derive(Clone, Debug)]
struct Hit {
    headers: axum::http::HeaderMap,
    body: Value,
    raw: String,
}

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<Hit>>>,
    /// Status codes to answer with, in order; 200 once they run out.
    statuses: Arc<Mutex<Vec<u16>>>,
}

impl Receiver {
    fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }
    fn answer(&self, codes: &[u16]) {
        *self.statuses.lock().unwrap() = codes.to_vec();
    }
    /// Every envelope received, batches flattened.
    fn envelopes(&self) -> Vec<Value> {
        self.hits().into_iter().flat_map(|h| if let Value::Array(a) = h.body { a } else { vec![h.body] }).collect()
    }
    fn topics(&self) -> Vec<String> {
        self.envelopes().iter().map(|e| e["topic"].as_str().unwrap_or("").to_string()).collect()
    }
}

async fn receiver() -> (Receiver, String) {
    dumbmail::features::webhooks::net::allow_localhost_for_tests();
    let r = Receiver::default();
    let app = axum::Router::new()
        .route(
            "/{*path}",
            axum::routing::any(|axum::extract::State(r): axum::extract::State<Receiver>, headers: axum::http::HeaderMap, body: String| async move {
                let code = {
                    let mut s = r.statuses.lock().unwrap();
                    if s.is_empty() { 200 } else { s.remove(0) }
                };
                r.hits.lock().unwrap().push(Hit { headers, body: serde_json::from_str(&body).unwrap_or(Value::Null), raw: body });
                (axum::http::StatusCode::from_u16(code).unwrap(), "ok")
            }),
        )
        .with_state(r.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (r, format!("http://{addr}"))
}

fn verify(secret: &str, hit: &Hit) -> bool {
    let ts = hit.headers["dumbmail-webhook-timestamp"].to_str().unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{ts}.{}", hit.raw).as_bytes());
    let want = format!("v1={}", hex::encode(mac.finalize().into_bytes()));
    hit.headers["dumbmail-webhook-signature"].to_str().unwrap().split(',').any(|s| s == want)
}

async fn account_id(h: &mut Harness) -> String {
    h.ok("GET", "/api/me", None).await["account"]["id"].as_str().unwrap().to_string()
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

/// Let an endpoint that's backing off be tried again now.
async fn retry_now(h: &Harness) {
    sqlx::query("UPDATE webhook_endpoints SET retry_at = NULL").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() WHERE status = 'pending'").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
}

#[tokio::test]
async fn events_reach_a_subscribed_endpoint_signed_without_any_flow() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    let acct = account_id(&mut h).await;

    // No endpoint: tracking an event queues no event job at all.
    h.ok("POST", "/api/events", Some(json!({"metric": "Viewed Product", "profile": {"email": "a@x.test"}}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'flow_event'").await, 0);
    h.drain().await;

    let ep = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "topics": ["event.tracked"], "metrics": ["Placed Order"], "batch_size": 1}))).await;
    let secret = ep["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("whsec_"));
    let id = ep["id"].as_str().unwrap().to_string();
    // The secret is shown once.
    let got = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    assert!(got.get("secret").is_none() && got["secret_hint"].as_str().unwrap().starts_with("whsec_…"), "{got}");
    let listed = h.ok("GET", "/api/webhooks", None).await;
    assert!(!listed.to_string().contains(&secret));

    // A metric it doesn't want: nothing queued at all. The one it wants: one outbox row and one
    // background fan-out job, never a flow job, and one signed request.
    h.ok("POST", "/api/events", Some(json!({"metric": "Viewed Product", "profile": {"email": "a@x.test"}}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox").await, 0);
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "a@x.test", "first_name": "Ann"}, "properties": {"total": 42}, "value": 42, "unique_id": "o-1"}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'flow_event'").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'webhooks_fanout' AND lane = 'background'").await, 1);
    h.drain().await;
    let hits = rx.hits();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let env = &hits[0].body;
    assert_eq!(env["topic"], "event.tracked");
    assert_eq!(env["account_id"], acct.as_str());
    assert_eq!(env["api_version"], "2026-11-17");
    assert_eq!(env["data"]["event"]["metric"], "Placed Order");
    assert_eq!(env["data"]["event"]["value"], 42.0);
    assert_eq!(env["data"]["profile"]["email"], "a@x.test");
    assert_eq!(env["source"], "api", "recorded through the API");
    assert!(env["id"].as_str().is_some());
    assert!(verify(&secret, &hits[0]), "signature doesn't verify");
    assert!(!verify("whsec_wrong", &hits[0]));
    assert_eq!(hits[0].headers["dumbmail-webhook-topic"], "event.tracked");

    let log = h.ok("GET", &format!("/api/webhooks/{id}/deliveries"), None).await;
    assert_eq!(log["total"], 1);
    assert_eq!(log["data"][0]["status"], "delivered");
    assert_eq!(log["data"][0]["response_code"], 200);
    let d = h.ok("GET", &format!("/api/webhooks/{id}/deliveries/{}", log["data"][0]["id"].as_str().unwrap()), None).await;
    assert_eq!(d["payload"]["id"], env["id"]);
    assert!(d.get("response_body").is_none(), "response bodies are never kept: {d}");
    h.cleanup().await;
}

#[tokio::test]
async fn profile_list_and_flow_topics() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    let topics = json!(["profile.created", "profile.updated", "profile.deleted", "profile.subscribed", "profile.unsubscribed", "list.member_added", "list.member_removed", "flow.status_changed", "import.finished"]);
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "topics": topics}))).await;

    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "b@x.test", "email_consent": "subscribed"}))).await;
    let pid = p["id"].as_str().unwrap().to_string();
    // Saving the same thing again changes nothing, so no profile.updated.
    h.ok("POST", "/api/profiles", Some(json!({"email": "b@x.test"}))).await;
    h.ok("PATCH", &format!("/api/profiles/{pid}"), Some(json!({"first_name": "Bea", "email_consent": "unsubscribed"}))).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIP"}))).await;
    let lid = list["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{lid}/members"), Some(json!({"profile_ids": [pid]}))).await;
    h.ok("DELETE", &format!("/api/lists/{lid}/members"), Some(json!({"profile_ids": [pid]}))).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "w", "nodes": [{"id": "w", "type": "delay", "amount": 1, "unit": "days", "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nc@x.test\n", "permission_confirmed": true}))).await;
    h.ok("DELETE", &format!("/api/profiles/{pid}"), None).await;
    h.drain().await;

    let envs = rx.envelopes();
    let topics = rx.topics();
    for t in ["profile.created", "profile.subscribed", "profile.updated", "profile.unsubscribed", "list.member_added", "list.member_removed", "flow.status_changed", "import.finished", "profile.deleted"] {
        assert!(topics.iter().any(|x| x == t), "missing {t} in {topics:?}");
    }
    let updated: Vec<&Value> = envs.iter().filter(|e| e["topic"] == "profile.updated" && e["data"]["profile"]["id"] == pid.as_str()).collect();
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert_eq!(updated[0]["data"]["changed"], json!(["first_name", "email_consent"]));
    let unsub = envs.iter().find(|e| e["topic"] == "profile.unsubscribed").unwrap();
    assert_eq!(unsub["data"]["channel"], "email");
    assert_eq!(unsub["data"]["reason"], "profile_update");
    assert_eq!((unsub["data"]["before"].as_str(), unsub["data"]["after"].as_str()), (Some("subscribed"), Some("unsubscribed")));
    let added = envs.iter().find(|e| e["topic"] == "list.member_added" && e["data"]["list"]["name"] == "VIP").unwrap();
    assert_eq!(added["data"]["profile"]["email"], "b@x.test");
    let flow_ev = envs.iter().find(|e| e["topic"] == "flow.status_changed").unwrap();
    assert_eq!((flow_ev["data"]["flow"]["previous_status"].as_str(), flow_ev["data"]["flow"]["status"].as_str()), (Some("draft"), Some("live")));
    let import = envs.iter().find(|e| e["topic"] == "import.finished").unwrap();
    assert_eq!(import["data"]["import"]["created"], 1);
    let deleted = envs.iter().find(|e| e["topic"] == "profile.deleted").unwrap();
    assert_eq!(deleted["data"]["profile"]["email"], "b@x.test");
    // Default batching: requests carry JSON arrays.
    assert!(rx.hits().iter().all(|h| h.body.is_array()));
    h.cleanup().await;
}

#[tokio::test]
async fn an_endpoint_only_gets_its_own_accounts_data() {
    let mut h = harness!();
    let (rx_a, base) = receiver().await;
    let (rx_b, base_b) = receiver().await;
    h.signup().await;
    let a_cookie = h.cookie.clone();
    let ep_a = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/a"), "topics": ["event.tracked", "profile.created"]}))).await;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base_b}/b"), "topics": ["event.tracked", "profile.created"]}))).await;
    // Account B can't see or change A's endpoint.
    let (s, _) = h.req("GET", &format!("/api/webhooks/{}", ep_a["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 404);
    let (s, _) = h.req("DELETE", &format!("/api/webhooks/{}", ep_a["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 404);
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "only-b@x.test"}}))).await;
    h.cookie = a_cookie;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "only-a@x.test"}}))).await;
    h.drain().await;
    let a = serde_json::to_string(&rx_a.envelopes()).unwrap();
    let b = serde_json::to_string(&rx_b.envelopes()).unwrap();
    assert!(a.contains("only-a@x.test") && !a.contains("only-b@x.test"), "{a}");
    assert!(b.contains("only-b@x.test") && !b.contains("only-a@x.test"), "{b}");
    h.cleanup().await;
}

#[tokio::test]
async fn failures_back_off_retry_and_turn_the_endpoint_off_after_three_days() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    let ep = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "topics": ["profile.created"], "batch_size": 1}))).await;
    let id = ep["id"].as_str().unwrap().to_string();

    rx.answer(&[500]);
    h.ok("POST", "/api/profiles", Some(json!({"email": "c1@x.test"}))).await;
    h.drain().await;
    let e = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    assert_eq!(e["status"], "failing", "{e}");
    assert!(e["next_retry_at"].is_string());
    let log = h.ok("GET", &format!("/api/webhooks/{id}/deliveries"), None).await;
    assert_eq!((log["data"][0]["status"].as_str(), log["data"][0]["attempts"].as_i64(), log["data"][0]["response_code"].as_i64()), (Some("pending"), Some(1), Some(500)));
    assert_eq!(log["data"][0]["error"], "http_error");
    // While it backs off, new events wait instead of hitting it again.
    h.ok("POST", "/api/profiles", Some(json!({"email": "c2@x.test"}))).await;
    h.fast_forward().await;
    assert_eq!(rx.hits().len(), 1);

    // It recovers: both go out, oldest first, with the same event id as the failed try.
    retry_now(&h).await;
    let hits = rx.hits();
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert_eq!(hits[1].body["id"], hits[0].body["id"]);
    assert_eq!(hits[1].body["data"]["profile"]["email"], "c1@x.test");
    assert_eq!(hits[2].body["data"]["profile"]["email"], "c2@x.test");
    assert_eq!(h.ok("GET", &format!("/api/webhooks/{id}"), None).await["status"], "active");

    // Past 24 hours a delivery is marked failed.
    rx.answer(&[503]);
    h.ok("POST", "/api/profiles", Some(json!({"email": "c3@x.test"}))).await;
    h.drain().await;
    sqlx::query("UPDATE webhook_deliveries SET expires_at = now() - interval '1 minute' WHERE status = 'pending'").execute(&h.state.db).await.unwrap();
    retry_now(&h).await;
    // The every-minute sweep fails it, whether or not anything else is sent.
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_deliveries WHERE status = 'failed'").await, 1);
    assert_eq!(count(&h, "SELECT pending::bigint FROM webhook_endpoints").await, 0);

    // Failing for three days turns it off and emails the account.
    rx.answer(&[500, 500]);
    h.ok("POST", "/api/profiles", Some(json!({"email": "c4@x.test"}))).await;
    h.drain().await;
    sqlx::query("UPDATE webhook_endpoints SET failing_since = now() - interval '4 days'").execute(&h.state.db).await.unwrap();
    retry_now(&h).await;
    let e = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    assert_eq!(e["status"], "disabled", "{e}");
    assert!(e["disabled_reason"].as_str().unwrap().contains("3 days"));
    let sent = h.state.email.sent();
    assert!(sent.iter().any(|m| m.subject == "A webhook endpoint was turned off" && m.to == "owner@acme.test"), "{:?}", sent.iter().map(|m| &m.subject).collect::<Vec<_>>());
    // Nothing more is queued for it while it's off.
    let before = rx.hits().len();
    h.ok("POST", "/api/profiles", Some(json!({"email": "c5@x.test"}))).await;
    h.fast_forward().await;
    assert_eq!(rx.hits().len(), before);

    // Turned back on: what was still waiting (c4, inside its 24 hours) goes out, and a resend
    // sends the one that had failed (c3).
    h.ok("PATCH", &format!("/api/webhooks/{id}"), Some(json!({"enabled": true}))).await;
    let r = h.ok("POST", &format!("/api/webhooks/{id}/resend"), Some(json!({"since": "2020-01-01T00:00:00Z"}))).await;
    assert_eq!(r["resent"], 1, "{r}");
    // One resend a minute: it's the claim, so a second one now is refused.
    let (s, _) = h.req("POST", &format!("/api/webhooks/{id}/resend"), Some(json!({"since": "2020-01-01T00:00:00Z"}))).await;
    assert_eq!(s, 429);
    h.drain().await;
    let emails: Vec<String> = rx.hits()[before..].iter().map(|x| x.body["data"]["profile"]["email"].as_str().unwrap().to_string()).collect();
    assert_eq!(emails, vec!["c3@x.test", "c4@x.test"]);
    h.cleanup().await;
}

#[tokio::test]
async fn batches_tests_rotation_filters_and_limits() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    // Only https, and never private addresses.
    let (s, v) = h.req("POST", "/api/webhooks", Some(json!({"url": "http://example.com/hook", "topics": ["profile.created"]}))).await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = h.req("POST", "/api/webhooks", Some(json!({"url": "https://10.1.2.3/hook", "topics": ["profile.created"]}))).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("public"));
    let (s, _) = h.req("POST", "/api/webhooks", Some(json!({"url": format!("{base}/x"), "topics": ["nope"]}))).await;
    assert_eq!(s, 400);
    let (s, _) = h.req("POST", "/api/webhooks", Some(json!({"url": format!("{base}/x"), "topics": ["profile.created"], "batch_size": 1000}))).await;
    assert_eq!(s, 400);

    let ep = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/batch"), "topics": ["event.tracked"], "batch_size": 3,
        "filter": [{"field": "event.value", "op": "gte", "value": 10}]}))).await;
    let id = ep["id"].as_str().unwrap().to_string();
    for v in [5, 10, 20, 30, 40, 50] {
        h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "value": v, "profile": {"email": "d@x.test"}}))).await;
    }
    h.drain().await;
    let hits = rx.hits();
    assert_eq!(hits.iter().map(|h| h.body.as_array().unwrap().len()).collect::<Vec<_>>(), vec![3, 2], "{hits:?}");
    assert!(rx.envelopes().iter().all(|e| e["data"]["event"]["value"].as_f64().unwrap() >= 10.0));

    // Rotation: the old secret keeps signing alongside the new one for a day.
    let old = ep["secret"].as_str().unwrap().to_string();
    let rot = h.ok("POST", &format!("/api/webhooks/{id}/rotate-secret"), None).await;
    let new = rot["secret"].as_str().unwrap().to_string();
    assert_ne!(old, new);
    // A test goes out on the endpoint's one-at-a-time lease: while a delivery holds it, it waits.
    sqlx::query("UPDATE webhook_endpoints SET lease_until = now() + interval '1 minute'").execute(&h.state.db).await.unwrap();
    let (s, _) = h.req("POST", &format!("/api/webhooks/{id}/test"), Some(json!({"topic": "message.opened"}))).await;
    assert_eq!(s, 429);
    sqlx::query("UPDATE webhook_endpoints SET lease_until = NULL").execute(&h.state.db).await.unwrap();
    let t = h.ok("POST", &format!("/api/webhooks/{id}/test"), Some(json!({"topic": "message.opened"}))).await;
    assert_eq!(t["ok"], true, "{t}");
    assert_eq!(t["delivery"]["test"], true);
    let last = rx.hits().pop().unwrap();
    assert_eq!(last.body[0]["test"], true);
    assert_eq!(last.body[0]["topic"], "message.opened");
    assert!(verify(&old, &last) && verify(&new, &last));
    assert_eq!(last.headers["dumbmail-webhook-signature"].to_str().unwrap().matches("v1=").count(), 2);

    // Topics are listed with descriptions; the sample endpoint gives the same shape.
    let topics = h.ok("GET", "/api/webhooks/topics", None).await;
    assert!(topics["data"].as_array().unwrap().len() >= 20);
    h.cleanup().await;
}

#[tokio::test]
async fn rest_hooks_for_zapier_and_make() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Zapier"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let hdrs = [("authorization", auth.as_str()), ("content-type", "application/json")];

    // The connection test.
    let (s, _, body) = h.raw("GET", "/api/hooks/me", &hdrs, "").await;
    assert_eq!(s, 200);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["account"]["name"], "Acme Goods");
    let (s, _, _) = h.raw("GET", "/api/hooks/me", &[], "").await;
    assert_eq!(s, 401);

    // Subscribe (as Zapier does), get a sample, receive one object per event.
    let (s, _, body) = h.raw("POST", "/api/hooks", &hdrs, &json!({"hookUrl": format!("{base}/zap"), "topic": "event.tracked", "metric": "Placed Order", "source": "zapier"}).to_string()).await;
    assert_eq!(s, 201, "{body}");
    let hook: Value = serde_json::from_str(&body).unwrap();
    let hook_id = hook["id"].as_str().unwrap().to_string();
    let (s, _, body) = h.raw("GET", "/api/hooks/samples?topic=event.tracked&metric=Placed%20Order", &hdrs, "").await;
    assert_eq!(s, 200);
    let sample: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(sample[0]["topic"], "event.tracked");
    assert_eq!(sample[0]["data"]["event"]["metric"], "Placed Order");
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "value": 9, "profile": {"email": "z@x.test"}}))).await;
    h.drain().await;
    let hits = rx.hits();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.is_object(), "REST hooks get one object per request");
    // With real events, the sample is the latest real one.
    let (_, _, body) = h.raw("GET", "/api/hooks/samples?topic=event.tracked&metric=Placed%20Order", &hdrs, "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()[0]["data"]["profile"]["email"], "z@x.test");
    let listed = h.ok("GET", "/api/webhooks", None).await;
    assert_eq!(listed["data"][0]["source"], "zapier");

    // Unsubscribe.
    let (s, _, _) = h.raw("DELETE", &format!("/api/hooks/{hook_id}"), &hdrs, "").await;
    assert_eq!(s, 200);
    assert_eq!(h.ok("GET", "/api/webhooks", None).await["total"], 0);

    // A hook answering 410 Gone is removed.
    let (_, _, body) = h.raw("POST", "/api/hooks", &hdrs, &json!({"url": format!("{base}/make"), "topic": "profile.created", "source": "make"}).to_string()).await;
    assert!(body.contains("\"id\""), "{body}");
    rx.answer(&[410]);
    h.ok("POST", "/api/profiles", Some(json!({"email": "gone@x.test"}))).await;
    h.drain().await;
    assert_eq!(h.ok("GET", "/api/webhooks", None).await["total"], 0);
    h.cleanup().await;
}

#[tokio::test]
async fn flow_webhook_steps_are_signed_with_the_account_secret() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    let info = h.ok("GET", "/api/webhooks/flow-secret", None).await;
    assert!(info["secret_hint"].is_null());
    let secret = h.ok("POST", "/api/webhooks/flow-secret/rotate", None).await["secret"].as_str().unwrap().to_string();
    assert!(h.ok("GET", "/api/webhooks/flow-secret", None).await["secret_hint"].as_str().unwrap().starts_with("whsec_…"));
    let flow = h
        .ok("POST", "/api/flows", Some(json!({"name": "Hook", "trigger": {"type": "metric", "metric": "Placed Order"},
            "definition": {"entry": "h", "nodes": [{"id": "h", "type": "webhook", "url": format!("{base}/flow"), "body": {"x": 1}, "next": null}]}})))
        .await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "f@x.test"}}))).await;
    h.fast_forward().await;
    let hits = rx.hits();
    assert_eq!(hits.len(), 1, "{hits:?}");
    // The body is what flows always sent, now with a signature.
    assert_eq!(hits[0].body["body"], json!({"x": 1}));
    assert_eq!(hits[0].body["profile"]["email"], "f@x.test");
    assert!(verify(&secret, &hits[0]));
    h.cleanup().await;
}

#[tokio::test]
async fn mcp_tools_for_webhooks() {
    let mut h = harness!();
    let (_rx, base) = receiver().await;
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let call = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let hdrs = [("authorization", auth.as_str()), ("content-type", "application/json")];
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string()).await;
    for t in ["create_webhook", "list_webhook_deliveries", "resend_webhook_deliveries", "rotate_flow_webhook_secret"] {
        assert!(body.contains(t), "{t} missing");
    }
    // Every tool name is used once (two features defining one name would hide one of them).
    let list: Value = serde_json::from_str(&body).unwrap();
    let mut names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    let n = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), n, "duplicate MCP tool names");
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &call("create_webhook", json!({"url": format!("{base}/m"), "topics": ["profile.created"]}))).await;
    let res: Value = serde_json::from_str(&body).unwrap();
    let created = &res["result"]["structuredContent"];
    assert!(created["secret"].as_str().unwrap().starts_with("whsec_"), "{res}");
    let id = created["id"].as_str().unwrap();
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &call("delete_webhook", json!({"id": id}))).await;
    assert!(body.contains("dry_run"));
    assert_eq!(h.ok("GET", "/api/webhooks", None).await["total"], 1);
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &call("delete_webhook", json!({"id": id, "confirm": true}))).await;
    assert!(body.contains("deleted"));
    assert_eq!(h.ok("GET", "/api/webhooks", None).await["total"], 0);
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/webhooks"}}).to_string()).await;
    assert!(body.contains("DumbMail-Webhook-Signature"));
    h.cleanup().await;
}

#[tokio::test]
async fn message_segment_and_form_topics() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    h.credit(10.0).await;
    let me = h.ok("GET", "/api/me", None).await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "batch_size": 1, "topics": [
        "message.sent", "message.delivered", "message.opened", "message.complained", "profile.suppressed", "profile.unsubscribed",
        "campaign.sent", "campaign.finished", "segment.entered", "segment.left", "form.submitted"]}))).await;

    // A campaign: sent, delivered, opened, then a spam complaint.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "m@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Launch", "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let (mid, pid): (uuid::Uuid, String) = sqlx::query_as("SELECT id, provider_message_id FROM messages WHERE to_address = 'm@x.test' AND NOT is_test").fetch_one(&h.state.db).await.unwrap();
    let ses = |m: Value| json!({"Type": "Notification", "Message": m.to_string()}).to_string();
    h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &ses(json!({"eventType": "Delivery", "mail": {"messageId": pid}}))).await;
    let html = h.state.email.sent().into_iter().find(|m| m.to == "m@x.test").unwrap().html;
    let open = find_link(&html, "/t/o/");
    let (s, _, _) = h.raw("GET", open.trim_start_matches("https://pl.test"), &[], "").await;
    assert_eq!(s, 200);
    h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &ses(json!({"notificationType": "Complaint", "mail": {"messageId": pid}, "complaint": {}}))).await;

    // A segment: the first refresh only takes a snapshot; then someone enters and leaves.
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "VIPs", "definition": {"match": "all", "conditions": [{"type": "property", "field": "vip", "op": "eq", "value": true}]}}))).await;
    let seg_id: uuid::Uuid = seg["id"].as_str().unwrap().parse().unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    let v = h.ok("POST", "/api/profiles", Some(json!({"email": "vip@x.test", "properties": {"vip": true}}))).await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    h.ok("PATCH", &format!("/api/profiles/{}", v["id"].as_str().unwrap()), Some(json!({"properties": {"vip": false}}))).await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();

    // A form sign-up.
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "kind": "popup", "list_id": list["id"], "config": {}, "status": "live"}))).await;
    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": form["id"], "email": "fan@x.test"}).to_string()).await;
    assert_eq!(s, 200, "{body}");
    h.drain().await;

    let envs = rx.envelopes();
    let topics = rx.topics();
    for t in ["campaign.sent", "message.sent", "campaign.finished", "message.delivered", "message.opened", "message.complained", "profile.suppressed", "profile.unsubscribed", "segment.entered", "segment.left", "form.submitted"] {
        assert_eq!(topics.iter().filter(|x| *x == t).count(), 1, "{t} in {topics:?}");
    }
    let by = |t: &str| envs.iter().find(|e| e["topic"] == t).unwrap().clone();
    assert_eq!(by("message.opened")["data"]["message"]["id"], mid.to_string());
    assert_eq!(by("message.delivered")["data"]["message"]["campaign_id"], c["id"]);
    assert_eq!(by("profile.suppressed")["data"]["reason"], "complaint");
    assert_eq!(by("profile.unsubscribed")["data"]["reason"], "spam_complaint");
    assert_eq!(by("campaign.finished")["data"]["campaign"]["recipients"], 1);
    assert_eq!(by("segment.entered")["data"]["profile"]["email"], "vip@x.test");
    assert_eq!(by("segment.left")["data"]["segment"]["name"], "VIPs");
    let f = by("form.submitted");
    assert_eq!((f["data"]["form"]["name"].as_str(), f["data"]["profile"]["email"].as_str()), (Some("Popup"), Some("fan@x.test")));
    h.cleanup().await;
}

/// A list of `n` subscribed profiles, imported. Returns its id.
async fn email_list(h: &mut Harness, prefix: &str, n: usize) -> String {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": prefix}))).await;
    let mut csv = String::from("email\n");
    for i in 0..n {
        csv.push_str(&format!("{prefix}{i}@x.test\n"));
    }
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    list["id"].as_str().unwrap().to_string()
}

async fn send_campaign(h: &mut Harness, list_id: &str) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Launch", "channel": "email", "audience": {"include": [{"type": "list", "id": list_id}]},
        "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    let id = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{id}/send"), None).await;
    id
}

async fn ses(h: &Harness, msg: Value) -> axum::http::StatusCode {
    let sns = json!({"Type": "Notification", "Message": msg.to_string()});
    h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &sns.to_string()).await.0
}

#[tokio::test]
async fn a_broken_outbox_never_breaks_sending_unsubscribes_or_provider_callbacks() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    h.credit(10.0).await;
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "topics": ["event.tracked", "message.sent", "message.delivered", "message.bounced",
        "profile.unsubscribed", "profile.suppressed", "campaign.sent", "campaign.finished", "segment.entered", "segment.left", "list.member_added"]}))).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Everyone", "definition": {"match": "all", "conditions": [{"type": "property", "field": "x", "op": "eq", "value": 1}]}}))).await;
    let seg_id: uuid::Uuid = seg["id"].as_str().unwrap().parse().unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    let list = email_list(&mut h, "obs", 3).await;
    h.drain().await;
    let before = rx.hits().len();

    // From here on every write to the outbox fails.
    sqlx::query("ALTER TABLE webhook_outbox ADD CONSTRAINT outbox_down CHECK (false) NOT VALID").execute(&h.state.db).await.unwrap();

    // A campaign still sends in full and finishes; nothing is marked failed.
    let cid = send_campaign(&mut h, &list).await;
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sent", "{c}");
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE status = 'sent' AND NOT is_test").await, 3);
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE status = 'failed'").await, 0);

    // An unsubscribe link still unsubscribes.
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "obs0@x.test").unwrap();
    let unsub = mail.unsubscribe_url.clone().unwrap();
    let (s, _, body) = h.raw("POST", unsub.trim_start_matches("https://pl.test"), &[("content-type", "application/x-www-form-urlencoded")], "").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'obs0@x.test' AND email_consent = 'unsubscribed'").await, 1);

    // Provider callbacks still land: a delivery and a hard bounce.
    let ids: Vec<String> = sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE NOT is_test ORDER BY to_address").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(ses(&h, json!({"eventType": "Delivery", "mail": {"messageId": ids[1]}})).await, 200);
    assert_eq!(ses(&h, json!({"notificationType": "Bounce", "mail": {"messageId": ids[2]}, "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}})).await, 200);
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE status = 'delivered'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE status = 'bounced'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email_suppressed_reason = 'hard_bounce'").await, 1);

    // A segment refresh (its webhook rows go in its own transaction) still commits.
    sqlx::query("UPDATE profiles SET properties = '{\"x\": 1}'").execute(&h.state.db).await.unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    assert_eq!(count(&h, "SELECT member_count::bigint FROM segments").await, 3);
    // So does adding people to a list, and tracking an event.
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "late@x.test", "list_ids": [list]}))).await;
    assert!(p["id"].is_string());
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "late@x.test"}}))).await;
    h.drain().await;
    assert_eq!(rx.hits().len(), before, "nothing could be queued, and nothing else broke");

    // Once the outbox works again, so do webhooks.
    sqlx::query("ALTER TABLE webhook_outbox DROP CONSTRAINT outbox_down").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "late@x.test"}}))).await;
    h.drain().await;
    assert!(rx.topics().iter().any(|t| t == "event.tracked"));
    h.cleanup().await;
}

#[tokio::test]
async fn a_big_send_with_a_subscriber_makes_no_job_per_message_and_stays_off_the_main_workers() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    h.credit(10.0).await;
    let list = email_list(&mut h, "big", 250).await;
    h.drain().await;
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "topics": ["message.sent"], "batch_size": 100}))).await;
    let cid = send_campaign(&mut h, &list).await;

    // The main workers send the whole campaign without touching webhook work.
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::MAIN).await.unwrap();
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE status = 'sent' AND NOT is_test").await, 250);
    assert!(rx.hits().is_empty(), "the main workers sent no webhooks");
    // No job per message: no flow job (no flow wants these events), one fan-out job in the background lane.
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'flow_event'").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind LIKE 'webhooks_%' AND lane = 'main'").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind LIKE 'webhooks_%'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox WHERE topic = 'message.sent'").await, 250);

    // The background workers deliver them, 100 to a request.
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::BACKGROUND).await.unwrap();
    assert_eq!(rx.envelopes().len(), 250);
    assert_eq!(rx.hits().len(), 3);
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox").await, 0);
    assert_eq!(count(&h, "SELECT pending::bigint FROM webhook_endpoints").await, 0);
    let ids: std::collections::HashSet<String> = rx.envelopes().iter().map(|e| e["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 250, "every event has its own id");
    assert!(rx.envelopes().iter().all(|e| e["source"] == "system" && e["data"]["message"]["campaign_id"] == cid.as_str()));
    h.cleanup().await;
}

#[tokio::test]
async fn changing_the_url_cancels_what_was_waiting_for_the_old_one() {
    let mut h = harness!();
    let (old_rx, old_base) = receiver().await;
    let (new_rx, new_base) = receiver().await;
    h.signup().await;
    let ep = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{old_base}/hook"), "topics": ["profile.created"], "batch_size": 1}))).await;
    let id = ep["id"].as_str().unwrap().to_string();
    old_rx.answer(&[500]);
    h.ok("POST", "/api/profiles", Some(json!({"email": "q1@x.test"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "q2@x.test"}))).await;
    h.drain().await;
    assert_eq!(old_rx.hits().len(), 1);
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_deliveries WHERE status = 'pending'").await, 2);

    // A new receiver: what was queued for the old URL is cancelled, never sent to the new one.
    let e = h.ok("PATCH", &format!("/api/webhooks/{id}"), Some(json!({"url": format!("{new_base}/hook")}))).await;
    assert_eq!(e["revision"], 2);
    assert_eq!(e["stats"]["pending"], 0);
    let log = h.ok("GET", &format!("/api/webhooks/{id}/deliveries?status=cancelled"), None).await;
    assert_eq!(log["total"], 2, "{log}");
    assert_eq!(log["data"][0]["error"], "url_changed");
    retry_now(&h).await;
    assert!(new_rx.hits().is_empty(), "old deliveries went to the new URL: {:?}", new_rx.hits());
    // Nor can a resend send them there.
    let r = h.ok("POST", &format!("/api/webhooks/{id}/resend"), Some(json!({"since": "2020-01-01T00:00:00Z", "status": "all"}))).await;
    assert_eq!(r["resent"], 0, "{r}");

    // New events go to the new URL.
    h.ok("POST", "/api/profiles", Some(json!({"email": "q3@x.test"}))).await;
    h.drain().await;
    let got: Vec<String> = new_rx.envelopes().iter().map(|e| e["data"]["profile"]["email"].as_str().unwrap().to_string()).collect();
    assert_eq!(got, vec!["q3@x.test"]);
    assert_eq!(old_rx.hits().len(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn every_event_says_who_sent_it_and_browser_ones_can_be_left_out() {
    let mut h = harness!();
    let (all_rx, all_base) = receiver().await;
    let (trusted_rx, trusted_base) = receiver().await;
    h.signup().await;
    let pk = h.ok("GET", "/api/me", None).await["account"]["public_key"].as_str().unwrap().to_string();
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{all_base}/all"), "topics": ["event.tracked", "message.sent"]}))).await;
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{trusted_base}/t"), "topics": ["event.tracked"], "trusted_only": true}))).await;

    // A browser with the public key can send any non-reserved metric, including "Placed Order".
    let (s, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "value": 999, "profile": {"email": "forger@x.test"}}).to_string()).await;
    assert_eq!(s, 200, "{b}");
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "value": 20, "profile": {"email": "real@x.test"}}))).await;
    // An API client may record a message metric (imports do), but that's not a message DumbMail sent.
    h.ok("POST", "/api/events", Some(json!({"metric": "Received Email", "profile": {"email": "real@x.test"}}))).await;
    h.drain().await;

    let all = all_rx.envelopes();
    let src = |rx: &[Value], email: &str| rx.iter().find(|e| e["data"]["profile"]["email"] == email && e["data"]["event"]["metric"] == "Placed Order").map(|e| e["source"].clone());
    assert_eq!(src(&all, "forger@x.test"), Some(json!("browser")));
    assert_eq!(src(&all, "real@x.test"), Some(json!("api")));
    assert!(!all.iter().any(|e| e["topic"] == "message.sent"), "a recorded metric doesn't make a message topic");
    let trusted = trusted_rx.envelopes();
    assert_eq!(src(&trusted, "forger@x.test"), None, "trusted_only leaves browser events out");
    assert_eq!(src(&trusted, "real@x.test"), Some(json!("api")));
    h.cleanup().await;
}

#[tokio::test]
async fn consent_changes_carry_before_and_after_and_repeat_callbacks_are_told_once() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    h.credit(10.0).await;
    let list = email_list(&mut h, "rep", 1).await;
    h.drain().await;
    h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "batch_size": 1, "topics": ["message.delivered", "profile.unsubscribed", "profile.suppressed"]}))).await;
    send_campaign(&mut h, &list).await;
    h.drain().await;
    let pid: String = sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE NOT is_test").fetch_one(&h.state.db).await.unwrap();
    // SNS repeats notifications; a repeat isn't a new delivery.
    for _ in 0..3 {
        assert_eq!(ses(&h, json!({"eventType": "Delivery", "mail": {"messageId": pid}})).await, 200);
    }
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "rep0@x.test").unwrap();
    let unsub = mail.unsubscribe_url.clone().unwrap();
    for _ in 0..2 {
        h.raw("POST", unsub.trim_start_matches("https://pl.test"), &[("content-type", "application/x-www-form-urlencoded")], "").await;
    }
    h.drain().await;
    let topics = rx.topics();
    assert_eq!(topics.iter().filter(|t| *t == "message.delivered").count(), 1, "{topics:?}");
    assert_eq!(topics.iter().filter(|t| *t == "profile.unsubscribed").count(), 1, "{topics:?}");
    let u = rx.envelopes().into_iter().find(|e| e["topic"] == "profile.unsubscribed").unwrap();
    assert_eq!((u["data"]["before"].as_str(), u["data"]["after"].as_str(), u["data"]["reason"].as_str()), (Some("subscribed"), Some("unsubscribed"), Some("unsubscribe_link")));
    assert_eq!(u["data"]["profile"]["email"], "rep0@x.test");
    // A bounce on an address already unsubscribed isn't reported as an unsubscribe; it is a suppression.
    assert_eq!(ses(&h, json!({"notificationType": "Bounce", "mail": {"messageId": pid}, "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}})).await, 200);
    assert_eq!(ses(&h, json!({"notificationType": "Bounce", "mail": {"messageId": pid}, "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}})).await, 200);
    h.drain().await;
    let topics = rx.topics();
    assert_eq!(topics.iter().filter(|t| *t == "profile.suppressed").count(), 1, "{topics:?}");
    assert_eq!(topics.iter().filter(|t| *t == "profile.unsubscribed").count(), 1, "{topics:?}");
    h.cleanup().await;
}

#[tokio::test]
async fn rest_hooks_belong_to_the_key_that_made_them() {
    let mut h = harness!();
    let (_rx, base) = receiver().await;
    h.signup().await;
    let key_a = h.ok("POST", "/api/api-keys", Some(json!({"name": "Zapier"}))).await["key"].as_str().unwrap().to_string();
    let key_b = h.ok("POST", "/api/api-keys", Some(json!({"name": "Make"}))).await["key"].as_str().unwrap().to_string();
    let (auth_a, auth_b) = (format!("Bearer {key_a}"), format!("Bearer {key_b}"));
    let a = [("authorization", auth_a.as_str()), ("content-type", "application/json")];
    let b = [("authorization", auth_b.as_str()), ("content-type", "application/json")];
    let dash = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/dash"), "topics": ["profile.created"]}))).await;
    let (s, _, body) = h.raw("POST", "/api/hooks", &a, &json!({"hookUrl": format!("{base}/zap"), "topic": "profile.created", "source": "zapier"}).to_string()).await;
    assert_eq!(s, 201, "{body}");
    let hook: Value = serde_json::from_str(&body).unwrap();

    // Each key sees and deletes only its own hooks; never a dashboard endpoint.
    let (_, _, body) = h.raw("GET", "/api/hooks", &a, "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["total"], 1);
    let (_, _, body) = h.raw("GET", "/api/hooks", &b, "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["total"], 0);
    let (s, _, _) = h.raw("DELETE", &format!("/api/hooks/{}", hook["id"].as_str().unwrap()), &b, "").await;
    assert_eq!(s, 404);
    let (s, _, _) = h.raw("DELETE", &format!("/api/hooks/{}", dash["id"].as_str().unwrap()), &a, "").await;
    assert_eq!(s, 404);
    assert_eq!(h.ok("GET", "/api/webhooks", None).await["total"], 2);
    // REST hooks are for API keys, not dashboard sessions.
    let (s, _) = h.req("POST", "/api/hooks", Some(json!({"hookUrl": format!("{base}/x"), "topic": "profile.created"}))).await;
    assert_eq!(s, 403);
    // Revoking the key removes its hooks.
    let key_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM api_keys WHERE name = 'Zapier'").fetch_one(&h.state.db).await.unwrap();
    h.ok("DELETE", &format!("/api/api-keys/{key_id}"), None).await;
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    assert_eq!(h.ok("GET", "/api/webhooks", None).await["total"], 1);
    h.cleanup().await;
}

#[tokio::test]
async fn a_failing_endpoint_is_turned_off_after_three_days_even_with_no_more_traffic() {
    let mut h = harness!();
    let (rx, base) = receiver().await;
    h.signup().await;
    let ep = h.ok("POST", "/api/webhooks", Some(json!({"url": format!("{base}/hook"), "topics": ["profile.created"], "batch_size": 1}))).await;
    let id = ep["id"].as_str().unwrap().to_string();
    rx.answer(&[500]);
    h.ok("POST", "/api/profiles", Some(json!({"email": "once@x.test"}))).await;
    h.drain().await;
    // Its last delivery expired long ago and nothing else is sent; the sweep still turns it off.
    sqlx::query("UPDATE webhook_deliveries SET expires_at = now() - interval '1 day'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE webhook_endpoints SET failing_since = now() - interval '4 days', retry_at = NULL").execute(&h.state.db).await.unwrap();
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    let e = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    assert_eq!(e["status"], "disabled", "{e}");
    let alert = h.state.email.sent().into_iter().find(|m| m.subject == "A webhook endpoint was turned off").unwrap();
    // The alert names the endpoint by host, never by its full URL (which can be a credential).
    assert!(!alert.text.contains("/hook") && alert.text.contains("127.0.0.1"), "{}", alert.text);
    assert_eq!(alert.from_email, "help@pl.test");
    assert_eq!(alert.to, "owner@acme.test");
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_deliveries WHERE status = 'failed' AND error = 'expired'").await, 1);
    h.cleanup().await;
}

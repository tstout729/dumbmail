//! Transactional email API end to end: sending, consent and suppression, tracking, idempotency
//! under retries, abuse limits, the automatic pause, billing, account isolation and MCP; who may use
//! it (off by default, restricted and full mode); delivery bound to the requested mailbox; exactly
//! one delivery and one charge across busy retries, crashes and concurrent attempts; no secrets kept.

#[macro_use]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dumbmail::features::transactional::send as tsend;
use dumbmail::jobs;
use dumbmail::providers::email::{DomainState, EmailProvider, LogEmail, OutgoingEmail, ProviderBusy};
use dumbmail::state::{AppState, Inner};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::*;

const ADMIN: (&str, &str) = ("authorization", "Bearer test-admin-token-0123456789abcdef0123");

/// POST with an API key (how stores call the transactional API).
async fn api(h: &Harness, key: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    call(&h.router, key, method, path, body).await
}

async fn call(router: &axum::Router, key: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {key}")).header("content-type", "application/json");
    let req = b.body(body.map_or(Body::empty(), |v| Body::from(v.to_string()))).unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let s = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn account_id(h: &mut Harness) -> String {
    h.ok("GET", "/api/me", None).await["account"]["id"].as_str().unwrap().to_string()
}

/// The owner turns transactional email on; `full` also has the operator grant full mode.
async fn enable(h: &mut Harness, full: bool) {
    let v = h.ok("PATCH", "/api/transactional/settings", Some(json!({"enabled": true, "acknowledge": true}))).await;
    assert_eq!(v["enabled"], true, "{v}");
    if full {
        let id = account_id(h).await;
        let (s, _, body) = h.raw("POST", "/admin/transactional/mode", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": id, "mode": "full"}).to_string()).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
}

/// Signed up, an API key, and transactional email on in full mode (most tests send inline content).
async fn setup(h: &mut Harness) -> String {
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "store"}))).await["key"].as_str().unwrap().to_string();
    enable(h, true).await;
    key
}

/// An email provider that says it's busy for the next `busy` sends, then accepts (like LogEmail).
struct Flaky {
    inner: LogEmail,
    busy: AtomicUsize,
}

#[async_trait::async_trait]
impl EmailProvider for Flaky {
    fn name(&self) -> &'static str {
        "log"
    }
    async fn send(&self, email: &OutgoingEmail) -> anyhow::Result<String> {
        let mut n = self.busy.load(Ordering::SeqCst);
        let busy = loop {
            if n == 0 {
                break false;
            }
            match self.busy.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break true,
                Err(now) => n = now,
            }
        };
        if busy {
            return Err(anyhow::Error::new(ProviderBusy { retry_after: std::time::Duration::from_secs(1), reason: "test busy".into() }));
        }
        self.inner.send(email).await
    }
    async fn add_domain(&self, domain: &str) -> anyhow::Result<DomainState> {
        self.inner.add_domain(domain).await
    }
    async fn domain_state(&self, domain: &str) -> anyhow::Result<DomainState> {
        self.inner.domain_state(domain).await
    }
    fn sent(&self) -> Vec<OutgoingEmail> {
        self.inner.sent()
    }
}

/// The same app on the same database, with `busy` busy answers from the email provider first.
fn flaky(h: &Harness, busy: usize) -> (AppState, axum::Router, Arc<Flaky>) {
    let provider = Arc::new(Flaky { inner: LogEmail::new(), busy: AtomicUsize::new(busy) });
    let s = &h.state;
    let st = AppState(Arc::new(Inner {
        db: s.db.clone(),
        cfg: s.cfg.clone(),
        email: provider.clone(),
        sms: s.sms.clone(),
        http: s.http.clone(),
        pending_checks: Default::default(),
        daily_limits: Default::default(),
    }));
    (st.clone(), dumbmail::app(st), provider)
}

/// Make every waiting transactional retry due and run the queue on `state`.
async fn run_retries(state: &AppState) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'transactional_retry' AND run_at <> 'infinity'").execute(&state.db).await.unwrap();
    jobs::drain(state).await.unwrap();
}

async fn row(h: &Harness, id: &str) -> (String, i32, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status, attempts, payload_enc, error FROM transactional_sends WHERE id = $1::uuid").bind(id).fetch_one(&h.state.db).await.unwrap()
}

fn receipt(to: Value) -> Value {
    json!({"to": to, "content": {"subject": "Order {{ order_id }} confirmed", "blocks": [
        {"type": "text", "html": "<p>Thanks {{ first_name|default('there') }}! Total {{ total }}.</p>"},
        {"type": "button", "text": "View order", "href": "https://acme.test/orders/{{ order_id }}"}]},
        "variables": {"order_id": "1042", "total": "$36.00"}})
}

fn sent_to(h: &Harness, to: &str) -> Vec<dumbmail::providers::email::OutgoingEmail> {
    h.state.email.sent().into_iter().filter(|m| m.to == to).collect()
}

async fn ses(h: &Harness, provider_id: &str, msg: Value) {
    let mut msg = msg;
    msg["mail"] = json!({"messageId": provider_id});
    let sns = json!({"Type": "Notification", "Message": msg.to_string()});
    let (s, _, body) = h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &sns.to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

async fn provider_id(h: &Harness, send: &Value) -> String {
    sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE id = $1::uuid").bind(send["message_id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn sends_receipts_without_marketing_consent_but_never_to_bounced_addresses() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    // Unsubscribed from marketing: still gets the receipt they triggered.
    h.ok("POST", "/api/profiles", Some(json!({"email": "ann@x.test", "first_name": "Ann", "email_consent": "unsubscribed"}))).await;
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("ann@x.test")))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["status"].as_str(), v["replayed"].as_bool()), (Some("sent"), Some(false)), "{v}");
    let mail = sent_to(&h, "ann@x.test");
    assert_eq!(mail.len(), 1);
    let m = &mail[0];
    assert_eq!(m.subject, "Order 1042 confirmed");
    assert!(m.html.contains("Thanks Ann! Total $36.00."));
    assert!(!m.html.to_lowercase().contains("unsubscribe") && !m.text.to_lowercase().contains("unsubscribe"), "no marketing footer");
    assert!(m.unsubscribe_url.is_none(), "no List-Unsubscribe header");
    assert!(!m.html.contains("1 Main St"), "no address unless asked");
    assert!(m.html.contains("href=\"https://acme.test/orders/1042\""), "clicks aren't tracked by default");
    assert!(m.html.contains("/t/o/"), "opens are");
    let profile = h.ok("GET", "/api/profiles?q=ann", None).await["data"][0].clone();
    assert_eq!(profile["email_consent"], "unsubscribed", "sending never changes consent");

    // On the person's timeline, and marked transactional (so it never counts as marketing).
    let pid = profile["id"].as_str().unwrap();
    let events = h.ok("GET", &format!("/api/events?profile_id={pid}"), None).await;
    assert!(events["data"].as_array().unwrap().iter().any(|e| e["metric"] == "Received Transactional Email"), "{events}");
    let transactional: bool = sqlx::query_scalar("SELECT transactional FROM messages WHERE id = $1::uuid").bind(v["message_id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert!(transactional);

    // Tracking: delivered, then opened.
    let pid_ses = provider_id(&h, &v).await;
    ses(&h, &pid_ses, json!({"notificationType": "Delivery"})).await;
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(h.ok("GET", &format!("/api/transactional/sends/{id}"), None).await["status"], "delivered");
    let pixel = find_link(&m.html, "https://pl.test/t/o/");
    let (s, _, _) = h.raw("GET", pixel.trim_start_matches("https://pl.test"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    let got = h.ok("GET", &format!("/api/transactional/sends/{id}"), None).await;
    assert_eq!(got["status"], "opened");
    assert!(got.get("request").is_none() && !got.to_string().contains("$36.00"), "no variables or body in the log: {got}");

    // Click tracking on request; postal address footer on request.
    let mut b = receipt(json!({"email": "bo@x.test", "name": "Bo Diaz"}));
    b["track_clicks"] = json!(true);
    b["address_footer"] = json!(true);
    let v = h.ok("POST", "/api/transactional/send", Some(b)).await;
    assert_eq!(v["status"], "sent");
    let m = &sent_to(&h, "bo@x.test")[0];
    assert!(m.html.contains("/t/c/") && !m.html.contains("href=\"https://acme.test/orders/1042\""));
    assert!(m.html.contains("1 Main St") && !m.html.to_lowercase().contains("unsubscribe"));
    assert!(m.html.contains("Thanks Bo!"), "the name fills the profile");

    // A hard bounce suppresses the address: the next receipt is skipped, not sent.
    ses(&h, &provider_id(&h, &v).await, json!({"notificationType": "Bounce", "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}})).await;
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("bo@x.test")))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "skipped", "{v}");
    assert!(v["error"].as_str().unwrap().contains("hard bounced"));
    assert_eq!(sent_to(&h, "bo@x.test").len(), 1);
    // A spam complaint too.
    let v = h.ok("POST", "/api/transactional/send", Some(receipt(json!("cy@x.test")))).await;
    ses(&h, &provider_id(&h, &v).await, json!({"notificationType": "Complaint", "complaint": {}})).await;
    let v = h.ok("POST", "/api/transactional/send", Some(receipt(json!("cy@x.test")))).await;
    assert_eq!(v["status"], "skipped");
    assert_eq!(sent_to(&h, "cy@x.test").len(), 1);

    // Plain text content works; the warm-up limit doesn't hold transactional mail.
    sqlx::query("UPDATE accounts SET email_daily_limit_override = 0").execute(&h.state.db).await.unwrap();
    let v = h.ok("POST", "/api/transactional/send", Some(json!({"to": "dee@x.test", "content": {"subject": "Reset your password", "text": "Use this link to reset it:\nhttps://acme.test/reset/abc"}}))).await;
    assert_eq!(v["status"], "sent", "{v}");
    let m = &sent_to(&h, "dee@x.test")[0];
    assert!(m.text.contains("https://acme.test/reset/abc") && m.html.contains("<br>"));

    // Search and filter the log.
    let l = h.ok("GET", "/api/transactional/sends?q=bo@&status=skipped", None).await;
    assert_eq!(l["data"].as_array().unwrap().len(), 1, "{l}");
    let (s, _) = h.req("GET", "/api/transactional/sends?status=lost", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // There's no resend: sending again is a new request with fresh variables.
    let (s, _) = h.req("POST", &format!("/api/transactional/sends/{id}/resend"), None).await;
    assert!(s == StatusCode::NOT_FOUND || s == StatusCode::METHOD_NOT_ALLOWED, "{s}");
    assert_eq!(sent_to(&h, "ann@x.test").len(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn idempotency_key_holds_under_retries() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let mut b = receipt(json!("eve@x.test"));
    b["idempotency_key"] = json!("order-1042");
    // Five retries at once: one email, one send, the same answer.
    let reqs: Vec<_> = (0..5).map(|_| Box::pin(api(&h, &key, "POST", "/api/transactional/send", Some(b.clone())))).collect();
    let results = futures_all(reqs).await;
    let ids: std::collections::HashSet<String> = results.iter().map(|(s, v)| {
        assert_eq!(*s, StatusCode::OK, "{v}");
        v["id"].as_str().unwrap().to_string()
    }).collect();
    assert_eq!(ids.len(), 1, "{results:?}");
    assert_eq!(results.iter().filter(|(_, v)| v["replayed"] == false).count(), 1);
    assert_eq!(sent_to(&h, "eve@x.test").len(), 1);
    // A later retry returns the first result without sending.
    let (_, again) = api(&h, &key, "POST", "/api/transactional/send", Some(b.clone())).await;
    assert_eq!((again["id"].as_str(), again["replayed"].as_bool()), (ids.iter().next().map(String::as_str), Some(true)));
    assert_eq!(sent_to(&h, "eve@x.test").len(), 1);
    // The same key for a different email is refused.
    let mut other = b.clone();
    other["variables"]["order_id"] = json!("9999");
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(other)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // After 24 hours the key can be used again.
    sqlx::query("UPDATE transactional_sends SET created_at = now() - interval '25 hours'").execute(&h.state.db).await.unwrap();
    let (_, fresh) = api(&h, &key, "POST", "/api/transactional/send", Some(b.clone())).await;
    assert_eq!(fresh["replayed"], false);
    assert_eq!(sent_to(&h, "eve@x.test").len(), 2);
    // A refused request leaves no record, so the corrected retry with the same key sends.
    let mut bad = receipt(json!("fay@x.test"));
    bad["idempotency_key"] = json!("signup-7");
    bad["from_email"] = json!("not an address");
    let (s, _) = api(&h, &key, "POST", "/api/transactional/send", Some(bad.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    bad.as_object_mut().unwrap().remove("from_email");
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(bad)).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");
    h.cleanup().await;
}

/// Poll the requests together, so they really overlap.
async fn futures_all<F: std::future::Future<Output = (StatusCode, Value)> + Unpin>(fs: Vec<F>) -> Vec<(StatusCode, Value)> {
    use std::task::Poll;
    let mut fs: Vec<Option<F>> = fs.into_iter().map(Some).collect();
    let mut out: Vec<Option<(StatusCode, Value)>> = (0..fs.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, slot) in fs.iter_mut().enumerate() {
            if let Some(f) = slot {
                match std::pin::Pin::new(f).poll(cx) {
                    Poll::Ready(v) => {
                        out[i] = Some(v);
                        *slot = None;
                    }
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending { Poll::Pending } else { Poll::Ready(()) }
    })
    .await;
    out.into_iter().map(Option::unwrap).collect()
}

#[tokio::test]
async fn abuse_rules_caps_pause_and_billing() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let me = h.ok("GET", "/api/me", None).await;
    let account_id = me["account"]["id"].as_str().unwrap().to_string();

    // One recipient per call.
    for to in [json!(["a@x.test", "b@x.test"]), json!("a@x.test, b@x.test"), json!({"email": "a@x.test", "cc": "b@x.test"})] {
        let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(to.clone()))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{to}: {v}");
    }
    // Promotions can't use it: inline, as a new template, or by editing a template later. One
    // promotional phrase is enough.
    for one in [json!({"subject": "Your order shipped", "text": "Limited time: track it below."}), json!({"subject": "Thanks", "text": "Use promo code BACK10 next time."})] {
        let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": "a@x.test", "content": one}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let (s, _) = h.req("POST", "/api/transactional/templates", Some(json!({"name": "One", "content": one}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
    let promo = json!({"subject": "20% off everything", "blocks": [{"type": "button", "text": "Shop now", "href": "https://acme.test"}]});
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": "a@x.test", "content": promo}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("reads like marketing"), "{v}");
    let (s, _) = h.req("POST", "/api/transactional/templates", Some(json!({"name": "Promo", "content": promo}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let t = h.ok("POST", "/api/transactional/templates", Some(json!({"name": "Shipped", "content": {"subject": "Order {{ order_id }} shipped", "blocks": [{"type": "text", "html": "<p>It's on the way.</p>"}]}}))).await;
    let tid = t["id"].as_str().unwrap().to_string();
    assert_eq!(t["transactional"], true);
    let v = h.ok("POST", "/api/transactional/send", Some(json!({"to": "a@x.test", "template_id": tid, "variables": {"order_id": "7"}}))).await;
    assert_eq!(v["subject"], "Order 7 shipped");
    h.ok("PATCH", &format!("/api/templates/{tid}"), Some(json!({"content": promo}))).await;
    let (s, _) = h.req("POST", "/api/transactional/send", Some(json!({"to": "a@x.test", "template_id": tid}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "edits are checked at every send");
    // Marketing templates can't be sent through it.
    let marketing = h.ok("POST", "/api/templates", Some(json!({"name": "Newsletter", "channel": "email", "content": {"subject": "News", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    let (s, v) = h.req("POST", "/api/transactional/send", Some(json!({"to": "a@x.test", "template_id": marketing["id"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // Another account can't see or resend these, or use this template.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "another pass", "company": "Other"}))).await;
    let sid = v_id(&h).await;
    assert_eq!(other.req("GET", &format!("/api/transactional/sends/{sid}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.req("POST", "/api/transactional/send", Some(json!({"to": "z@x.test", "template_id": tid}))).await.0, StatusCode::FORBIDDEN, "off for them");
    other.ok("PATCH", "/api/transactional/settings", Some(json!({"enabled": true, "acknowledge": true}))).await;
    assert_eq!(other.req("POST", "/api/transactional/send", Some(json!({"to": "z@x.test", "template_id": tid}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.ok("GET", "/api/transactional/sends", None).await["data"], json!([]));

    // Per-minute cap (set by the operator).
    let (s, _, body) = h.raw("POST", "/admin/transactional/limits", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": account_id, "per_minute_limit": 3, "daily_limit": null}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _, _) = h.raw("POST", "/admin/transactional/limits", &[("content-type", "application/json")], &json!({"account_id": account_id, "per_minute_limit": 1000}).to_string()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let mut statuses = vec![];
    for i in 0..3 {
        statuses.push(api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!(format!("cap{i}@x.test"))))).await.0);
    }
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 2, "one send already counted this minute: {statuses:?}");
    assert_eq!(statuses[2], StatusCode::PAYMENT_REQUIRED);
    let st = h.ok("GET", "/api/transactional/settings", None).await;
    assert_eq!((st["per_minute_limit"].as_i64(), st["sent_last_minute"].as_i64()), (Some(3), Some(3)), "{st}");
    sqlx::query("UPDATE transactional_sends SET created_at = now() - interval '2 hours'").execute(&h.state.db).await.unwrap();

    // A sudden flood to people with no other activity pauses transactional email and alerts the operator.
    sqlx::query(
        "INSERT INTO transactional_sends (id, account_id, to_email, status, request_hash, request, created_at)
         SELECT gen_random_uuid(), $1::uuid, 'flood' || g || '@x.test', 'sent', 'h', '{}', now() - interval '5 minutes' FROM generate_series(1, 600) g",
    )
    .bind(&account_id)
    .execute(&h.state.db)
    .await
    .unwrap();
    jobs::scheduler_tick(&h.state, 0).await.unwrap();
    let st = h.ok("GET", "/api/transactional/settings", None).await;
    assert_eq!(st["paused"], true, "{st}");
    assert!(st["paused_reason"].as_str().unwrap().contains("in the last hour"));
    let alert = h.state.email.sent().into_iter().find(|m| m.to == "ops@pl.test").expect("operator alert");
    assert!(alert.subject.starts_with("Transactional email paused: Acme Goods"));
    assert!(alert.text.contains("/admin/transactional/unpause") && alert.text.contains(&account_id));
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("late@x.test")))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // Campaigns are untouched; the operator lifts it.
    let (s, _, body) = h.raw("POST", "/admin/transactional/unpause", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": account_id}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    sqlx::query("DELETE FROM transactional_sends WHERE to_email LIKE 'flood%'").execute(&h.state.db).await.unwrap();
    let (s, _) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("late@x.test")))).await;
    assert_eq!(s, StatusCode::OK);

    // The account-wide email pause (bounces and complaints) stops transactional email too.
    sqlx::query("UPDATE accounts SET sending_paused_at = now(), sending_paused_reason = 'hard bounce rate 9%'").execute(&h.state.db).await.unwrap();
    let (s, _) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("late@x.test")))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    sqlx::query("UPDATE accounts SET sending_paused_at = NULL, sending_paused_reason = NULL").execute(&h.state.db).await.unwrap();

    // Billing: the free allowance first, then credit, through the normal per-email path.
    sqlx::query("UPDATE accounts SET free_emails_used = 25").execute(&h.state.db).await.unwrap();
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("paid@x.test")))).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    assert!(sent_to(&h, "paid@x.test").is_empty());
    let acct: uuid::Uuid = account_id.parse().unwrap();
    dumbmail::billing::post_entry(&h.state.db, acct, "topup", dumbmail::billing::to_micros(1.0), "test credit", "t1").await.unwrap();
    let before = h.ok("GET", "/api/billing", None).await["balance"].as_f64().unwrap();
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("paid@x.test")))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");
    let after = h.ok("GET", "/api/billing", None).await["balance"].as_f64().unwrap();
    assert!((before - after - 0.0002).abs() < 1e-9, "{before} -> {after}");
    h.cleanup().await;
}

async fn v_id(h: &Harness) -> String {
    sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM transactional_sends ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap().to_string()
}

#[tokio::test]
async fn mcp_tools_dry_run_then_send() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let rpc = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let call_mcp = |body: Value| {
        let router = h.router.clone();
        let key = key.clone();
        async move {
            let req = Request::builder().method("POST").uri("/mcp").header("authorization", format!("Bearer {key}")).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
            let res = router.oneshot(req).await.unwrap();
            let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
            v["result"].clone()
        }
    };
    let mut args = receipt(json!("gus@x.test"));
    args["include_html"] = json!(true);
    let dry = call_mcp(rpc("send_transactional_email", args.clone())).await;
    let dry = &dry["structuredContent"];
    assert_eq!((dry["dry_run"].as_bool(), dry["would_send"].as_bool()), (Some(true), Some(true)), "{dry}");
    assert_eq!(dry["subject"], "Order 1042 confirmed");
    assert!(dry["html"].as_str().unwrap().contains("Total $36.00"));
    assert!(sent_to(&h, "gus@x.test").is_empty());
    let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM transactional_sends").fetch_one(&h.state.db).await.unwrap();
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE email = 'gus@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((saved, profiles), (0, 0), "a dry run saves nothing");

    args["confirm"] = json!(true);
    let sent = call_mcp(rpc("send_transactional_email", args)).await;
    assert_eq!(sent["structuredContent"]["status"], "sent", "{sent}");
    let id = sent["structuredContent"]["id"].as_str().unwrap().to_string();
    assert_eq!(sent_to(&h, "gus@x.test").len(), 1);
    let list = call_mcp(rpc("list_transactional_sends", json!({"q": "gus"}))).await;
    assert_eq!(list["structuredContent"]["data"][0]["id"], json!(id));
    let one = call_mcp(rpc("get_transactional_send", json!({"id": id}))).await;
    assert_eq!(one["structuredContent"]["to"], "gus@x.test");
    let tools = call_mcp(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})).await;
    assert!(!tools["tools"].as_array().unwrap().iter().any(|t| t["name"] == "resend_transactional_email"), "no resend tool");
    let t = call_mcp(rpc("create_transactional_template", json!({"name": "Reset", "content": {"subject": "Reset your password", "text": "Reset: {{ link }}"}}))).await;
    assert_eq!(t["isError"], false, "{t}");
    assert_eq!(call_mcp(rpc("list_transactional_templates", json!({}))).await["structuredContent"]["data"].as_array().unwrap().len(), 1);
    let s = call_mcp(rpc("get_transactional_settings", json!({}))).await;
    assert_eq!(s["structuredContent"]["daily_limit"], 5000);
    let docs = call_mcp(json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/transactional"}})).await;
    assert!(docs["contents"][0]["text"].as_str().unwrap().contains("idempotency_key"));
    h.cleanup().await;
}

#[tokio::test]
async fn off_until_the_owner_turns_it_on_then_restricted_until_full_mode() {
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "store"}))).await["key"].as_str().unwrap().to_string();
    let id = account_id(&mut h).await;
    // Off by default: nothing sends, and the dry run says why.
    let st = h.ok("GET", "/api/transactional/settings", None).await;
    assert_eq!((st["enabled"].as_bool(), st["mode"].as_str()), (Some(false), Some("off")), "{st}");
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("ann@x.test")))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("off for this account"), "{v}");
    assert!(sent_to(&h, "ann@x.test").is_empty());
    // An API key can't turn it on, and the owner has to accept what it's for.
    let (s, _) = api(&h, &key, "PATCH", "/api/transactional/settings", Some(json!({"enabled": true, "acknowledge": true}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = h.req("PATCH", "/api/transactional/settings", Some(json!({"enabled": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    enable(&mut h, false).await;
    let st = h.ok("GET", "/api/transactional/settings", None).await;
    assert_eq!((st["mode"].as_str(), st["daily_limit"].as_i64()), (Some("restricted"), Some(200)), "{st}");

    // Restricted (hosted): saved transactional templates only, no inline content.
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("ann@x.test")))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("restricted mode"), "{v}");
    let t = h.ok("POST", "/api/transactional/templates", Some(json!({"name": "Receipt", "content": {"subject": "Order {{ order_id }}", "text": "Thanks for order {{ order_id }}."}}))).await;
    let by_template = json!({"to": "ann@x.test", "template_id": t["id"], "variables": {"order_id": "7"}});
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(by_template.clone())).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");
    // Its low daily cap.
    sqlx::query(
        "INSERT INTO transactional_sends (id, account_id, to_email, status, request_hash, request, created_at)
         SELECT gen_random_uuid(), $1::uuid, 'cap' || g || '@x.test', 'sent', 'h', '{}', now() - interval '2 hours' FROM generate_series(1, 199) g",
    )
    .bind(&id)
    .execute(&h.state.db)
    .await
    .unwrap();
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(by_template.clone())).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "200 a day in restricted mode: {v}");
    sqlx::query("DELETE FROM transactional_sends WHERE to_email LIKE 'cap%'").execute(&h.state.db).await.unwrap();

    // The operator grants full mode: inline content and the normal caps.
    let (s, _, _) = h.raw("POST", "/admin/transactional/mode", &[("content-type", "application/json")], &json!({"account_id": id, "mode": "full"}).to_string()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, body) = h.raw("POST", "/admin/transactional/mode", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": id, "mode": "full"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let st = h.ok("GET", "/api/transactional/settings", None).await;
    assert_eq!((st["mode"].as_str(), st["daily_limit"].as_i64()), (Some("full"), Some(5000)), "{st}");
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("bo@x.test")))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");

    // Anyone on the account can turn it off; then nothing sends.
    let (s, v) = api(&h, &key, "PATCH", "/api/transactional/settings", Some(json!({"enabled": false}))).await;
    assert_eq!((s, v["mode"].as_str()), (StatusCode::OK, Some("off")), "{v}");
    let (s, _) = api(&h, &key, "POST", "/api/transactional/send", Some(by_template)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    h.cleanup().await;

    // Self-hosted: once the owner turns it on, everything is allowed.
    let mut h = match Harness::with(|c| c.hosted = false).await {
        Some(h) => h,
        None => return,
    };
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "store"}))).await["key"].as_str().unwrap().to_string();
    assert_eq!(api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("cy@x.test")))).await.0, StatusCode::FORBIDDEN);
    enable(&mut h, false).await;
    assert_eq!(h.ok("GET", "/api/transactional/settings", None).await["mode"], "full");
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("cy@x.test")))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn goes_to_the_requested_mailbox_and_never_changes_a_profile() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let ann = h.ok("POST", "/api/profiles", Some(json!({"email": "old@x.test", "external_id": "cust-1", "first_name": "Ann", "email_consent": "unsubscribed", "properties": {"tier": "gold"}}))).await;
    let ann_id = ann["id"].as_str().unwrap().to_string();
    // A different email with Ann's external id: nothing merges, moves or changes.
    let b = json!({"to": {"email": "new@x.test", "external_id": "cust-1", "first_name": "Mallory", "properties": {"tier": "none"}},
        "content": {"subject": "Reset for {{ first_name }}", "text": "Reset link: {{ link }}"}, "variables": {"link": "https://acme.test/r/1"}});
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(b)).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");
    assert_eq!(sent_to(&h, "new@x.test").len(), 1);
    assert!(sent_to(&h, "old@x.test").is_empty());
    let after = h.ok("GET", &format!("/api/profiles/{ann_id}"), None).await["profile"].clone();
    assert_eq!((after["email"].as_str(), after["first_name"].as_str(), after["email_consent"].as_str()), (Some("old@x.test"), Some("Ann"), Some("unsubscribed")), "{after}");
    assert_eq!(after["properties"]["tier"], "gold");
    assert_ne!(v["profile_id"].as_str(), Some(ann_id.as_str()), "linked by email only");
    let new = h.ok("GET", &format!("/api/profiles/{}", v["profile_id"].as_str().unwrap()), None).await["profile"].clone();
    assert_eq!((new["email"].as_str(), new["email_consent"].as_str(), new["external_id"].as_str()), (Some("new@x.test"), Some("never_subscribed"), None), "{new}");
    // To Ann's own address with another name: the email uses it, her profile keeps hers.
    let (_, v) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": {"email": "old@x.test", "first_name": "Zed"}, "content": {"subject": "Hi {{ first_name }}", "text": "Receipt."}}))).await;
    assert_eq!(v["profile_id"].as_str(), Some(ann_id.as_str()));
    assert_eq!(sent_to(&h, "old@x.test")[0].subject, "Hi Zed");
    assert_eq!(h.ok("GET", &format!("/api/profiles/{ann_id}"), None).await["profile"]["first_name"], "Ann");

    // A retry goes to the recorded address even if the profile's email changed meanwhile.
    let (st, router, provider) = flaky(&h, 1);
    let (s, v) = call(&router, &key, "POST", "/api/transactional/send", Some(json!({"to": "reset@x.test", "content": {"subject": "Reset", "text": "Your reset link: {{ link }}"}, "variables": {"link": "https://acme.test/r/2"}}))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("queued")), "busy: waits for a retry: {v}");
    sqlx::query("UPDATE profiles SET email = 'someone-else@x.test' WHERE id = $1::uuid").bind(v["profile_id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();
    run_retries(&st).await;
    let got: Vec<String> = provider.sent().into_iter().map(|m| m.to).collect();
    assert_eq!(got, vec!["reset@x.test"], "never the profile's new address");
    assert_eq!(h.ok("GET", &format!("/api/transactional/sends/{}", v["id"].as_str().unwrap()), None).await["to"], "reset@x.test");

    // Suppression is by mailbox: an address that hard bounced stays suppressed after its profile moves on.
    let v = h.ok("POST", "/api/transactional/send", Some(receipt(json!("gone@x.test")))).await;
    ses(&h, &provider_id(&h, &v).await, json!({"notificationType": "Bounce", "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}})).await;
    sqlx::query("UPDATE profiles SET email = 'gone-now@x.test', email_suppressed_reason = NULL WHERE email = 'gone@x.test'").execute(&h.state.db).await.unwrap();
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("gone@x.test")))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("skipped")), "{v}");
    assert_eq!(sent_to(&h, "gone@x.test").len(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn one_delivery_and_one_charge_across_retries_crashes_and_races() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let acct: uuid::Uuid = account_id(&mut h).await.parse().unwrap();
    // Past the free allowance, so every email costs; with credit.
    sqlx::query("UPDATE accounts SET free_emails_used = 25, free_month = to_char(now() AT TIME ZONE timezone, 'YYYY-MM')").execute(&h.state.db).await.unwrap();
    h.credit(1.0).await;
    let balance = || async { dumbmail::billing::balance(&h.state.db, acct).await.unwrap() };
    let price = dumbmail::billing::prices(&h.state).email;

    // The provider is busy: the send waits with its job, then goes once and is charged once.
    let (st, router, provider) = flaky(&h, 2);
    let before = balance().await;
    let (s, v) = call(&router, &key, "POST", "/api/transactional/send", Some(receipt(json!("eve@x.test")))).await;
    assert_eq!((s, v["status"].as_str(), v["attempts"].as_i64()), (StatusCode::OK, Some("queued"), Some(1)), "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    // Five workers pick it up at once (a duplicated retry job, a backstop job, ...): one attempt wins.
    sqlx::query("UPDATE transactional_sends SET status = 'queued' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    provider.busy.store(0, Ordering::SeqCst);
    let uid: uuid::Uuid = id.parse().unwrap();
    let (a, b, c, d, e) = tokio::join!(tsend::retry(&st, uid), tsend::retry(&st, uid), tsend::retry(&st, uid), tsend::retry(&st, uid), tsend::retry(&st, uid));
    for r in [a, b, c, d, e] {
        r.unwrap();
    }
    run_retries(&st).await;
    h.fast_forward().await;
    assert_eq!(provider.sent().iter().filter(|m| m.to == "eve@x.test").count(), 1, "exactly one email");
    assert_eq!(before - balance().await, price, "charged exactly once");
    let (status, attempts, payload, _) = row(&h, &id).await;
    assert_eq!((status.as_str(), attempts, payload), ("sent", 2, None), "payload deleted once final");

    // A worker that claimed an attempt and died (it may have handed the email over): never sent again.
    let (st, router, provider) = flaky(&h, 1);
    let (_, v) = call(&router, &key, "POST", "/api/transactional/send", Some(receipt(json!("fay@x.test")))).await;
    let id = v["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE transactional_sends SET status = 'sending', claimed_at = now() - interval '11 minutes' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    jobs::scheduler_tick(&st, 1).await.unwrap();
    run_retries(&st).await;
    let (status, _, payload, error) = row(&h, &id).await;
    assert_eq!((status.as_str(), payload), ("unknown", None), "{error:?}");
    assert!(error.unwrap().contains("may have gone out"));
    assert!(provider.sent().iter().all(|m| m.to != "fay@x.test"));
    assert_eq!(h.ok("GET", &format!("/api/transactional/sends/{id}"), None).await["status"], "unknown");
    let l = h.ok("GET", "/api/transactional/sends?status=unknown", None).await;
    assert_eq!(l["data"].as_array().unwrap().len(), 1, "{l}");

    // A crash right after the reservation: its job, written with it, sends it once.
    let (st, _, provider) = flaky(&h, 0);
    let body: tsend::SendBody = serde_json::from_value(receipt(json!("gil@x.test"))).unwrap();
    let pid: uuid::Uuid = sqlx::query_scalar("INSERT INTO profiles (id, account_id, email) VALUES (gen_random_uuid(), $1, 'gil@x.test') RETURNING id").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let access = dumbmail::features::transactional::guard::access(&st, acct).await.unwrap();
    let sid = uuid::Uuid::now_v7();
    let reserved = dumbmail::features::transactional::guard::reserve(
        &st,
        acct,
        &access,
        dumbmail::features::transactional::guard::Reservation {
            id: sid,
            message_id: Some(uuid::Uuid::now_v7()),
            to: "gil@x.test",
            profile_id: pid,
            template_id: None,
            subject: "Order 1042 confirmed",
            idempotency_key: None,
            hash: "h",
            metadata: json!({}),
            track_clicks: false,
            via: "api",
            skipped: None,
            payload_enc: Some(dumbmail::crypto::encrypt(&st.cfg.secret_key, &serde_json::to_string(&body).unwrap()).unwrap()),
        },
    )
    .await
    .unwrap();
    assert!(matches!(reserved, dumbmail::features::transactional::guard::Reserved::New));
    let jobs_for: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'transactional_retry' AND payload->>'send_id' = $1").bind(sid.to_string()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(jobs_for, 1, "the job is saved with the reservation");
    run_retries(&st).await;
    run_retries(&st).await;
    assert_eq!(provider.sent().iter().filter(|m| m.to == "gil@x.test").count(), 1);
    assert_eq!(row(&h, &sid.to_string()).await.0, "sent");

    // Always busy: a bounded number of attempts, then a clear failure.
    let (st, router, provider) = flaky(&h, 1000);
    let (_, v) = call(&router, &key, "POST", "/api/transactional/send", Some(receipt(json!("hal@x.test")))).await;
    let id = v["id"].as_str().unwrap().to_string();
    for _ in 0..10 {
        run_retries(&st).await;
    }
    let (status, attempts, payload, error) = row(&h, &id).await;
    assert_eq!((status.as_str(), attempts, payload), ("failed", tsend::MAX_ATTEMPTS, None), "{error:?}");
    assert!(error.unwrap().contains("busy"));
    assert!(provider.sent().is_empty());

    // Retries check the pause, the caps and the balance again.
    for (setup_sql, undo_sql, expect) in [
        ("UPDATE accounts SET sending_paused_at = now(), sending_paused_reason = 'test'", "UPDATE accounts SET sending_paused_at = NULL, sending_paused_reason = NULL", "paused"),
        ("UPDATE transactional_settings SET daily_limit = 1", "UPDATE transactional_settings SET daily_limit = NULL", "limit"),
        ("UPDATE accounts SET balance_micros = 0", "SELECT 1", "balance"),
    ] {
        let (st, router, provider) = flaky(&h, 1);
        let (s, v) = call(&router, &key, "POST", "/api/transactional/send", Some(receipt(json!(format!("{expect}@x.test"))))).await;
        assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("queued")), "{v}");
        sqlx::query(setup_sql).execute(&h.state.db).await.unwrap();
        run_retries(&st).await;
        sqlx::query(undo_sql).execute(&h.state.db).await.unwrap();
        let (status, _, _, error) = row(&h, v["id"].as_str().unwrap()).await;
        assert_eq!(status, "failed", "{expect}: {error:?}");
        assert!(error.as_deref().unwrap_or("").contains(expect), "{expect}: {error:?}");
        assert!(provider.sent().is_empty(), "{expect}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn retries_of_an_accepted_key_replay_even_at_the_cap_and_busy_accounts_dont_starve_the_pool() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let id = account_id(&mut h).await;
    let (s, _, _) = h.raw("POST", "/admin/transactional/limits", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": id, "per_minute_limit": 1, "daily_limit": null}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let mut b = receipt(json!("ivy@x.test"));
    b["idempotency_key"] = json!("order-1");
    let (s, first) = api(&h, &key, "POST", "/api/transactional/send", Some(b.clone())).await;
    assert_eq!(s, StatusCode::OK, "{first}");
    // The cap is used up, but these are retries of the accepted request: all get its result.
    let reqs: Vec<_> = (0..4).map(|_| Box::pin(api(&h, &key, "POST", "/api/transactional/send", Some(b.clone())))).collect();
    for (s, v) in futures_all(reqs).await {
        assert_eq!((s, v["id"].as_str(), v["replayed"].as_bool()), (StatusCode::OK, first["id"].as_str(), Some(true)), "{v}");
    }
    // A new email is refused by the cap.
    assert_eq!(api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!("jo@x.test")))).await.0, StatusCode::PAYMENT_REQUIRED);

    // Many requests for one account at once (more than the pool has connections) all finish promptly.
    let (s, _, _) = h.raw("POST", "/admin/transactional/limits", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": id, "per_minute_limit": null, "daily_limit": null}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let reqs: Vec<_> = (0..16).map(|i| Box::pin(api(&h, &key, "POST", "/api/transactional/send", Some(receipt(json!(format!("crowd{i}@x.test"))))))).collect();
    let results = tokio::time::timeout(std::time::Duration::from_secs(20), futures_all(reqs)).await.expect("no pool starvation");
    assert!(results.iter().all(|(s, v)| *s == StatusCode::OK && v["status"] == "sent"), "{results:?}");
    h.cleanup().await;
}

#[tokio::test]
async fn the_log_keeps_no_variables_or_bodies() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let secret_link = "https://acme.test/reset/SECRET-TOKEN-9f8e7d";
    let b = json!({"to": "kim@x.test", "content": {"subject": "Reset your password", "text": "Reset it here: {{ link }}\n\nOr enter {{ code }}."}, "variables": {"link": secret_link, "code": "482913"}});
    // While it waits for a retry, the payload is encrypted.
    let (st, router, _) = flaky(&h, 1);
    let (_, v) = call(&router, &key, "POST", "/api/transactional/send", Some(b.clone())).await;
    let id = v["id"].as_str().unwrap().to_string();
    let dump = |id: String| {
        let db = h.state.db.clone();
        async move {
            let t: String = sqlx::query_scalar("SELECT row_to_json(t)::text FROM transactional_sends t WHERE id = $1::uuid").bind(&id).fetch_one(&db).await.unwrap();
            let m: String = sqlx::query_scalar("SELECT COALESCE(string_agg(row_to_json(m)::text, ''), '') FROM messages m WHERE m.id = (SELECT message_id FROM transactional_sends WHERE id = $1::uuid)").bind(&id).fetch_one(&db).await.unwrap();
            let e: String = sqlx::query_scalar("SELECT COALESCE(string_agg(e.properties::text, ''), '') FROM events e").fetch_one(&db).await.unwrap();
            format!("{t}{m}{e}")
        }
    };
    let waiting = dump(id.clone()).await;
    assert!(waiting.contains("payload_enc") && !waiting.contains("SECRET-TOKEN") && !waiting.contains("482913"), "{waiting}");
    run_retries(&st).await;
    let done = dump(id.clone()).await;
    assert!(!done.contains("SECRET-TOKEN") && !done.contains("482913"), "{done}");
    let (status, _, payload, _) = row(&h, &id).await;
    assert_eq!((status.as_str(), payload), ("sent", None));
    // The API and MCP show the send, never its variables or body.
    let got = h.ok("GET", &format!("/api/transactional/sends/{id}"), None).await;
    assert!(!got.to_string().contains("SECRET-TOKEN") && !got.to_string().contains("482913"), "{got}");
    let list = h.ok("GET", "/api/transactional/sends", None).await;
    assert!(!list.to_string().contains("SECRET-TOKEN"));
    // A payload never outlives 24 hours: a send still waiting then fails.
    let (st, router, _) = flaky(&h, 1000);
    let (_, v) = call(&router, &key, "POST", "/api/transactional/send", Some(b)).await;
    let id = v["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE transactional_sends SET created_at = now() - interval '25 hours' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    jobs::scheduler_tick(&st, 1).await.unwrap();
    let (status, _, payload, error) = row(&h, &id).await;
    assert_eq!((status.as_str(), payload), ("failed", None), "{error:?}");
    h.cleanup().await;
}

#[tokio::test]
async fn reply_to_and_sender_headers_are_checked_as_resolved() {
    let mut h = harness!();
    let key = setup(&mut h).await;
    let id = account_id(&mut h).await;
    let content = |extra: Value| {
        let mut c = json!({"subject": "Your receipt", "text": "Thanks."});
        c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        c
    };
    // Replies can't be routed to an unrelated mailbox, from the request or from the content.
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": "lee@x.test", "content": content(json!({})), "reply_to": "collect@elsewhere.test"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("verified sending domains"), "{v}");
    let (s, _) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": "lee@x.test", "content": content(json!({"reply_to": "collect@elsewhere.test"}))}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Header injection through content or templates is refused.
    for bad in [json!({"reply_to": "a@acme.test\r\nBcc: x@evil.test"}), json!({"from_name": "Acme\r\nBcc: x@evil.test"}), json!({"from_email": "hello@acme.test\nBcc: x@evil.test"})] {
        let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": "lee@x.test", "content": content(bad.clone())}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    assert!(sent_to(&h, "lee@x.test").is_empty());
    // On a verified domain, or the account's own sender address: fine.
    sqlx::query("INSERT INTO sending_domains (id, account_id, domain, status) VALUES (gen_random_uuid(), $1::uuid, 'acme.test', 'verified')").bind(&id).execute(&h.state.db).await.unwrap();
    let (s, v) = api(&h, &key, "POST", "/api/transactional/send", Some(json!({"to": "lee@x.test", "content": content(json!({})), "reply_to": "Help@Acme.test"}))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("sent")), "{v}");
    assert_eq!(sent_to(&h, "lee@x.test")[0].reply_to.as_deref(), Some("help@acme.test"));
    h.cleanup().await;
}

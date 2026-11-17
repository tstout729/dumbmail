//! Transactional email API end to end: sending, consent and suppression, tracking, idempotency
//! under retries, abuse limits, the automatic pause, billing, account isolation and MCP.

#[macro_use]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dumbmail::jobs;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::*;

const ADMIN: (&str, &str) = ("authorization", "Bearer test-admin-token-0123456789abcdef0123");

/// POST with an API key (how stores call the transactional API).
async fn api(h: &Harness, key: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {key}")).header("content-type", "application/json");
    let req = b.body(body.map_or(Body::empty(), |v| Body::from(v.to_string()))).unwrap();
    let res = h.router.clone().oneshot(req).await.unwrap();
    let s = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn setup(h: &mut Harness) -> String {
    h.signup().await;
    h.ok("POST", "/api/api-keys", Some(json!({"name": "store"}))).await["key"].as_str().unwrap().to_string()
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
    assert_eq!(got["request"]["variables"]["order_id"], "1042");

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

    // Resend makes a new send to the same person.
    let r = h.ok("POST", &format!("/api/transactional/sends/{id}/resend"), None).await;
    assert_ne!(r["id"].as_str(), Some(id.as_str()));
    assert_eq!(h.ok("GET", &format!("/api/transactional/sends/{}", r["id"].as_str().unwrap()), None).await["resent_from"], json!(id));
    assert_eq!(sent_to(&h, "ann@x.test").len(), 2);
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
    // Promotions can't use it: inline, as a new template, or by editing a template later.
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
    assert_eq!(other.req("POST", &format!("/api/transactional/sends/{sid}/resend"), None).await.0, StatusCode::NOT_FOUND);
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
    let call = |body: Value| {
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
    let dry = call(rpc("send_transactional_email", args.clone())).await;
    let dry = &dry["structuredContent"];
    assert_eq!((dry["dry_run"].as_bool(), dry["would_send"].as_bool()), (Some(true), Some(true)), "{dry}");
    assert_eq!(dry["subject"], "Order 1042 confirmed");
    assert!(dry["html"].as_str().unwrap().contains("Total $36.00"));
    assert!(sent_to(&h, "gus@x.test").is_empty());
    let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM transactional_sends").fetch_one(&h.state.db).await.unwrap();
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE email = 'gus@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((saved, profiles), (0, 0), "a dry run saves nothing");

    args["confirm"] = json!(true);
    let sent = call(rpc("send_transactional_email", args)).await;
    assert_eq!(sent["structuredContent"]["status"], "sent", "{sent}");
    let id = sent["structuredContent"]["id"].as_str().unwrap().to_string();
    assert_eq!(sent_to(&h, "gus@x.test").len(), 1);
    let list = call(rpc("list_transactional_sends", json!({"q": "gus"}))).await;
    assert_eq!(list["structuredContent"]["data"][0]["id"], json!(id));
    let one = call(rpc("get_transactional_send", json!({"id": id}))).await;
    assert_eq!(one["structuredContent"]["to"], "gus@x.test");
    let dry = call(rpc("resend_transactional_email", json!({"id": id}))).await;
    assert_eq!(dry["structuredContent"]["dry_run"], true);
    assert_eq!(sent_to(&h, "gus@x.test").len(), 1);
    let t = call(rpc("create_transactional_template", json!({"name": "Reset", "content": {"subject": "Reset your password", "text": "Reset: {{ link }}"}}))).await;
    assert_eq!(t["isError"], false, "{t}");
    assert_eq!(call(rpc("list_transactional_templates", json!({}))).await["structuredContent"]["data"].as_array().unwrap().len(), 1);
    let s = call(rpc("get_transactional_settings", json!({}))).await;
    assert_eq!(s["structuredContent"]["daily_limit"], 5000);
    let docs = call(json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/transactional"}})).await;
    assert!(docs["contents"][0]["text"].as_str().unwrap().contains("idempotency_key"));
    h.cleanup().await;
}

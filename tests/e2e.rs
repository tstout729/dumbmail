//! End-to-end tests against a real Postgres. Set TEST_DATABASE_URL to an admin connection
//! (e.g. postgres://postgres@127.0.0.1:55432/postgres); each test creates and drops its own database.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use postlark::config::Config;
use postlark::{app, jobs, AppState, MIGRATOR};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

struct Harness {
    state: AppState,
    router: axum::Router,
    cookie: Option<String>,
    admin: PgPool,
    dbname: String,
}

impl Harness {
    async fn new() -> Option<Self> {
        let admin_url = std::env::var("TEST_DATABASE_URL").ok()?;
        let admin = PgPoolOptions::new().max_connections(2).connect(&admin_url).await.expect("admin connect");
        let dbname = format!("postlark_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {dbname}")).execute(&admin).await.unwrap();
        let url = match admin_url.rsplit_once('/') {
            Some((base, _)) => format!("{base}/{dbname}"),
            None => panic!("bad TEST_DATABASE_URL"),
        };
        let db = PgPoolOptions::new().max_connections(5).connect(&url).await.unwrap();
        MIGRATOR.run(&db).await.unwrap();
        let cfg = Config {
            database_url: url,
            bind: "127.0.0.1:0".into(),
            public_url: "https://pl.test".into(),
            secret_key: "test-secret".into(),
            email_provider: "log".into(),
            ses_configuration_set: None,
            sms_provider: "log".into(),
            telnyx_api_key: None,
            telnyx_messaging_profile_id: None,
            twilio_account_sid: None,
            twilio_auth_token: None,
            webhook_secret: "hooksecret".into(),
            hosted: true,
            allow_signup: true,
            send_rate_per_sec: 1000,
            workers: 1,
            secure_cookies: false,
        };
        let state = AppState::build(cfg, db).await.unwrap();
        let router = app(state.clone());
        Some(Self { state, router, cookie: None, admin, dbname })
    }

    async fn req(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(c) = &self.cookie {
            b = b.header(header::COOKIE, c);
        }
        let req = match body {
            Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = self.router.clone().oneshot(req).await.unwrap();
        if let Some(set) = res.headers().get(header::SET_COOKIE) {
            let v = set.to_str().unwrap().split(';').next().unwrap().to_string();
            self.cookie = Some(v);
        }
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes).unwrap_or(json!({"_raw": String::from_utf8_lossy(&bytes)}));
        (status, v)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body).await;
        assert!(s.is_success(), "{method} {path} -> {s}: {v}");
        v
    }

    async fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = self.router.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let h = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, h, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn drain(&self) {
        jobs::drain(&self.state).await.unwrap();
    }

    /// Make every scheduled job due now (fast-forward delays).
    async fn fast_forward(&self) {
        sqlx::query("UPDATE jobs SET run_at = now() WHERE run_at <> 'infinity'").execute(&self.state.db).await.unwrap();
        self.drain().await;
    }

    async fn signup(&mut self) -> Value {
        let v = self.ok("POST", "/api/auth/signup", Some(json!({"email": "owner@acme.test", "password": "correct horse", "company": "Acme Goods"}))).await;
        self.ok("PATCH", "/api/account", Some(json!({"address": "1 Main St, Portland, OR 97201", "from_email": "hello@acme.test", "website": "acme.test"}))).await;
        v
    }

    async fn cleanup(self) {
        let Harness { state, admin, dbname, .. } = self;
        state.db.close().await;
        drop(state);
        sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)")).execute(&admin).await.unwrap();
    }
}

macro_rules! harness {
    () => {
        match Harness::new().await {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

fn find_link(html: &str, needle: &str) -> String {
    let i = html.find(needle).unwrap_or_else(|| panic!("{needle} not in html"));
    let rest = &html[i..];
    let end = rest.find('"').unwrap();
    rest[..end].replace("&amp;", "&")
}

#[tokio::test]
async fn signup_import_segment_campaign_tracking_unsubscribe() {
    let mut h = harness!();
    let me = h.signup().await;
    assert_eq!(me["account"]["name"], "Acme Goods");
    let (s, _) = h.req("GET", "/api/me", None).await;
    assert_eq!(s, StatusCode::OK);

    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let csv = "Email,First Name,City,Orders\nann@x.test,Ann,Portland,3\nbob@x.test,Bob,Seattle,0\nnot-an-email,Bad,Nowhere,1\ncat@x.test,Cat,portland,7\n";
    let imp = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list_id, "email_consent": "subscribed"}))).await;
    assert_eq!(imp["created"], 3);
    assert_eq!(imp["skipped"], 1);

    // Segment: Portland (case-insensitive) with more than 2 orders.
    let def = json!({"match": "all", "conditions": [
        {"type": "property", "field": "City", "op": "eq", "value": "PORTLAND"},
        {"type": "property", "field": "Orders", "op": "gt", "value": 2}]});
    let prev = h.ok("POST", "/api/segments/preview", Some(json!({"definition": def}))).await;
    assert_eq!(prev["count"], 2);
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Portland regulars", "definition": def}))).await;
    assert_eq!(seg["member_count"], 2);

    // Campaign to the list, excluding the segment → only Bob.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Fall launch", "channel": "email",
        "audience": {"include": [{"type": "list", "id": list_id}], "exclude": [{"type": "segment", "id": seg["id"]}]},
        "content": {"subject": "Hi {{ first_name|default('there') }}", "preheader": "New",
            "blocks": [{"type": "heading", "text": "Fall is here"}, {"type": "button", "text": "Shop", "href": "https://acme.test/fall"}]}}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": c["audience"]}))).await;
    assert_eq!(est["count"], 1);

    // Test send does not count.
    h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["qa@acme.test"]}))).await;
    assert!(h.state.email.sent().last().unwrap().subject.starts_with("[Test]"));

    let sent = h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(sent["status"], "sending");
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sent");
    assert_eq!(c["stats"]["recipients"], 1);
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "bob@x.test").expect("bob got the campaign");
    assert_eq!(mail.subject, "Hi Bob");
    assert!(mail.html.contains("1 Main St"));
    assert!(mail.unsubscribe_url.as_deref().unwrap().starts_with("https://pl.test/u/"));

    // Open pixel and click tracking.
    let pixel = find_link(&mail.html, "https://pl.test/t/o/").replace("https://pl.test", "");
    let (s, hd, _) = h.raw("GET", &pixel, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("content-type").unwrap(), "image/gif");
    let click = find_link(&mail.html, "https://pl.test/t/c/").replace("https://pl.test", "");
    let (s, hd, _) = h.raw("GET", &click, &[], "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(hd.get("location").unwrap(), "https://acme.test/fall");
    // Tampered destination is refused (no open redirect).
    let evil = click.replace("acme.test%2Ffall", "evil.test");
    let (s, _, _) = h.raw("GET", &evil, &[], "").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Attribution: Bob orders within 5 days of clicking.
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "bob@x.test"}, "value": 120.5, "unique_id": "order-1"}))).await;
    let dup = h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "bob@x.test"}, "value": 120.5, "unique_id": "order-1"}))).await;
    assert_eq!(dup["duplicate"], true);
    let rep = h.ok("GET", &format!("/api/campaigns/{cid}/report"), None).await;
    assert_eq!(rep["stats"]["unique_opens"], 1);
    assert_eq!(rep["stats"]["unique_clicks"], 1);
    assert_eq!(rep["stats"]["revenue"], 120.5);
    assert_eq!(rep["links"][0]["url"], "https://acme.test/fall");
    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    assert_eq!(ov["totals"]["campaign_revenue"], 120.5);
    assert_eq!(ov["totals"]["emails_sent"], 1);

    // One-click unsubscribe (RFC 8058).
    let unsub = mail.unsubscribe_url.unwrap().replace("https://pl.test", "");
    let (s, _, body) = h.raw("GET", &unsub, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Unsubscribe"));
    let (s, _, _) = h.raw("POST", &unsub, &[("content-type", "application/x-www-form-urlencoded")], "List-Unsubscribe=One-Click").await;
    assert_eq!(s, StatusCode::OK);
    let profiles = h.ok("GET", "/api/profiles?q=bob", None).await;
    assert_eq!(profiles["data"][0]["email_consent"], "unsubscribed");

    // Sent campaigns can't be edited or deleted.
    let (s, _) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"name": "x"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = h.req("DELETE", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.cleanup().await;
}

#[tokio::test]
async fn welcome_and_abandoned_checkout_flows() {
    let mut h = harness!();
    h.signup().await;
    let bps = h.ok("GET", "/api/flows/blueprints", None).await;
    assert!(bps["data"].as_array().unwrap().len() >= 5);

    let flow = h.ok("POST", "/api/flows/from-blueprint", Some(json!({"key": "welcome_series"}))).await;
    let fid = flow["id"].as_str().unwrap().to_string();
    let list_id = flow["trigger"]["list_id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;

    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "first_name": "Nia", "email_consent": "subscribed", "list_ids": [list_id]}))).await;
    h.drain().await;
    let first: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "new@x.test").collect();
    assert_eq!(first.len(), 1, "welcome email 1 sends immediately");
    assert!(first[0].subject.contains("Nia"));
    h.fast_forward().await; // 2-day delay
    h.fast_forward().await; // 3-day delay
    let all: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "new@x.test").collect();
    assert_eq!(all.len(), 3);
    let rep = h.ok("GET", &format!("/api/flows/{fid}/report"), None).await;
    assert_eq!(rep["total_runs"], 1);
    assert_eq!(rep["active_runs"], 0);
    // Re-adding to the list does not re-enter (reentry: never).
    h.ok("POST", &format!("/api/lists/{list_id}/members"), Some(json!({"emails": ["new@x.test"]}))).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/flows/{fid}/report"), None).await["total_runs"], 1);

    // Abandoned checkout: exits when the person buys.
    let ac = h.ok("POST", "/api/flows/from-blueprint", Some(json!({"key": "abandoned_checkout"}))).await;
    let acid = ac["id"].as_str().unwrap().to_string();
    // The blueprint has an SMS step, so going live needs an SMS number.
    let (s, v) = h.req("POST", &format!("/api/flows/{acid}/status"), Some(json!({"status": "live"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    h.ok("PATCH", "/api/account", Some(json!({"sms_from": "+15035550100"}))).await;
    h.ok("POST", &format!("/api/flows/{acid}/status"), Some(json!({"status": "live"}))).await;

    h.ok("POST", "/api/events", Some(json!({"metric": "Started Checkout", "profile": {"email": "buyer@x.test"}, "properties": {"checkout_url": "https://acme.test/cart/1"}, "value": 80}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Started Checkout", "profile": {"email": "browser@x.test"}, "properties": {"checkout_url": "https://acme.test/cart/2"}}))).await;
    h.drain().await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "buyer@x.test"}, "value": 80}))).await;
    h.fast_forward().await; // 4h delay → reminder for browser only
    let to_browser: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "browser@x.test").collect();
    assert_eq!(to_browser.len(), 1);
    assert!(to_browser[0].html.contains("https://pl.test/t/c/"), "cart link is tracked");
    assert!(h.state.email.sent().iter().all(|m| m.to != "buyer@x.test"), "buyer exited before the reminder");
    h.fast_forward().await; // 1 day → condition: no SMS consent → last-call email
    let to_browser: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "browser@x.test").collect();
    assert_eq!(to_browser.len(), 2);
    h.cleanup().await;
}

#[tokio::test]
async fn sms_stop_forms_client_events_and_limits() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.ok("PATCH", "/api/account", Some(json!({"sms_from": "+15035550100"}))).await;

    // Browser events with the public key; consent can't be set from the browser.
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Viewed Product", "profile": {"email": "shopper@x.test", "email_consent": "subscribed"}, "properties": {"name": "Scarf"}}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let p = h.ok("GET", "/api/profiles?q=shopper", None).await;
    assert_eq!(p["data"][0]["email_consent"], "never_subscribed");
    let (s, _, _) = h.raw("POST", "/client/events?k=pk_wrong", &[("content-type", "application/json")], "{}").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Sign-up form with SMS consent.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "SMS club"}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "kind": "popup", "list_id": list["id"], "config": {"collect": ["email", "phone_number"]}}))).await;
    let form_id = form["id"].as_str().unwrap().to_string();
    let (s, _, js) = h.raw("GET", &format!("/f/{form_id}/embed.js"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(js.contains("not published"));
    h.ok("PATCH", &format!("/api/forms/{form_id}"), Some(json!({"status": "live"}))).await;
    let (_, _, js) = h.raw("GET", &format!("/f/{form_id}/embed.js"), &[], "").await;
    assert!(js.contains("/client/subscribe"));
    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")],
        &json!({"form_id": form_id, "email": "fan@x.test", "phone_number": "(503) 555-0199", "sms_consent": true}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let fan = h.ok("GET", "/api/profiles?q=fan", None).await["data"][0].clone();
    assert_eq!(fan["email_consent"], "subscribed");
    assert_eq!(fan["sms_consent"], "subscribed");
    assert_eq!(fan["phone_number"], "+15035550199");
    assert_eq!(h.ok("GET", &format!("/api/lists/{}", list["id"].as_str().unwrap()), None).await["member_count"], 1);

    // SMS preview counts segments and adds compliance text.
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": "Hey {{ first_name }}, 20% off today!"}}))).await;
    assert_eq!(r["segments"], 1);
    assert!(r["body"].as_str().unwrap().contains("Reply STOP"));

    // Inbound STOP through the Telnyx webhook.
    let (s, _, _) = h.raw("POST", "/webhooks/telnyx/wrong", &[("content-type", "application/json")], "{}").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let stop = json!({"data": {"event_type": "message.received", "payload": {"text": "STOP", "from": {"phone_number": "+15035550199"}, "to": [{"phone_number": "+15035550100"}]}}});
    let (s, _, _) = h.raw("POST", "/webhooks/telnyx/hooksecret", &[("content-type", "application/json")], &stop.to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let fan = h.ok("GET", "/api/profiles?q=fan", None).await["data"][0].clone();
    assert_eq!(fan["sms_consent"], "unsubscribed");

    // Free plan has no SMS: sending is refused with a clear limit error.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"body": "hi"}}))).await;
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed'").execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");

    // SES hard bounce suppresses the address.
    h.ok("POST", "/api/profiles", Some(json!({"email": "gone@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let ec = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", ec["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let pid: String = sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE to_address = 'gone@x.test' AND NOT is_test").fetch_one(&h.state.db).await.unwrap();
    let sns = json!({"Type": "Notification", "Message": json!({"notificationType": "Bounce", "mail": {"messageId": pid}, "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}}).to_string()});
    let (s, _, _) = h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &sns.to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let reason: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE email = 'gone@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(reason.as_deref(), Some("hard_bounce"));
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}}))).await;
    assert_eq!(est["count"], 1, "bounced address is no longer reachable");
    h.cleanup().await;
}

#[tokio::test]
async fn mcp_server() {
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Claude"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let rpc = |id: i64, method: &str, params: Value| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let call = |h: &Harness, body: String| {
        let auth = auth.clone();
        let router = h.router.clone();
        async move {
            let req = Request::builder().method("POST").uri("/mcp").header("authorization", auth).header("content-type", "application/json").body(Body::from(body)).unwrap();
            let res = router.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };

    let (s, _, _) = h.raw("POST", "/mcp", &[("content-type", "application/json")], &rpc(1, "initialize", json!({}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let (_, init) = call(&h, rpc(1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}))).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let (s, _) = call(&h, json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string()).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (_, list) = call(&h, rpc(2, "tools/list", json!({}))).await;
    assert!(list["result"]["tools"].as_array().unwrap().len() > 40);

    let tool = |id: i64, name: &str, args: Value| rpc(id, "tools/call", json!({"name": name, "arguments": args}));
    let (_, acct) = call(&h, tool(3, "get_account", json!({}))).await;
    assert_eq!(acct["result"]["isError"], false);
    assert_eq!(acct["result"]["structuredContent"]["account"]["name"], "Acme Goods");

    let (_, l) = call(&h, tool(4, "create_list", json!({"name": "VIP"}))).await;
    let list_id = l["result"]["structuredContent"]["id"].as_str().unwrap().to_string();
    call(&h, tool(5, "upsert_profile", json!({"email": "vip@x.test", "first_name": "Val", "email_consent": "subscribed", "list_ids": [list_id]}))).await;
    let (_, ev) = call(&h, tool(6, "track_event", json!({"metric": "Placed Order", "profile": {"email": "vip@x.test"}, "value": 300}))).await;
    assert_eq!(ev["result"]["isError"], false, "{ev}");
    let (_, pv) = call(&h, tool(7, "preview_segment", json!({"definition": {"match": "all", "conditions": [{"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1, "where": [{"field": "value", "op": "gte", "value": 250}]}]}}))).await;
    assert_eq!(pv["result"]["structuredContent"]["count"], 1);

    let (_, c) = call(&h, tool(8, "create_campaign", json!({"name": "VIP preview", "channel": "email", "audience": {"include": [{"type": "list", "id": list_id}]},
        "content": {"subject": "For you, {{ first_name }}", "blocks": [{"type": "text", "html": "<p>Early access.</p>"}]}}))).await;
    let sc = &c["result"]["structuredContent"];
    assert_eq!(sc["estimated_recipients"], 1);
    assert_eq!(sc["ready_to_send"], true, "{sc}");
    let cid = sc["id"].as_str().unwrap().to_string();

    let (_, pr) = call(&h, tool(9, "render_preview", json!({"channel": "email", "content": {"subject": "For you, {{ first_name }}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}))).await;
    assert_eq!(pr["result"]["structuredContent"]["subject"], "For you, Alex");
    assert!(pr["result"]["structuredContent"]["html"].is_null(), "html omitted unless asked");

    // Without confirm it's a dry run; nothing is sent.
    let (_, dry) = call(&h, tool(10, "send_campaign", json!({"campaign_id": cid}))).await;
    assert_eq!(dry["result"]["structuredContent"]["dry_run"], true);
    h.drain().await;
    assert!(h.state.email.sent().is_empty());
    let (_, go) = call(&h, tool(11, "send_campaign", json!({"campaign_id": cid, "confirm": true}))).await;
    assert_eq!(go["result"]["structuredContent"]["status"], "sending", "{go}");
    h.drain().await;
    assert_eq!(h.state.email.sent().len(), 1);

    // Flow from blueprint, dry run on going live.
    let (_, f) = call(&h, tool(12, "create_flow_from_blueprint", json!({"key": "post_purchase"}))).await;
    let fid = f["result"]["structuredContent"]["id"].as_str().unwrap().to_string();
    let (_, d) = call(&h, tool(13, "set_flow_status", json!({"id": fid, "status": "live"}))).await;
    assert_eq!(d["result"]["structuredContent"]["dry_run"], true);

    // Invalid flow definitions come back as tool errors, not protocol errors.
    let (_, bad) = call(&h, tool(14, "create_flow", json!({"name": "Bad", "trigger": {"type": "metric", "metric": "X"}, "definition": {"entry": "a", "nodes": [{"id": "a", "type": "delay", "amount": 1, "unit": "days", "next": "nope"}]}}))).await;
    assert_eq!(bad["result"]["isError"], true);

    let (_, perf) = call(&h, tool(15, "campaign_performance", json!({}))).await;
    assert_eq!(perf["result"]["structuredContent"]["campaigns"].as_array().unwrap().len(), 1);
    let (_, an) = call(&h, tool(16, "get_analytics", json!({"days": 7}))).await;
    assert_eq!(an["result"]["structuredContent"]["totals"]["emails_sent"], 1);
    let (_, res) = call(&h, rpc(17, "resources/read", json!({"uri": "postlark://docs/flows"}))).await;
    assert!(res["result"]["contents"][0]["text"].as_str().unwrap().contains("exit_condition"));
    h.cleanup().await;
}

#[tokio::test]
async fn security_guards() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();

    // Flow webhooks can't target internal addresses.
    for url in ["http://127.0.0.1:7311/api/me", "http://169.254.169.254/latest/meta-data", "http://localhost/", "http://[::1]/", "http://10.0.0.5/"] {
        let (s, v) = h.req("POST", "/api/flows", Some(json!({"name": "w", "trigger": {"type": "metric", "metric": "X"},
            "definition": {"entry": "a", "nodes": [{"id": "a", "type": "webhook", "url": url, "next": null}]}}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {v}");
    }

    // Lists from another account can't be used in flows.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "another pass", "company": "Other"}))).await;
    let foreign = other.ok("POST", "/api/lists", Some(json!({"name": "Theirs"}))).await;
    let (s, _) = h.req("POST", "/api/flows", Some(json!({"name": "steal", "trigger": {"type": "metric", "metric": "X"},
        "definition": {"entry": "a", "nodes": [{"id": "a", "type": "add_to_list", "list_id": foreign["id"], "next": null}]}}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.req("GET", &format!("/api/lists/{}", foreign["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Form colors must be hex (no CSS/HTML injection).
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await;
    let (s, _) = h.req("POST", "/api/forms", Some(json!({"list_id": list["id"], "config": {"brand_color": "red}</style><script>alert(1)</script>"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // The browser can't overwrite an existing profile's identity or name.
    h.ok("POST", "/api/profiles", Some(json!({"email": "vic@x.test", "first_name": "Victoria", "phone_number": "+15035550111"}))).await;
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Active on Site", "profile": {"email": "vic@x.test", "first_name": "Mallory", "phone_number": "+15035550999"}}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let vic = h.ok("GET", "/api/profiles?q=vic", None).await["data"][0].clone();
    assert_eq!(vic["first_name"], "Victoria");
    assert_eq!(vic["phone_number"], "+15035550111");

    // Test sends refuse unsubscribed addresses.
    h.ok("POST", "/api/profiles", Some(json!({"email": "nope@x.test", "email_consent": "unsubscribed"}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "content": {"subject": "s", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}))).await;
    let (s, _) = h.req("POST", &format!("/api/campaigns/{}/test", c["id"].as_str().unwrap()), Some(json!({"to": ["nope@x.test"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // API keys can't mint more keys; revoked keys stop working.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "k"}))).await;
    let k = key["key"].as_str().unwrap();
    let (s, _, _) = h.raw("POST", "/api/api-keys", &[("authorization", &format!("Bearer {k}")), ("content-type", "application/json")], "{}").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    h.ok("DELETE", &format!("/api/api-keys/{}", key["id"].as_str().unwrap()), None).await;
    let (s, _, _) = h.raw("GET", "/api/me", &[("authorization", &format!("Bearer {k}"))], "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.cleanup().await;
}

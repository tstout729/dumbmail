//! End-to-end tests against a real Postgres. Set TEST_DATABASE_URL to an admin connection
//! (e.g. postgres://postgres@127.0.0.1:55432/postgres); each test creates and drops its own database.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use dumbmail::config::Config;
use dumbmail::{app, jobs, AppState, MIGRATOR};
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
        let dbname = format!("dumbmail_test_{}", uuid::Uuid::new_v4().simple());
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
            stripe_secret_key: None,
            stripe_webhook_secret: Some("whsec_test".into()),
            price_email_per_1000: 0.20,
            price_sms_segment: 0.0099,
            price_mms: 0.025,
            price_number_monthly: 2.20,
            free_emails_per_month: 25,
            aws_region: "us-east-1".into(),
            railway_api_token: None,
            railway_project_id: None,
            railway_environment_id: None,
            railway_service_id: None,
            admin_token: Some("test-admin-token-0123456789abcdef0123".into()),
            google_client_id: Some("test-client.apps.googleusercontent.com".into()),
            google_client_secret: Some("test-google-secret".into()),
            google_site_verification: Some("google0123abcd.html".into()),
            support_email: Some("help@pl.test".into()),
            redirect_hosts: vec!["old.pl.test".into()],
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

    /// Give the account prepaid credit (as a Stripe top-up would).
    async fn credit(&self, usd: f64) {
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&self.state.db).await.unwrap();
        dumbmail::billing::post_entry(&self.state.db, id, "topup", dumbmail::billing::to_micros(usd), "test credit", &uuid::Uuid::new_v4().to_string()).await.unwrap();
    }

    /// Claim a texting number from the dev inventory and return it.
    async fn claim_number(&mut self) -> String {
        let avail = self.ok("GET", "/api/phone-numbers/available?type=toll_free", None).await;
        let num = avail["data"][0]["phone_number"].as_str().unwrap().to_string();
        let row = self.ok("POST", "/api/phone-numbers", Some(json!({"phone_number": num}))).await;
        assert_eq!(row["status"], "active", "{row}");
        num
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
    assert!(hd.get("location").unwrap().to_str().unwrap().starts_with("https://acme.test/fall?_pl="));
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
    h.credit(5.0).await;
    h.claim_number().await;
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
    // Hosted: you can't just type in someone else's number.
    let (s, _) = h.req("PATCH", "/api/account", Some(json!({"sms_from": "+15035550100"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.credit(5.0).await;
    let our_number = h.claim_number().await;

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
    let stop = json!({"data": {"event_type": "message.received", "payload": {"text": "STOP", "from": {"phone_number": "+15035550199"}, "to": [{"phone_number": our_number}]}}});
    let (s, _, _) = h.raw("POST", "/webhooks/telnyx/hooksecret", &[("content-type", "application/json")], &stop.to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let fan = h.ok("GET", "/api/profiles?q=fan", None).await["data"][0].clone();
    assert_eq!(fan["sms_consent"], "unsubscribed");

    // An empty balance refuses SMS sends with a clear message.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"body": "hi"}}))).await;
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE accounts SET balance_micros = 0").execute(&h.state.db).await.unwrap();
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


#[tokio::test]
async fn billing_numbers_onsite_and_domains() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["mode"], "hosted");
    assert_eq!(b["balance"], 0.0);
    assert_eq!(b["prices"]["email_per_1000"], 0.2);
    assert_eq!(b["free_emails_remaining"], 25);

    // 30 recipients: 25 free, 5 billable at $0.0002 → refused on an empty balance, then sent after a top-up.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await;
    let mut csv = String::from("email\n");
    for i in 0..30 { csv.push_str(&format!("p{i}@x.test\n")); }
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed"}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("$0.00"));

    // Stripe top-up webhook (signed) credits the balance once, even if delivered twice.
    let ev = json!({"type": "checkout.session.completed", "data": {"object": {"id": "cs_test_1", "payment_status": "paid", "amount_total": 1000,
        "metadata": {"kind": "topup", "account_id": me["account"]["id"]}, "customer": "cus_1"}}}).to_string();
    let t = chrono::Utc::now().timestamp();
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"whsec_test").unwrap();
    mac.update(format!("{t}.{ev}").as_bytes());
    let sig = format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()));
    for _ in 0..2 {
        let (s, _, body) = h.raw("POST", "/webhooks/stripe", &[("stripe-signature", &sig), ("content-type", "application/json")], &ev).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let (s, _, _) = h.raw("POST", "/webhooks/stripe", &[("stripe-signature", "t=1,v1=bad")], &ev).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(h.ok("GET", "/api/billing", None).await["balance"], 10.0);

    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["free_emails_remaining"], 0);
    assert!((b["balance"].as_f64().unwrap() - (10.0 - 5.0 * 0.0002)).abs() < 1e-9, "{}", b["balance"]);
    assert_eq!(b["usage"][0]["kind"], "email");
    assert_eq!(b["usage"][0]["quantity"], 30);

    // Phone numbers: claim one (first month charged), then text a subscriber and get billed per segment.
    let num = h.claim_number().await;
    let nums = h.ok("GET", "/api/phone-numbers", None).await;
    assert_eq!(nums["sms"]["from_number"], num.as_str());
    assert_eq!(nums["sms"]["can_send"], true);
    assert!(nums["sms"]["inbound_webhook_url"].as_str().unwrap().contains("/webhooks/telnyx/"));
    let before = h.ok("GET", "/api/billing", None).await["balance"].as_f64().unwrap();
    assert!((before - (10.0 - 0.001 - 2.20)).abs() < 1e-6, "{before}");
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550142", "sms_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let sc = h.ok("POST", "/api/campaigns", Some(json!({"channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"body": "Flash sale today"}}))).await;
    assert_eq!(sc["audience"]["exclude"], json!([]), "a partial audience comes back in full: {sc}");
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", sc["id"].as_str().unwrap()), None).await;
    assert!(s.is_success(), "{v}");
    h.drain().await;
    // Quiet hours may defer the text; when it sent, it cost one segment.
    let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE channel = 'sms' AND status = 'sent'").fetch_one(&h.state.db).await.unwrap();
    let after = h.ok("GET", "/api/billing", None).await["balance"].as_f64().unwrap();
    assert!((before - after - 0.0099 * sent as f64).abs() < 1e-6, "{before} {after} {sent}");

    // Per-account inbound webhook token only affects that account.
    let hook = nums["sms"]["inbound_webhook_url"].as_str().unwrap().replace("https://pl.test", "");
    let stop = json!({"data": {"event_type": "message.received", "payload": {"text": "stop", "from": {"phone_number": "+15035550142"}, "to": [{"phone_number": num}]}}});
    let (s, _, _) = h.raw("POST", &hook, &[("content-type", "application/json")], &stop.to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let p = h.ok("GET", "/api/profiles?q=0142", None).await;
    assert_eq!(p["data"][0]["sms_consent"], "unsubscribed");

    // Onsite script, forms feed and install detection.
    let (s, hd, js) = h.raw("GET", &format!("/onsite.js?k={pk}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(hd.get("content-type").unwrap().to_str().unwrap().contains("javascript"));
    assert!(js.contains(&format!("\"{pk}\"")) && js.contains("\"https://pl.test\"") && !js.contains("__BASE__"));
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Pop", "kind": "popup", "list_id": list["id"], "status": "live",
        "config": {"targeting": {"trigger": "exit_intent", "url_excludes": ["/checkout"]}}}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", form["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let (_, _, feed) = h.raw("GET", &format!("/client/forms?k={pk}"), &[], "").await;
    let feed: Value = serde_json::from_str(&feed).unwrap();
    assert_eq!(feed["data"][0]["config"]["targeting"]["trigger"], "exit_intent");
    assert_eq!(h.ok("GET", "/api/website", None).await["installed"], false);
    h.raw("POST", &format!("/client/ping?k={pk}"), &[("content-type", "application/json")], &json!({"url": "https://shop.example.com/"}).to_string()).await;
    let w = h.ok("GET", "/api/website", None).await;
    assert_eq!(w["installed"], true);
    assert_eq!(w["last_seen_url"], "https://shop.example.com/");
    assert!(w["snippet"].as_str().unwrap().contains("/onsite.js?k="));
    let (s, v) = h.req("POST", "/api/website/check", Some(json!({"url": "http://127.0.0.1:5432/"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // Email click → destination gets a token that identifies the visitor to the onsite script.
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "p0@x.test" && !m.subject.starts_with("[Test]")).unwrap();
    let _ = mail; // campaign had no links; send one with a link
    let lc = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Link", "blocks": [{"type": "button", "text": "Shop", "href": "https://acme.test/new?x=1"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", lc["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let mail = h.state.email.sent().into_iter().rev().find(|m| m.to == "p1@x.test").unwrap();
    let i = mail.html.find("https://pl.test/t/c/").unwrap();
    let click = mail.html[i..mail.html[i..].find('"').unwrap() + i].replace("&amp;", "&").replace("https://pl.test", "");
    let (s, hd, _) = h.raw("GET", &click, &[], "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = hd.get("location").unwrap().to_str().unwrap().to_string();
    assert!(loc.starts_with("https://acme.test/new?x=1&_pl="), "{loc}");
    let token = url::Url::parse(&loc).unwrap().query_pairs().find(|(k, _)| k == "_pl").unwrap().1.to_string();
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Viewed Product", "profile": {"$token": token}, "properties": {"name": "Scarf"}}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = 'Viewed Product' AND p.email = 'p1@x.test'")
        .fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1, "token identified the clicker");
    // A forged token is ignored (no profile without an identifier).
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Viewed Product", "profile": {"$token": "pdeadbeef.forged"}}).to_string()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Sending domain: DKIM + MAIL FROM + DMARC records; link domain: CNAME to the app, plus routing guard.
    let sd = h.ok("POST", "/api/account/domain", Some(json!({"domain": "Acme-Test-Domain.example"}))).await;
    let recs = sd["email"]["dns_records"].as_array().unwrap();
    assert!(recs.iter().any(|r| r["purpose"] == "dkim"));
    assert!(recs.iter().any(|r| r["purpose"] == "mail_from" && r["type"] == "MX" && r["name"] == "send.acme-test-domain.example"));
    assert!(recs.iter().any(|r| r["purpose"] == "dmarc"));
    assert!(recs.iter().any(|r| r["purpose"] == "verification" && r["name"] == "_dumbmail.acme-test-domain.example"
        && r["value"].as_str().unwrap().starts_with("dumbmail-verify=")));
    assert_eq!(sd["email"]["status"], "pending", "not verified without the ownership record");
    assert_eq!(sd["email"]["mail_from_domain"], "send.acme-test-domain.example");
    // Several sending domains: each is listed with its own records and status.
    let sd = h.ok("POST", "/api/account/domains", Some(json!({"domain": "second-brand.example"}))).await;
    let names: Vec<&str> = sd["email"]["domains"].as_array().unwrap().iter().map(|d| d["domain"].as_str().unwrap()).collect();
    assert_eq!(names, ["acme-test-domain.example", "second-brand.example"]);
    assert!(sd["email"]["domains"][1]["dns_records"].as_array().unwrap().iter().any(|r| r["name"] == "_dumbmail.second-brand.example"));
    // Adding one that's already there just re-checks it.
    let again = h.ok("POST", "/api/account/domains", Some(json!({"domain": "Second-Brand.example"}))).await;
    assert_eq!(again["email"]["domains"].as_array().unwrap().len(), 2);
    // Verified domains (and their subdomains) are what the account may send from.
    sqlx::query("UPDATE sending_domains SET status = 'verified' WHERE domain = 'second-brand.example'").execute(&h.state.db).await.unwrap();
    let acct_id: uuid::Uuid = sqlx::query_scalar("SELECT account_id FROM sending_domains WHERE domain = 'second-brand.example'").fetch_one(&h.state.db).await.unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct_id).await.unwrap();
    assert_eq!(account.verified_domains, ["second-brand.example"]);
    assert!(dumbmail::sending::check_sender_domain_name(&account, "hi@news.second-brand.example").is_ok());
    assert!(dumbmail::sending::check_sender_domain_name(&account, "hi@acme-test-domain.example").is_err());
    let v = h.ok("POST", "/api/account/domains/second-brand.example/verify", None).await;
    assert_eq!(v["email"]["domains"][1]["domain"], "second-brand.example");
    // The old single-domain delete refuses to guess which one to remove.
    let (s, _) = h.req("DELETE", "/api/account/domain", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // There's a cap per account.
    sqlx::query("INSERT INTO sending_domains (id, account_id, domain) SELECT gen_random_uuid(), $1, 'filler' || g || '.example' FROM generate_series(1, 18) g")
        .bind(acct_id).execute(&h.state.db).await.unwrap();
    let (s, _) = h.req("POST", "/api/account/domains", Some(json!({"domain": "one-too-many.example"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Simultaneous adds can't get past the cap either.
    sqlx::query("DELETE FROM sending_domains WHERE domain = 'filler1.example'").execute(&h.state.db).await.unwrap();
    let (a, b) = tokio::join!(
        dumbmail::domains::add_sending_domain(&h.state, acct_id, "race-a.example"),
        dumbmail::domains::add_sending_domain(&h.state, acct_id, "race-b.example"),
    );
    assert!(a.is_ok() != b.is_ok(), "exactly one of two simultaneous adds fits under the cap");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sending_domains WHERE account_id = $1").bind(acct_id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, dumbmail::domains::MAX_SENDING_DOMAINS);
    sqlx::query("DELETE FROM sending_domains WHERE domain LIKE 'race-%'").execute(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM sending_domains WHERE domain LIKE 'filler%'").execute(&h.state.db).await.unwrap();
    let rm = h.ok("DELETE", "/api/account/domains/second-brand.example", None).await;
    assert_eq!(rm["email"]["domains"].as_array().unwrap().len(), 1);
    let (s, _) = h.req("DELETE", "/api/account/domains/not-mine.example", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let ld = h.ok("POST", "/api/account/link-domain", Some(json!({"domain": "links.acme-test-domain.example"}))).await;
    assert_eq!(ld["link_domain"]["domain"], "links.acme-test-domain.example");
    assert_eq!(ld["link_domain"]["dns_records"][0]["value"], "pl.test");
    let (s, hd, _) = h.raw("GET", "/campaigns", &[("host", "links.acme-test-domain.example")], "").await;
    assert_eq!(s, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(hd.get("location").unwrap(), "https://pl.test/campaigns");
    let (s, _, _) = h.raw("GET", "/healthz", &[("host", "links.acme-test-domain.example")], "").await;
    assert_eq!(s, StatusCode::OK);
    h.ok("DELETE", "/api/account/link-domain", None).await;

    // Operator credit endpoint.
    let body = json!({"email": "owner@acme.test", "amount": 5}).to_string();
    let (s, _, _) = h.raw("POST", "/admin/credit", &[("authorization", "Bearer wrong-token-0123456789abcdef01234567"), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, out) = h.raw("POST", "/admin/credit", &[("authorization", "Bearer test-admin-token-0123456789abcdef0123"), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");

    // Texts through the customer's own provider are never charged.
    let acct = uuid::Uuid::parse_str(me["account"]["id"].as_str().unwrap()).unwrap();
    // The flag comes from the provider that sends, so flipping the account setting mid-send can't skip a charge.
    let (_, _, own) = dumbmail::phone::sender(&h.state, acct).await.unwrap();
    assert!(!own);
    assert!(dumbmail::billing::quote(&h.state, acct, "sms", 2, false, own).await.unwrap() > 0);
    sqlx::query("UPDATE accounts SET sms_mode = 'byo' WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    assert!(dumbmail::billing::quote(&h.state, acct, "sms", 2, false, own).await.unwrap() > 0, "platform send stays billed");
    sqlx::query("UPDATE accounts SET sms_mode = 'platform' WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    assert_eq!(dumbmail::billing::quote(&h.state, acct, "sms", 2, false, true).await.unwrap(), 0);
    assert_eq!(dumbmail::billing::quote(&h.state, acct, "sms", 1, true, true).await.unwrap(), 0);
    assert_eq!(dumbmail::billing::charge_message(&h.state, acct, "sms", 3, false, true).await.unwrap(), 0);

    // Number rent never drives a balance negative: unpaid → past due → released after the grace period.
    sqlx::query("UPDATE accounts SET balance_micros = 1000000 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE phone_numbers SET next_charge_at = now() - interval '1 minute' WHERE account_id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::billing::charge_numbers(&h.state).await.unwrap();
    assert_eq!(dumbmail::billing::balance(&h.state.db, acct).await.unwrap(), 1_000_000, "rent must not be charged into a negative balance");
    let nums = h.ok("GET", "/api/phone-numbers", None).await;
    assert!(nums["data"][0]["release_at"].is_string(), "{nums}");
    let marked: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT past_due_since FROM phone_numbers WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    dumbmail::billing::charge_numbers(&h.state).await.unwrap();
    let again: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT past_due_since FROM phone_numbers WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(marked, again, "a past-due number inside the grace period is left alone");
    // Topping up within the grace period pays the rent and clears the warning.
    sqlx::query("UPDATE accounts SET balance_micros = 5000000 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::billing::charge_numbers(&h.state).await.unwrap();
    assert_eq!(dumbmail::billing::balance(&h.state.db, acct).await.unwrap(), 5_000_000 - 2_200_000);
    assert!(h.ok("GET", "/api/phone-numbers", None).await["data"][0]["release_at"].is_null());
    // Unpaid past the grace period → released.
    sqlx::query("UPDATE accounts SET balance_micros = 0 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE phone_numbers SET next_charge_at = now() - interval '1 minute', past_due_since = now() - interval '8 days' WHERE account_id = $1")
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    dumbmail::billing::charge_numbers(&h.state).await.unwrap();
    assert_eq!(h.ok("GET", "/api/phone-numbers", None).await["data"].as_array().unwrap().len(), 0);
    assert_eq!(dumbmail::billing::balance(&h.state.db, acct).await.unwrap(), 0);
    h.cleanup().await;
}

#[tokio::test]
async fn brand_import_and_google_sign_in() {
    use dumbmail::api::google::{google_user, GoogleClaims};
    let mut h = harness!();
    h.signup().await;
    let old_cookie = h.cookie.clone();

    // Social links and logo are validated.
    let a = h.ok("PATCH", "/api/account", Some(json!({"social_links": {"instagram": "instagram.com/acme", "x": ""}, "logo_url": "https://cdn.acme.test/logo.png"}))).await;
    assert_eq!(a["social_links"], json!({"instagram": "https://instagram.com/acme"}));
    let (s, _) = h.req("PATCH", "/api/account", Some(json!({"social_links": {"myspace": "https://myspace.com/a"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.req("PATCH", "/api/account", Some(json!({"logo_url": "javascript:alert(1)"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Brand import only reads public websites.
    for url in ["http://127.0.0.1:9/", "localhost", "http://169.254.169.254/latest", ""] {
        let (s, v) = h.req("POST", "/api/brand/import", Some(json!({"url": url}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {v}");
    }

    // The sign-in pages learn Google is on; starting sends the browser to Google with PKCE.
    let opts = h.ok("GET", "/api/auth/options", None).await;
    assert_eq!(opts, json!({"google": true, "signup": true}));
    let (s, hdrs, _) = h.raw("GET", "/api/auth/google?next=/campaigns", &[], "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = hdrs.get(header::LOCATION).unwrap().to_str().unwrap().to_string();
    assert!(loc.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{loc}");
    for part in ["client_id=test-client.apps.googleusercontent.com", "redirect_uri=https%3A%2F%2Fpl.test%2Fapi%2Fauth%2Fgoogle%2Fcallback", "code_challenge_method=S256", "scope=openid+email+profile"] {
        assert!(loc.contains(part), "{part} missing from {loc}");
    }
    let oauth_cookie = hdrs.get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    assert!(oauth_cookie.starts_with("pl_oauth="));
    // A callback whose state doesn't match the cookie is refused before anything is sent to Google.
    let (s, hdrs, _) = h.raw("GET", "/api/auth/google/callback?code=abc&state=forged", &[("cookie", &oauth_cookie)], "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(hdrs.get(header::LOCATION).unwrap().to_str().unwrap(), "/login?error=expired");
    let (_, hdrs, _) = h.raw("GET", "/api/auth/google/callback?error=access_denied&state=x", &[], "").await;
    assert_eq!(hdrs.get(header::LOCATION).unwrap().to_str().unwrap(), "/login?error=cancelled");

    // Google with the same email joins the existing account, turns its password off and ends old sessions.
    let claims = |sub: &str, email: &str, given: Option<&str>| GoogleClaims { sub: sub.into(), email: email.into(), name: given.map(|g| format!("{g} Smith")), given_name: given.map(String::from) };
    let account_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "planted"}))).await;
    let key = key["key"].as_str().unwrap().to_string();
    let (uid, acct, created) = google_user(&h.state, &claims("g-owner", "owner@acme.test", Some("Olive"))).await.unwrap();
    assert_eq!((acct, created), (account_id, false));
    assert_eq!(google_user(&h.state, &claims("g-owner", "owner@acme.test", None)).await.unwrap(), (uid, account_id, false));
    h.cookie = old_cookie;
    let (s, _) = h.req("GET", "/api/me", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "old sessions end when Google is linked");
    let (s, v) = h.req("POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "correct horse"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("Google"), "{v}");
    assert!(google_user(&h.state, &claims("g-intruder", "owner@acme.test", None)).await.is_err(), "a second Google account can't take over the email");
    let (s, _, _) = h.raw("GET", "/api/me", &[("authorization", &format!("Bearer {key}"))], "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "API keys made before Google proved the email are revoked");

    // New people get a new account: named after them on free mail, after their domain on a work address.
    let (_, ann_acct, created) = google_user(&h.state, &claims("g-ann", "ann@gmail.com", Some("Ann"))).await.unwrap();
    assert!(created);
    let (name, website): (String, Option<String>) = sqlx::query_as("SELECT name, website FROM accounts WHERE id = $1").bind(ann_acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((name.as_str(), website), ("Ann's brand", None));
    let (_, bob_acct, _) = google_user(&h.state, &claims("g-bob", "bob@stayportland.com", Some("Bob"))).await.unwrap();
    let (name, website): (String, Option<String>) = sqlx::query_as("SELECT name, website FROM accounts WHERE id = $1").bind(bob_acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((name.as_str(), website.as_deref()), ("stayportland.com", Some("https://stayportland.com")));
    h.cleanup().await;
}

#[tokio::test]
async fn public_pages_and_site_verification() {
    let h = harness!();
    for (path, needle) in [("/about", "$0.20 per 1,000"), ("/privacy", "Limited Use"), ("/terms", "CAN-SPAM")] {
        let (s, headers, body) = h.raw("GET", path, &[], "").await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert!(headers[axum::http::header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"), "{path}");
        assert!(body.contains(needle), "{path} should mention {needle}");
        if path == "/about" {
            assert!(body.contains("first 25 emails each month are free"));
        }
        assert!(body.contains("mailto:help@pl.test"), "{path} shows the support address");
        assert!(body.contains("href=\"/privacy\"") && body.contains("href=\"/terms\""), "{path} links the legal pages");
    }
    // The retired hostname sends page visits to PUBLIC_URL but keeps webhooks, API and tracking working.
    let (s, headers, _) = h.raw("GET", "/settings/account?tab=1", &[("host", "old.pl.test")], "").await;
    assert_eq!(s, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers["location"], "https://pl.test/settings/account?tab=1");
    let (s, headers, _) = h.raw("GET", "/api/auth/google", &[("host", "old.pl.test")], "").await;
    assert_eq!(s, StatusCode::PERMANENT_REDIRECT, "sign-in must start on the new host so its cookie matches");
    assert_eq!(headers["location"], "https://pl.test/api/auth/google");
    for path in ["/healthz", "/api/auth/options", "/onsite.js?k=x"] {
        let (s, _, _) = h.raw("GET", path, &[("host", "old.pl.test")], "").await;
        assert_ne!(s, StatusCode::PERMANENT_REDIRECT, "{path} stays on the old host");
    }
    let (s, _, _) = h.raw("POST", "/privacy", &[("host", "old.pl.test")], "").await;
    assert_ne!(s, StatusCode::PERMANENT_REDIRECT, "only GET and HEAD move");
    let (s, _, _) = h.raw("GET", "/privacy", &[("host", "pl.test")], "").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, body) = h.raw("GET", "/google0123abcd.html", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, "google-site-verification: google0123abcd.html");
    // Other google*.html paths fall through to the dashboard, not the verification file.
    let (_, _, body) = h.raw("GET", "/google9999.html", &[], "").await;
    assert!(!body.contains("google-site-verification"));
    h.cleanup().await;
}

// ---------- A/B tests ----------

/// A list of `n` subscribed email profiles (ab0@x.test …), returning the list id.
async fn ab_list(h: &mut Harness, n: usize) -> String {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "AB"}))).await;
    let mut csv = String::from("email\n");
    for i in 0..n {
        csv.push_str(&format!("ab{i}@x.test\n"));
    }
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed"}))).await;
    list["id"].as_str().unwrap().to_string()
}

fn ab_content(version: &str) -> Value {
    json!({"subject": format!("Subject {}", version.to_uppercase()), "blocks": [{"type": "button", "text": "Shop", "href": format!("https://acme.test/{version}")}]})
}

/// An email campaign to the list whose version B has its own subject and link.
fn ab_campaign(list_id: &str, test: Value) -> Value {
    let mut test = test;
    test["variants"] = json!([{"id": "b", "content": ab_content("b")}]);
    json!({"name": "AB", "channel": "email", "audience": {"include": [{"type": "list", "id": list_id}]}, "content": ab_content("a"), "ab_test": test})
}

/// (to_address, variant) for a campaign's real messages.
async fn ab_messages(h: &Harness, cid: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT to_address, variant FROM messages WHERE campaign_id = $1::uuid AND NOT is_test ORDER BY to_address")
        .bind(cid)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

/// The subject each address received for the campaign's subject family ("Subject A"/"Subject B").
fn subject_to(h: &Harness, to: &str) -> Vec<String> {
    h.state.email.sent().into_iter().filter(|m| m.to == to && m.subject.starts_with("Subject ")).map(|m| m.subject).collect()
}

async fn enqueue_duplicate_send(h: &Harness, cid: &str) {
    sqlx::query("INSERT INTO jobs (kind, payload) VALUES ('campaign_send', jsonb_build_object('campaign_id', $1::text))").bind(cid).execute(&h.state.db).await.unwrap();
}

async fn end_test_now(h: &Harness, cid: &str) {
    sqlx::query("UPDATE campaigns SET ab_test_ends_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(cid).execute(&h.state.db).await.unwrap();
}

#[tokio::test]
async fn campaign_ab_test_picks_winner_automatically() {
    let mut h = harness!();
    h.signup().await;
    let list_id = ab_list(&mut h, 20).await;
    let c = h.ok("POST", "/api/campaigns", Some(ab_campaign(&list_id, json!({"test_percent": 50, "wait_hours": 4, "metric": "click_rate", "pick": "auto"})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    assert_eq!(c["ab_test"]["variants"][0]["id"], "b");
    assert_eq!(h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await["status"], "sending");
    h.drain().await;

    // The test slice (half the list) went out split evenly; the campaign waits for results.
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "testing", "{c}");
    assert!(c["ab_winner"].is_null());
    let ends: chrono::DateTime<chrono::Utc> = c["ab_test_ends_at"].as_str().unwrap().parse().unwrap();
    let sent_at: chrono::DateTime<chrono::Utc> = c["sent_at"].as_str().unwrap().parse().unwrap();
    assert_eq!((ends - sent_at).num_hours(), 4);
    let msgs = ab_messages(&h, &cid).await;
    assert_eq!(msgs.len(), 10);
    assert_eq!(msgs.iter().filter(|m| m.1.as_deref() == Some("a")).count(), 5);
    assert_eq!(msgs.iter().filter(|m| m.1.as_deref() == Some("b")).count(), 5);
    for (to, v) in &msgs {
        assert_eq!(subject_to(&h, to), vec![format!("Subject {}", v.as_deref().unwrap().to_uppercase())], "{to} got its version");
    }
    assert_eq!(h.state.email.sent().len(), 10);

    // Four of B's recipients click, one of A's.
    let clickers: Vec<String> = msgs.iter().filter(|m| m.1.as_deref() == Some("b")).take(4).chain(msgs.iter().filter(|m| m.1.as_deref() == Some("a")).take(1)).map(|m| m.0.clone()).collect();
    for to in &clickers {
        let mail = h.state.email.sent().into_iter().find(|m| &m.to == to).unwrap();
        let click = find_link(&mail.html, "https://pl.test/t/c/").replace("https://pl.test", "");
        let (s, _, _) = h.raw("GET", &click, &[], "").await;
        assert_eq!(s, StatusCode::SEE_OTHER);
    }
    // A B clicker orders: revenue is credited to version B.
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": clickers[0]}, "value": 40}))).await;

    let rep = h.ok("GET", &format!("/api/campaigns/{cid}/report"), None).await;
    let ab = &rep["ab_test"];
    assert_eq!(ab["leader"], "b", "{ab}");
    assert!(ab["winner"].is_null());
    assert!(ab["confidence"].as_f64().unwrap() > 0.5, "{ab}");
    assert_eq!(ab["versions"][0]["id"], "a");
    assert_eq!(ab["versions"][0]["stats"]["recipients"], 5);
    assert_eq!(ab["versions"][0]["stats"]["unique_clicks"], 1);
    assert_eq!(ab["versions"][1]["stats"]["unique_clicks"], 4);
    assert_eq!(ab["versions"][1]["stats"]["orders"], 1);
    assert_eq!(ab["versions"][1]["stats"]["revenue"], 40.0);
    assert_eq!(ab["versions"][0]["stats"]["revenue"], 0.0);

    // A testing campaign can't be edited or deleted.
    let (s, _) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"name": "x"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = h.req("DELETE", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Before the wait is over the scheduler leaves it alone.
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "testing");

    // Once it's over, the leader goes to everyone else.
    end_test_now(&h, &cid).await;
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sent", "{c}");
    assert_eq!(c["ab_winner"], "b");
    let msgs = ab_messages(&h, &cid).await;
    assert_eq!(msgs.len(), 20);
    let rest: Vec<_> = msgs.iter().filter(|m| m.1.is_none()).collect();
    assert_eq!(rest.len(), 10);
    for (to, _) in rest {
        assert_eq!(subject_to(&h, to), vec!["Subject B".to_string()], "{to} got the winner");
    }
    assert_eq!(h.state.email.sent().len(), 20, "nobody got two versions");
    let rep = h.ok("GET", &format!("/api/campaigns/{cid}/report"), None).await;
    assert_eq!(rep["ab_test"]["winner"], "b");
    assert_eq!(rep["stats"]["recipients"], 20);
    // A second tick doesn't resend.
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    assert_eq!(h.state.email.sent().len(), 20);
    h.cleanup().await;
}

#[tokio::test]
async fn campaign_ab_test_manual_pick_cancel_and_full_test() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let list_id = ab_list(&mut h, 20).await;

    // Manual pick: the scheduler never picks, the sender does.
    let c = h.ok("POST", "/api/campaigns", Some(ab_campaign(&list_id, json!({"test_percent": 50, "wait_hours": 1, "pick": "manual"})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "a"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "no winner before the test runs");
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "testing");
    end_test_now(&h, &cid).await;
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "testing");
    assert!(c["ab_winner"].is_null());
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "z"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let picked = h.ok("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "a"}))).await;
    assert_eq!(picked["status"], "sending");
    assert_eq!(picked["ab_winner"], "a");
    // Picking queues everyone else in the same step, before any job runs.
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND NOT is_test AND variant IS NULL AND status = 'queued'").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(queued, 10);
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sent");
    let msgs = ab_messages(&h, &cid).await;
    assert_eq!(msgs.len(), 20);
    for (to, _) in msgs.iter().filter(|m| m.1.is_none()) {
        assert_eq!(subject_to(&h, to), vec!["Subject A".to_string()]);
    }
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "b"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}/report"), None).await["ab_test"]["winner"], "a");
    let sent_so_far = h.state.email.sent().len();
    assert_eq!(sent_so_far, 20);

    // Cancelling while testing stops the winner send.
    let c = h.ok("POST", "/api/campaigns", Some(ab_campaign(&list_id, json!({"test_percent": 50, "pick": "auto"})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "testing");
    assert_eq!(h.state.email.sent().len(), sent_so_far + 10);
    assert_eq!(h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await["status"], "cancelled");
    end_test_now(&h, &cid).await;
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "cancelled");
    assert!(c["ab_winner"].is_null());
    assert_eq!(ab_messages(&h, &cid).await.len(), 10);
    assert_eq!(h.state.email.sent().len(), sent_so_far + 10);
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let sent_so_far = h.state.email.sent().len();

    // At 100% everyone is in the test and the campaign is done once it's out.
    let c = h.ok("POST", "/api/campaigns", Some(ab_campaign(&list_id, json!({"test_percent": 100})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sent", "{c}");
    assert!(c["ab_winner"].is_null());
    let msgs = ab_messages(&h, &cid).await;
    assert_eq!(msgs.iter().filter(|m| m.1.as_deref() == Some("a")).count(), 10);
    assert_eq!(msgs.iter().filter(|m| m.1.as_deref() == Some("b")).count(), 10);
    assert_eq!(h.state.email.sent().len(), sent_so_far + 20);
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.cleanup().await;
}

#[tokio::test]
async fn campaign_ab_test_validation() {
    let mut h = harness!();
    h.signup().await;
    let list_id = ab_list(&mut h, 2).await;
    let base = ab_campaign(&list_id, json!({}));
    // A variant with empty content gets the fields the editor needs.
    let mut sparse = base.clone();
    sparse["ab_test"] = json!({"variants": [{"id": "b", "content": {}}], "metric": "revenue"});
    let c = h.ok("POST", "/api/campaigns", Some(sparse)).await;
    assert_eq!(c["ab_test"]["variants"][0]["content"], json!({"subject": "", "blocks": []}));
    assert!(h.ok("GET", &format!("/api/campaigns/{}/report", c["id"].as_str().unwrap()), None).await["ab_test"]["confidence"].is_null(), "no confidence for revenue");
    let bad = [
        json!({"variants": [{"id": "c", "content": {"subject": "C"}}]}),
        json!({"variants": [{"id": "b", "content": {"subject": "B"}}], "test_percent": 2}),
        json!({"variants": [{"id": "b", "content": {"subject": "B"}}], "wait_hours": 0}),
        json!({"variants": [{"id": "b", "content": {"subject": "B"}}], "metric": "vibes"}),
        json!({"variants": [{"id": "b", "content": {"subject": "B"}}], "pick": "sometimes"}),
        json!({"variants": [{"id": "b", "content": "B"}]}),
        json!({"variants": []}),
        json!("yes"),
    ];
    for t in bad {
        let mut body = base.clone();
        body["ab_test"] = t.clone();
        let (s, v) = h.req("POST", "/api/campaigns", Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{t}: {v}");
    }
    let (s, v) = h.req("POST", "/api/campaigns", Some(json!({"channel": "sms", "content": {"body": "hi"},
        "ab_test": {"variants": [{"id": "b", "content": {"body": "yo"}}], "metric": "open_rate"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "SMS has no opens: {v}");

    // Only variants given: the settings are filled with defaults.
    let c = h.ok("POST", "/api/campaigns", Some(base.clone())).await;
    let cid = c["id"].as_str().unwrap().to_string();
    assert_eq!(c["ab_test"]["test_percent"], 20);
    assert_eq!(c["ab_test"]["wait_hours"], 4);
    assert_eq!(c["ab_test"]["metric"], "click_rate");
    assert_eq!(c["ab_test"]["pick"], "auto");

    // Test sends can show either version.
    h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["qa@acme.test"], "version": "b"}))).await;
    assert_eq!(h.state.email.sent().last().unwrap().subject, "[Test] Subject B");
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["qa@acme.test"], "version": "d"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Editing other fields keeps the test; the channel change is re-checked against it.
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"name": "Renamed"}))).await;
    assert_eq!(c["ab_test"]["variants"][0]["id"], "b");
    h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": {"variants": [{"id": "b", "content": ab_content("b")}], "metric": "open_rate"}}))).await;
    let (s, _) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"channel": "sms"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": {"variants": [{"id": "b", "content": {}}], "test_percent": 101}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Duplicates carry the test.
    let copy = h.ok("POST", &format!("/api/campaigns/{cid}/duplicate"), None).await;
    assert_eq!(copy["ab_test"]["metric"], "open_rate");

    // A version that isn't ready blocks the send.
    h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": {"variants": [{"id": "b", "content": {"subject": "", "blocks": []}}]}}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("Version B: Add a subject line"), "{v}");

    // null removes the test.
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": null}))).await;
    assert!(c["ab_test"].is_null(), "{c}");
    assert!(h.ok("GET", &format!("/api/campaigns/{cid}/report"), None).await["ab_test"].is_null());

    // Content types are checked; custom-HTML versions don't get blocks added.
    for bad in [json!({"subject": "B", "blocks": "x"}), json!({"subject": null}), json!({"subject": "B", "html": 5})] {
        let (s, v) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": {"variants": [{"id": "b", "content": bad}]}}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": {"variants": [{"id": "b", "content": {"subject": "Subject B", "html": "<p>B</p>"}}]}}))).await;
    assert_eq!(c["ab_test"]["variants"][0]["content"], json!({"subject": "Subject B", "html": "<p>B</p>"}));
    let mut sms = base.clone();
    sms["channel"] = json!("sms");
    sms["content"] = json!({"body": "A. Reply STOP to opt out."});
    sms["ab_test"] = json!({"variants": [{"id": "b", "content": {"body": null}}]});
    let (s, v) = h.req("POST", "/api/campaigns", Some(sms)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // A duplicated send job can't send anyone a second email.
    h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"ab_test": null}))).await;
    let before = h.state.email.sent().len();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    enqueue_duplicate_send(&h, &cid).await;
    h.drain().await;
    assert_eq!(h.state.email.sent().len() - before, 2, "two recipients, one email each");
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");

    // A message still claimed by another worker holds the campaign open; an abandoned claim is failed, not resent.
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
    let mid: uuid::Uuid = sqlx::query_scalar("UPDATE messages SET status = 'sending', claimed_at = now() WHERE id = (SELECT id FROM messages WHERE campaign_id = $1::uuid AND NOT is_test LIMIT 1) RETURNING id").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    enqueue_duplicate_send(&h, &cid).await;
    h.drain().await;
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sending", "an active claim keeps it open");
    sqlx::query("UPDATE messages SET claimed_at = now() - interval '1 hour' WHERE id = $1").bind(mid).execute(&h.state.db).await.unwrap();
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");
    let status: String = sqlx::query_scalar("SELECT status FROM messages WHERE id = $1").bind(mid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "failed");
    assert_eq!(h.state.email.sent().len() - before, 2, "nothing resent");
    h.cleanup().await;
}

#[tokio::test]
async fn flow_random_split() {
    let mut h = harness!();
    h.signup().await;
    let split = |percent: Option<Value>| {
        let mut node = json!({"id": "s", "type": "random_split", "yes": "pa", "no": "pb"});
        if let Some(p) = percent {
            node["percent"] = p;
        }
        json!({"name": "Split", "trigger": {"type": "metric", "metric": "Split Me"}, "definition": {"entry": "s", "nodes": [
            node,
            {"id": "pa", "type": "update_profile", "properties": {"path": "a"}, "next": null},
            {"id": "pb", "type": "update_profile", "properties": {"path": "b"}, "next": null}]}})
    };
    for p in [Some(json!(0)), Some(json!(100)), None, Some(json!(50.5)), Some(json!("50")), Some(json!(-5))] {
        let (s, v) = h.req("POST", "/api/flows", Some(split(p.clone()))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{p:?}: {v}");
    }
    let mut dangling = split(Some(json!(50)));
    dangling["definition"]["nodes"][0]["no"] = json!("nowhere");
    let (s, _) = h.req("POST", "/api/flows", Some(dangling)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let f = h.ok("POST", "/api/flows", Some(split(Some(json!(50))))).await;
    let fid = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    for i in 0..20 {
        h.ok("POST", "/api/events", Some(json!({"metric": "Split Me", "profile": {"email": format!("s{i}@x.test")}}))).await;
    }
    h.drain().await;
    let paths: Vec<(Option<String>, i64)> = sqlx::query_as("SELECT properties->>'path', count(*) FROM profiles WHERE email LIKE 's%@x.test' GROUP BY 1 ORDER BY 1")
        .fetch_all(&h.state.db)
        .await
        .unwrap();
    let count = |p: &str| paths.iter().find(|x| x.0.as_deref() == Some(p)).map_or(0, |x| x.1);
    // With 20 people the chance one path gets nobody is about 2 in a million.
    assert!(count("a") > 0 && count("b") > 0, "{paths:?}");
    assert_eq!(count("a") + count("b"), 20, "{paths:?}");
    let rep = h.ok("GET", &format!("/api/flows/{fid}/report"), None).await;
    assert_eq!(rep["total_runs"], 20);
    assert_eq!(rep["active_runs"], 0);
    let entered = |id: &str| rep["nodes"].as_array().unwrap().iter().find(|n| n["node_id"] == id).unwrap()["entered"].as_i64().unwrap();
    assert_eq!(entered("s"), 20);
    assert_eq!(entered("pa"), count("a"));
    assert_eq!(entered("pb"), count("b"));
    h.cleanup().await;
}

#[tokio::test]
async fn form_ab_test() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Signups"}))).await;
    let form_body = |variants: Value| json!({"name": "Popup", "list_id": list["id"], "config": {"headline": "Headline A", "variants": variants}});

    for bad in [
        json!([{"id": "c", "headline": "C"}]),
        json!([{"id": "b", "brand_color": "#000000"}]),
        json!([{"id": "b", "headline": 5}]),
        json!([{"id": "b", "image_url": "javascript:alert(1)"}]),
        json!(["b"]),
        json!("b"),
        json!([{"id": "b"}, {"id": "c"}, {"id": "d"}, {"id": "e"}]),
    ] {
        let (s, v) = h.req("POST", "/api/forms", Some(form_body(bad.clone()))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    let form = h.ok("POST", "/api/forms", Some(form_body(json!([{"id": "b", "headline": "Headline B", "body": ""}])))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    assert_eq!(form["variant_stats"], json!([{"id": "a", "views": 0, "submits": 0, "submit_rate": 0.0}, {"id": "b", "views": 0, "submits": 0, "submit_rate": 0.0}]));
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;

    // Views count per version; unknown versions only count toward the total.
    for v in ["?v=a", "?v=b", "?v=b", "?v=z", ""] {
        let (s, _, _) = h.raw("POST", &format!("/client/forms/{fid}/view{v}"), &[], "").await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }
    let sub = |email: &str, variant: &str| json!({"form_id": fid, "email": email, "variant": variant}).to_string();
    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &sub("fb@x.test", "b")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _, _) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &sub("fz@x.test", "z")).await;
    assert_eq!(s, StatusCode::OK);
    let db = h.state.db.clone();
    let version_of = move |email: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT e.properties->>'form_version' FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = 'Subscribed to List' AND p.email = $1")
                .bind(email)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(version_of("fb@x.test").await.as_deref(), Some("b"));
    assert_eq!(version_of("fz@x.test").await, None);

    let f = h.ok("GET", &format!("/api/forms/{fid}"), None).await;
    assert_eq!(f["stats"], json!({"views": 5, "submits": 2}));
    assert_eq!(f["variant_stats"][0], json!({"id": "a", "views": 1, "submits": 0, "submit_rate": 0.0}));
    assert_eq!(f["variant_stats"][1], json!({"id": "b", "views": 2, "submits": 1, "submit_rate": 0.5}));

    // The hosted page shows one version and posts it back with the sign-up.
    let (s, _, html) = h.raw("GET", &format!("/f/{fid}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    let shown = if html.contains(r#"<input type="hidden" name="variant" value="b">"#) {
        "b"
    } else {
        assert!(html.contains(r#"<input type="hidden" name="variant" value="a">"#), "{html}");
        "a"
    };
    assert!(html.contains(&format!("<h1>Headline {}</h1>", shown.to_uppercase())), "{html}");
    assert!(html.contains("Join our list"), "an empty variant field falls back to the form's own");
    let (s, _, _) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/x-www-form-urlencoded")], &format!("form_id={fid}&email=fh%40x.test&variant={shown}")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(version_of("fh@x.test").await.as_deref(), Some(shown));
    let f = h.ok("GET", &format!("/api/forms/{fid}"), None).await;
    let i = if shown == "a" { 0 } else { 1 };
    let (views, submits) = if shown == "a" { (2, 1) } else { (3, 2) };
    assert_eq!(f["variant_stats"][i]["views"], views, "{f}");
    assert_eq!(f["variant_stats"][i]["submits"], submits, "{f}");
    assert_eq!(f["stats"], json!({"views": 6, "submits": 3}));
    let listed = h.ok("GET", "/api/forms", None).await;
    assert_eq!(listed["data"][0]["variant_stats"], f["variant_stats"]);

    // Edits that leave a version's content alone keep its counts; changing what it shows starts it over.
    let f = h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"success_message": "Welcome!"}}))).await;
    assert_eq!(f["variant_stats"][1]["submits"], if shown == "b" { 2 } else { 1 });
    assert_eq!(f["variant_stats"][0]["views"], if shown == "a" { 2 } else { 1 });
    let f = h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"headline": "New A"}}))).await;
    assert_eq!(f["variant_stats"][0], json!({"id": "a", "views": 0, "submits": 0, "submit_rate": 0.0}), "A changed");
    assert_eq!(f["variant_stats"][1]["submits"], if shown == "b" { 2 } else { 1 }, "B overrides the headline, so it still shows the same");
    let f = h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"variants": [{"id": "b", "headline": "Headline B", "button_text": "Join"}]}}))).await;
    assert_eq!(f["variant_stats"][1], json!({"id": "b", "views": 0, "submits": 0, "submit_rate": 0.0}), "B changed");
    // Ending the test drops every count.
    let f = h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"variants": []}}))).await;
    assert_eq!(f["variant_stats"], json!([]));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM form_variant_stats WHERE form_id = $1::uuid").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(rows, 0);
    let (_, _, html) = h.raw("GET", &format!("/f/{fid}"), &[], "").await;
    assert!(!html.contains(r#"name="variant""#));
    assert!(html.contains("<h1>New A</h1>"));
    h.cleanup().await;
}

#[tokio::test]
async fn website_setup_test_and_late_events() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let w = h.ok("GET", "/api/website", None).await;
    assert!(w["shopify_pixel"].as_str().unwrap().contains(&pk), "pixel carries the account key");
    assert!(w["shopify_pixel"].as_str().unwrap().contains("checkout_completed"));

    // Starting a test defaults to the account website and gives a link that turns test mode on.
    let t = h.ok("POST", "/api/website/tests", Some(json!({}))).await;
    let code = t["code"].as_str().unwrap().to_string();
    assert_eq!(t["url"], "https://acme.test/");
    assert_eq!(t["open_url"], format!("https://acme.test/?dumbmail_test={code}"));
    assert_eq!(t["active"], true);
    let (s, _) = h.req("POST", "/api/website/tests", Some(json!({"url": "not a site"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // The script only enters test mode for a live test of the account its key belongs to.
    let (_, _, body) = h.raw("GET", &format!("/client/test?k={pk}&code={code}"), &[], "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["active"], true);
    let (_, _, body) = h.raw("GET", &format!("/client/test?k={pk}&code=madeUpCode123"), &[], "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["active"], false);
    let (_, _, body) = h.raw("GET", &format!("/client/test?k=pk_other&code={code}"), &[], "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["active"], false);

    let report = |check: &str, detail: Value| json!({"code": code, "check": check, "detail": detail, "url": "https://acme.test/products/mug"}).to_string();
    let ct = [("content-type", "application/json")];
    let (s, _, _) = h.raw("POST", &format!("/client/test?k={pk}"), &ct, &report("loaded", json!({"platform": "shopify", "summary": "On acme.test"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = h.raw("POST", &format!("/client/test?k={pk}"), &ct, &report("product", json!({"summary": "Mug · $18"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = h.raw("POST", &format!("/client/test?k={pk}"), &ct, &report("anything", json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "only known checks");
    let (s, _, _) = h.raw("POST", &format!("/client/test?k={pk}"), &ct, &report("product", json!({"summary": "x".repeat(5000)}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // A snippet from another account is flagged, not recorded as a pass.
    let (s, _, _) = h.raw("POST", "/client/test?k=pk_someone_else", &ct, &report("cart", json!({"summary": "Mug"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let t = h.ok("GET", &format!("/api/website/tests/{code}"), None).await;
    assert_eq!(t["checks"]["loaded"]["platform"], "shopify");
    assert_eq!(t["checks"]["product"]["summary"].as_str().unwrap().len(), 300, "long text is trimmed");
    let (s, _, _) = h.raw("POST", &format!("/client/test?k={pk}"), &ct, &report("forms", json!({"summary": "y".repeat(20_000)}))).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "reports are small");
    assert!(t["checks"]["cart"].is_null());
    assert_eq!(t["checks"]["wrong_key"]["key"], "pk_someone_e");
    assert_eq!(t["last_url"], "https://acme.test/products/mug");
    let site = h.ok("GET", "/api/website", None).await;
    assert_eq!(site["installed"], true, "a test report counts as installed");

    // Expired tests and other accounts' tests accept nothing.
    sqlx::query("UPDATE site_tests SET expires_at = now() - interval '1 minute' WHERE code = $1").bind(&code).execute(&h.state.db).await.unwrap();
    let (s, _, _) = h.raw("POST", &format!("/client/test?k={pk}"), &ct, &report("cart", json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(h.ok("GET", &format!("/api/website/tests/{code}"), None).await["active"], false);
    let (s, _) = h.req("GET", "/api/website/tests/nope", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Events the browser held until it knew the visitor keep their original time (within a week) and dedupe by unique_id.
    let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp_millis();
    for _ in 0..2 {
        let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &ct,
            &json!({"metric": "Placed Order", "profile": {"email": "late@x.test"}, "properties": {"order_id": "1001"}, "value": 18, "unique_id": "order:1001", "time": two_hours_ago}).to_string()).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &ct,
        &json!({"metric": "Viewed Product", "profile": {"email": "late@x.test"}, "time": 1_000_000_000_000_i64}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT m.name, EXTRACT(EPOCH FROM now() - e.occurred_at)::float8 FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE p.email = 'late@x.test' ORDER BY m.name")
        .fetch_all(&h.state.db).await.unwrap();
    assert_eq!(rows.len(), 2, "duplicate order dropped: {rows:?}");
    assert!((rows[0].1 - 7200.0).abs() < 120.0, "held order keeps its time: {rows:?}");
    assert!(rows[1].1 < 60.0, "an old time is recorded as now: {rows:?}");
    h.cleanup().await;
}

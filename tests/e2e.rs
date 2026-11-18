//! End-to-end tests against a real Postgres. Set TEST_DATABASE_URL to an admin connection
//! (e.g. postgres://postgres@127.0.0.1:55432/postgres); each test creates and drops its own database.

#[macro_use]
mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use dumbmail::jobs;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::*;

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
    let imp = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list_id, "email_consent": "subscribed", "permission_confirmed": true}))).await;
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
    assert_eq!(ov["totals"]["attributed_orders"], 1);
    assert_eq!(ov["totals"]["email_clicks"], 1);
    assert_eq!(ov["top_campaigns"][0]["orders"], 1);
    // Nothing happened in the 30 days before, so the comparison window is empty.
    assert_eq!(ov["previous"]["emails_sent"], 0);
    assert_eq!(ov["previous"]["attributed_revenue"], 0.0);
    assert_eq!(ov["series"].as_array().unwrap().last().unwrap()["campaign_revenue"], 120.5);
    // The flows summary counts flow revenue only; this order belongs to the campaign.
    let fa = h.ok("GET", "/api/analytics/flows?days=30", None).await;
    assert_eq!(fa["totals"]["revenue"], 0.0);
    assert_eq!(fa["totals"]["store_revenue"], 120.5);
    assert_eq!(fa["totals"]["messages_sent"], 0);
    let metrics = h.ok("GET", "/api/metrics", None).await;
    let order = metrics["data"].as_array().unwrap().iter().find(|m| m["name"] == "Placed Order").unwrap().clone();
    assert_eq!(order["has_value"], true);
    assert_eq!(order["count_prev_30d"], 0);
    let trend = order["trend"].as_array().unwrap();
    assert_eq!(trend.len(), 30);
    assert_eq!(trend[29], 1, "today's order lands in the last bucket");
    let events = h.ok("GET", "/api/events?metric=Placed%20Order", None).await;
    assert_eq!(events["data"][0]["profile_label"], "bob@x.test");

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
    let fa = h.ok("GET", "/api/analytics/flows?days=30", None).await;
    assert_eq!(fa["totals"]["messages_sent"], 3);
    assert_eq!(fa["totals"]["recipients"], 1);
    assert_eq!(fa["previous"]["messages_sent"], 0);
    assert_eq!(fa["series"].as_array().unwrap().iter().map(|d| d["messages_sent"].as_i64().unwrap()).sum::<i64>(), 3);

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
    let (_, fa) = call(&h, tool(16, "get_analytics", json!({"days": 7, "scope": "flows"}))).await;
    assert!(fa["result"]["structuredContent"]["totals"]["revenue_per_recipient"].is_number(), "{fa}");
    let (_, fo) = call(&h, tool(16, "get_analytics", json!({"days": 7, "scope": "forms"}))).await;
    assert!(fo["result"]["structuredContent"]["totals"]["submit_rate"].is_number(), "{fo}");
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

    // Only keys with the account scope can mint more keys (tests/team.rs covers scopes); revoked keys stop working.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "k", "scopes": ["audience", "messaging"]}))).await;
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
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
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
    use dumbmail::api::google::{google_user, GoogleClaims, GoogleUser};
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
    assert_eq!(opts, json!({"google": true, "signup": true, "site_url": null}));
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

    // Google with the same email matches an existing login only for an address Google is the
    // authority for (a Workspace `hd` here), and never changes it on the spot: an unconfirmed login
    // goes to the choice page, a confirmed one is linked once its sign-in is complete (see
    // tests/login_link.rs). Without `hd` the login's password is the way in.
    let claims = |sub: &str, email: &str, given: Option<&str>| {
        let domain = email.rsplit('@').next().unwrap();
        let hd = (domain != "gmail.com").then(|| domain.to_string());
        GoogleClaims { sub: sub.into(), email: email.into(), name: given.map(|g| format!("{g} Smith")), given_name: given.map(String::from), hd }
    };
    let account_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let uid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = 'owner@acme.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(google_user(&h.state, &claims("g-owner", "owner@acme.test", Some("Olive"))).await.unwrap(), GoogleUser::Unconfirmed(uid));
    let mut no_hd = claims("g-owner", "owner@acme.test", None);
    no_hd.hd = None;
    assert_eq!(google_user(&h.state, &no_hd).await.unwrap(), GoogleUser::UsePassword);
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE id = $1").bind(uid).execute(&h.state.db).await.unwrap();
    assert!(matches!(google_user(&h.state, &claims("g-owner", "owner@acme.test", None)).await.unwrap(), GoogleUser::Link { user_id, account_id: a, .. } if user_id == uid && a == account_id));
    let linked: Option<String> = sqlx::query_scalar("SELECT google_sub FROM users WHERE id = $1").bind(uid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(linked, None, "nothing is linked until the sign-in completes");
    h.cookie = old_cookie;
    let (s, _) = h.req("GET", "/api/me", None).await;
    assert_eq!(s, StatusCode::OK, "nothing about the login changed");

    // New people get a new account: named after them on free mail, after their domain on a work address.
    let GoogleUser::SignedIn(_, ann_acct, created) = google_user(&h.state, &claims("g-ann", "ann@gmail.com", Some("Ann"))).await.unwrap() else { panic!("new Google user") };
    assert!(created);
    let (name, website): (String, Option<String>) = sqlx::query_as("SELECT name, website FROM accounts WHERE id = $1").bind(ann_acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((name.as_str(), website), ("Ann's brand", None));
    let GoogleUser::SignedIn(_, bob_acct, _) = google_user(&h.state, &claims("g-bob", "bob@stayportland.com", Some("Bob"))).await.unwrap() else { panic!("new Google user") };
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

// ---------- Marketing site and app on separate hosts ----------

#[tokio::test]
async fn one_host_keeps_dashboard_at_root_and_landing_at_home() {
    let h = harness!();
    let (s, _, body) = h.raw("GET", "/", &[("host", "pl.test")], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!body.contains("Postage, not rent."), "/ stays the dashboard without SITE_URL");
    let (s, _, body) = h.raw("GET", "/home", &[("host", "pl.test")], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Postage, not rent."));
    assert!(body.contains("href=\"/signup\""), "links stay relative on one host");
    assert!(body.contains("$0.20 per 1,000") || body.contains(">$0.20<"), "prices come from config");
    assert!(body.contains("<link rel=\"canonical\" href=\"https://pl.test/home\">"));
    let (s, _, body) = h.raw("GET", "/pricing", &[("host", "pl.test")], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("30,000 emails") && body.contains("$10<small>"), "plans come from the subscription tiers");
    for path in ["/about", "/privacy", "/terms"] {
        let (s, _, _) = h.raw("GET", path, &[("host", "pl.test")], "").await;
        assert_eq!(s, StatusCode::OK, "{path}");
    }
    let (s, headers, _) = h.raw("GET", "/site/og.png", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(headers[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));
    // Host-only cookies without COOKIE_DOMAIN.
    let (s, headers, _) = h
        .raw("POST", "/api/auth/signup", &[("content-type", "application/json")], r#"{"email":"solo@acme.test","password":"correct horse","company":"Solo"}"#)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!headers[header::SET_COOKIE].to_str().unwrap().contains("Domain="));
    h.cleanup().await;
}

#[tokio::test]
async fn site_host_and_app_host_split() {
    let Some(h) = Harness::with(|c| {
        c.public_url = "https://app.pl.test".into();
        c.site_url = Some("https://pl.test/".into());
        c.cookie_domain = Some("pl.test".into());
        c.google_redirect_base = Some("https://pl.test".into());
    })
    .await
    else {
        return;
    };
    let site = [("host", "pl.test")];
    let app = [("host", "app.pl.test")];

    // The site host serves the landing page and the site pages.
    let (s, _, body) = h.raw("GET", "/", &site, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Postage, not rent."));
    assert!(body.contains("href=\"https://app.pl.test/signup\"") && body.contains("href=\"https://app.pl.test/login\""), "sign-up and log-in go to the app");
    assert!(body.contains("<link rel=\"canonical\" href=\"https://pl.test/\">"));
    assert!(body.contains("first 25 emails") || body.contains("First 25 free"), "free emails come from config");
    for path in ["/about", "/privacy", "/terms", "/pricing", "/robots.txt", "/sitemap.xml", "/site/og.png", "/google0123abcd.html"] {
        let (s, _, _) = h.raw("GET", path, &site, "").await;
        assert_eq!(s, StatusCode::OK, "{path} on the site host");
    }
    // Other page visits move to the app, keeping path and query.
    for path in ["/login?next=/campaigns", "/signup", "/campaigns", "/forms/123?tab=design", "/settings/account"] {
        let (s, headers, _) = h.raw("GET", path, &site, "").await;
        assert_eq!(s, StatusCode::FOUND, "{path}");
        assert_eq!(headers["location"], format!("https://app.pl.test{path}"));
    }
    // Everything existing clients and links use keeps working on the site host.
    for path in ["/api/auth/options", "/healthz", "/onsite.js?k=x", "/client/dumbmail.js?k=x", "/t/o/bad-token", "/t/c/bad-token", "/u/bad-token", "/c/bad-token", "/f/bad-id", "/webhooks/stripe", "/mcp"] {
        let (s, _, _) = h.raw("GET", path, &site, "").await;
        assert_ne!(s, StatusCode::FOUND, "{path} stays on the site host");
        assert_ne!(s, StatusCode::PERMANENT_REDIRECT, "{path} stays on the site host");
    }
    let (s, _, body) = h.raw("GET", "/api/auth/options", &site, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("\"site_url\":\"https://pl.test\""));
    let (s, _, _) = h.raw("POST", "/webhooks/stripe", &site, "{}").await;
    assert_ne!(s, StatusCode::FOUND, "webhooks aren't redirected");
    // Google sends people back to the site host; the callback finishes there and lands in the app.
    let (s, headers, _) = h.raw("GET", "/api/auth/google/callback?error=access_denied&state=x", &site, "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "https://app.pl.test/login?error=cancelled");

    // The app host sends site pages to the site and keeps the dashboard at /.
    for path in ["/about", "/privacy", "/terms", "/pricing"] {
        let (s, headers, _) = h.raw("GET", &format!("{path}?ref=app"), &app, "").await;
        assert_eq!(s, StatusCode::FOUND, "{path}");
        assert_eq!(headers["location"], format!("https://pl.test{path}?ref=app"));
    }
    let (s, _, body) = h.raw("GET", "/", &app, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!body.contains("Postage, not rent."), "the app host's / is the dashboard");
    let (_, headers, _) = h.raw("GET", "/home", &app, "").await;
    assert_eq!(headers["location"], "https://pl.test/");

    // Sign-in starts on the app host; its state cookie and Google's redirect URI reach the site host.
    let (s, headers, _) = h.raw("GET", "/api/auth/google?next=/campaigns", &app, "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = headers[header::LOCATION].to_str().unwrap();
    assert!(loc.contains("redirect_uri=https%3A%2F%2Fpl.test%2Fapi%2Fauth%2Fgoogle%2Fcallback"), "{loc}");
    let oauth = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(oauth.starts_with("pl_oauth=") && oauth.contains("Domain=pl.test"), "{oauth}");
    // A forged state is refused, and the error lands on the app's login page.
    let cookie = oauth.split(';').next().unwrap().to_string();
    let (_, headers, _) = h.raw("GET", "/api/auth/google/callback?code=abc&state=forged", &[("host", "pl.test"), ("cookie", &cookie)], "").await;
    assert_eq!(headers["location"], "https://app.pl.test/login?error=expired");

    // Session cookies cover both hosts, and logging out clears that same cookie.
    let (s, headers, body) = h
        .raw("POST", "/api/auth/signup", &[("host", "app.pl.test"), ("content-type", "application/json")], r#"{"email":"split@acme.test","password":"correct horse","company":"Split"}"#)
        .await;
    assert_eq!(s, StatusCode::OK);
    let account = serde_json::from_str::<Value>(&body).unwrap()["account"]["id"].as_str().unwrap().to_string();
    let set = headers[header::SET_COOKIE].to_str().unwrap().to_string();
    assert!(set.starts_with("pl_session=") && set.contains("Domain=pl.test"), "{set}");
    let session = set.split(';').next().unwrap().to_string();
    let (s, _, _) = h.raw("GET", "/api/me", &[("host", "pl.test"), ("cookie", &session)], "").await;
    assert_eq!(s, StatusCode::OK, "the session works on the site host's API too");
    // Signing out is a write with the session cookie: the dashboard sends it from the app host,
    // naming its account. From the marketing site it's refused.
    let (s, _, _) = h.raw("POST", "/api/auth/logout", &[("host", "app.pl.test"), ("cookie", &session), ("origin", "https://pl.test"), ("x-dumbmail-account", &account)], "").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, headers, _) = h.raw("POST", "/api/auth/logout", &[("host", "app.pl.test"), ("cookie", &session), ("origin", "https://app.pl.test"), ("x-dumbmail-account", &account)], "").await;
    assert_eq!(s, StatusCode::OK);
    let cleared = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cleared.starts_with("pl_session=;") && cleared.contains("Domain=pl.test") && cleared.contains("Max-Age=0"), "{cleared}");
    h.cleanup().await;
}

#[tokio::test]
async fn public_files_point_back_to_the_host_that_served_them() {
    let Some(mut h) = Harness::with(|c| {
        c.public_url = "https://app.pl.test".into();
        c.site_url = Some("https://pl.test".into());
    })
    .await
    else {
        return;
    };
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter"}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Page", "kind": "page", "list_id": list["id"]}))).await;
    let form_id = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{form_id}"), Some(json!({"status": "live"}))).await;

    // Served on the site host, everything points back to the site host only, so snippets installed
    // before the app moved keep talking to dumbmail.ai alone.
    for (host, base, other) in [("pl.test", "https://pl.test", "app.pl.test"), ("app.pl.test", "https://app.pl.test", "\"https://pl.test")] {
        let (s, _, js) = h.raw("GET", &format!("/onsite.js?k={pk}"), &[("host", host)], "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(js.contains(&format!("\"{base}\"")) && !js.contains(other), "onsite.js on {host}");
        let (s, _, js) = h.raw("GET", &format!("/f/{form_id}/embed.js"), &[("host", host)], "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(js.contains(&format!("\"{base}\"")) && !js.contains(other), "embed.js on {host}");
        let (s, _, html) = h.raw("GET", &format!("/f/{form_id}"), &[("host", host)], "").await;
        assert_eq!(s, StatusCode::OK, "hosted form on {host}");
        assert!(html.contains(&format!("{base}/client/subscribe?k={pk}")), "subscribe URL on {host}");
        assert!(html.contains(&format!("<script src=\"{base}/client/form-render.js\">")), "renderer on {host}");
        assert!(!html.contains(other), "hosted form on {host} only names {base}");
    }
    // Any other Host header gets PUBLIC_URL, never itself.
    let (_, _, js) = h.raw("GET", &format!("/onsite.js?k={pk}"), &[("host", "evil.test")], "").await;
    assert!(js.contains("\"https://app.pl.test\"") && !js.contains("evil.test"));
    let (_, _, html) = h.raw("GET", &format!("/f/{form_id}"), &[("host", "evil.test")], "").await;
    assert!(html.contains("https://app.pl.test/client/form-render.js") && !html.contains("evil.test"));

    // www. + the site host moves permanently to SITE_URL; other methods aren't redirected.
    let (s, headers, _) = h.raw("GET", "/pricing?ref=x", &[("host", "www.pl.test")], "").await;
    assert_eq!(s, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(headers["location"], "https://pl.test/pricing?ref=x");
    let (s, headers, _) = h.raw("HEAD", "/", &[("host", "www.pl.test")], "").await;
    assert_eq!(s, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(headers["location"], "https://pl.test/");
    let (s, _, _) = h.raw("POST", "/webhooks/stripe", &[("host", "www.pl.test")], "{}").await;
    assert_ne!(s, StatusCode::MOVED_PERMANENTLY);
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
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
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
    // The Forms page summary counts the same views and sign-ups, by day.
    let fa = h.ok("GET", "/api/analytics/forms?days=7", None).await;
    assert_eq!(fa["totals"], json!({"views": 5, "submits": 2, "submit_rate": 0.4}));
    assert_eq!(fa["previous"]["views"], 0);
    assert_eq!(fa["series"].as_array().unwrap().last().unwrap()["submits"], 2);
    assert_eq!(fa["forms"][0]["id"], fid.as_str());
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
    let f = h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"legal_text": "No spam."}}))).await;
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

// ---------- Automatic email pause ----------

/// A list of `n` subscribed email profiles named `{prefix}{i}@x.test`, returning the list id.
async fn email_list(h: &mut Harness, prefix: &str, n: usize) -> String {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": prefix}))).await;
    let mut csv = String::from("email\n");
    for i in 0..n {
        csv.push_str(&format!("{prefix}{i}@x.test\n"));
    }
    let imp = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(imp["created"], n, "{imp}");
    list["id"].as_str().unwrap().to_string()
}

/// Create an email campaign to the list and start sending it (jobs not yet run). Returns its id.
async fn start_email_campaign(h: &mut Harness, list_id: &str, name: &str) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": name, "channel": "email", "audience": {"include": [{"type": "list", "id": list_id}]},
        "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    let id = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{id}/send"), None).await;
    id
}

/// Provider ids of `n` sent messages of a campaign.
async fn sent_ids(h: &Harness, cid: &str, n: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE campaign_id = $1::uuid AND status = 'sent' AND NOT is_test ORDER BY to_address LIMIT $2")
        .bind(cid)
        .bind(n)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

/// Deliver an SES bounce ("Permanent" or "Transient") or complaint ("Complaint") through the webhook.
async fn ses_event(h: &Harness, provider_id: &str, kind: &str) {
    let msg = if kind == "Complaint" {
        json!({"notificationType": "Complaint", "mail": {"messageId": provider_id}, "complaint": {}})
    } else {
        json!({"notificationType": "Bounce", "mail": {"messageId": provider_id}, "bounce": {"bounceType": kind, "bounceSubType": "General"}})
    };
    let sns = json!({"Type": "Notification", "Message": msg.to_string()});
    let (s, _, body) = h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &sns.to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

/// Bounce these messages, then let the scheduler run the rate checks the webhooks put off.
async fn bounce_all(h: &Harness, ids: &[String], kind: &str) {
    for id in ids {
        ses_event(h, id, kind).await;
    }
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
}

async fn paused(h: &mut Harness) -> Option<String> {
    let me = h.ok("GET", "/api/me", None).await;
    me["account"]["sending_paused_at"].as_str().map(|_| me["account"]["sending_paused_reason"].as_str().unwrap_or("").to_string())
}

const ADMIN: (&str, &str) = ("authorization", "Bearer test-admin-token-0123456789abcdef0123");

#[tokio::test]
async fn account_pauses_past_the_bounce_threshold_and_operator_lifts_it() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;

    // A small sender: half of 60 emails bounce, but 60 is below every minimum, so nothing pauses.
    let small = email_list(&mut h, "small", 60).await;
    let cid = start_email_campaign(&mut h, &small, "Small").await;
    h.drain().await;
    bounce_all(&h, &sent_ids(&h, &cid, 30).await, "Permanent").await;
    assert_eq!(paused(&mut h).await, None, "60 emails is too few to judge");

    // Another 190 emails: 250 sent. Checks run when a hard bounce or complaint arrives.
    let more = email_list(&mut h, "more", 190).await;
    let cid2 = start_email_campaign(&mut h, &more, "More").await;
    h.drain().await;
    assert_eq!(paused(&mut h).await, None);
    bounce_all(&h, &sent_ids(&h, &cid2, 5).await, "Transient").await;
    assert_eq!(paused(&mut h).await, None, "soft bounces don't count or trigger a check");
    bounce_all(&h, &sent_ids(&h, &cid2, 1).await, "Permanent").await;
    let reason = paused(&mut h).await.expect("12% hard bounces over 250 emails pauses the account");
    assert_eq!(reason, "hard bounce rate 12.4% (31 of 250 emails) in the last 7 days");
    let me = h.ok("GET", "/api/me", None).await;
    assert_eq!(me["support_email"], "help@pl.test");

    // The operator was emailed.
    let alert = h.state.email.sent().into_iter().find(|m| m.to == "ops@pl.test").expect("operator alert");
    assert_eq!(alert.subject, "Sending paused: Acme Goods");
    assert!(alert.text.contains("Owner email: owner@acme.test") && alert.text.contains("/admin/unpause"), "{}", alert.text);
    assert!(alert.text.contains(r#"-d '{"account_id":""#), "the command carries only the account id: {}", alert.text);

    // Paused: test sends, new campaigns and MCP all say so; SMS readiness is unaffected.
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["qa@acme.test"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("Email sending is paused"), "{v}");
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Blocked", "channel": "email", "audience": {"include": [{"type": "list", "id": more}]},
        "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("Email sending is paused"), "{v}");
    let before = h.state.email.sent().len();
    h.drain().await;
    assert_eq!(h.state.email.sent().len(), before, "nothing was sent while paused");

    // A campaign scheduled before the pause stops when it comes due; its recipients are skipped with a reason.
    sqlx::query("UPDATE campaigns SET status = 'scheduled', send_at = now() WHERE id = $1::uuid").bind(c["id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();
    jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    let stopped = h.ok("GET", &format!("/api/campaigns/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(stopped["status"], "failed", "{stopped}");
    assert!(stopped["error"].as_str().unwrap().contains("Email sending is paused"), "{stopped}");
    assert_eq!(h.state.email.sent().len(), before);

    // A live welcome flow records its email as skipped, with the reason, instead of sending it.
    let flow = h.ok("POST", "/api/flows/from-blueprint", Some(json!({"key": "welcome_series"}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "email_consent": "subscribed", "list_ids": [flow["trigger"]["list_id"]]}))).await;
    h.drain().await;
    assert_eq!(h.state.email.sent().len(), before);
    let row: (String, Option<String>) = sqlx::query_as("SELECT status, error FROM messages WHERE to_address = 'new@x.test' AND flow_id IS NOT NULL")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row, ("skipped".to_string(), Some("sending paused".to_string())));

    // Lifting the pause: wrong token refused, then the right one. Old bounces no longer count.
    let body = json!({"email": "owner@acme.test"}).to_string();
    let (s, _, _) = h.raw("POST", "/admin/unpause", &[("authorization", "Bearer wrong-token-0123456789abcdef01234567"), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(paused(&mut h).await.is_some());
    let (s, _, out) = h.raw("POST", "/admin/unpause", &[ADMIN, ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    let out: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["was_paused"], true);
    assert_eq!(paused(&mut h).await, None);
    // A late bounce from before the unpause doesn't pause it again.
    bounce_all(&h, &sent_ids(&h, &cid2, 3).await, "Permanent").await;
    assert_eq!(paused(&mut h).await, None, "the window restarted at the unpause");
    h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["qa@acme.test"]}))).await;
    h.cleanup().await;
}

#[tokio::test]
async fn campaign_circuit_breaker_stops_a_bad_list_partway() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let list = email_list(&mut h, "bought", 300).await;
    h.drain().await;
    let cid = start_email_campaign(&mut h, &list, "Bought list").await;
    // Prepare, then send only the first batch of 100.
    assert!(jobs::run_one(&h.state).await.unwrap());
    assert!(jobs::run_one(&h.state).await.unwrap());
    let sent_before = h.state.email.sent().len();
    assert_eq!(sent_before, 100);

    // 9 hard bounces (9%) is under the campaign limit; 15 (15%) trips it, though the account
    // as a whole (100 sent) is still below its 200-email minimum.
    let ids = sent_ids(&h, &cid, 100).await;
    bounce_all(&h, &ids[..9], "Permanent").await;
    assert_eq!(paused(&mut h).await, None);
    // Deleting profiles (which deletes their messages) erases neither the sends nor the bounces, including
    // bounces that arrive after the delete (ids 9..15).
    let bounced: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT profile_id FROM messages WHERE provider_message_id = ANY($1)")
        .bind(&ids[..15])
        .fetch_all(&h.state.db)
        .await
        .unwrap();
    for pid in bounced {
        h.ok("DELETE", &format!("/api/profiles/{pid}"), None).await;
    }
    bounce_all(&h, &ids[9..15], "Permanent").await;
    let reason = paused(&mut h).await.expect("the campaign breaker pauses the account");
    assert_eq!(reason, "hard bounce rate 15.0% (15 of 100 emails) on campaign \"Bought list\"");

    // The rest of the campaign never goes out; its recipients are skipped with the reason.
    h.drain().await;
    assert_eq!(h.state.email.sent().iter().filter(|m| m.to != "ops@pl.test").count(), sent_before);
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "failed", "{c}");
    assert!(c["error"].as_str().unwrap().contains("on campaign"), "{c}");
    let skipped: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND status = 'skipped' AND error = 'sending paused'")
        .bind(&cid)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(skipped, 200);

    // MCP get_account reports it.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "mcp"}))).await;
    let bearer = format!("Bearer {}", key["key"].as_str().unwrap());
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_account", "arguments": {}}}).to_string();
    let (s, _, out) = h.raw("POST", "/mcp", &[("authorization", &bearer), ("content-type", "application/json"), ("accept", "application/json, text/event-stream")], &call).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert!(out.contains("email_sending_paused") && out.contains("Bought list"), "{out}");

    // The operator can also lift it by account id.
    let account_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let body = json!({"account_id": account_id}).to_string();
    let (s, _, out) = h.raw("POST", "/admin/unpause", &[ADMIN, ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert!(out.contains(r#""was_paused":true"#), "{out}");
    assert_eq!(paused(&mut h).await, None);
    h.cleanup().await;
}

/// A stand-in for the few Stripe endpoints billing uses. Like Stripe, it replays the first answer
/// for an idempotency key and lists a customer's PaymentIntents with their metadata.
#[derive(Default)]
struct MockStripe {
    sessions: Vec<std::collections::HashMap<String, String>>,
    /// PaymentIntents created: (idempotency key, request form, id, status).
    intents: Vec<MockIntent>,
    /// First answer per idempotency key.
    replies: std::collections::HashMap<String, (u16, Value)>,
    /// POST /payment_intents behavior: "ok" (default); "decline" (402, PaymentIntent left failed);
    /// "error" (500, nothing created); "lost" (charged, but the answer is a 500); "conflict" (409).
    mode: String,
    /// Calls to POST /payment_intents, including ones that created nothing.
    posts: usize,
}

type Mock = std::sync::Arc<std::sync::Mutex<MockStripe>>;
type MockIntent = (Option<String>, std::collections::HashMap<String, String>, String, String);

fn intent_json(form: &std::collections::HashMap<String, String>, id: &str, status: &str) -> Value {
    let metadata: serde_json::Map<String, Value> = form
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("metadata[").and_then(|k| k.strip_suffix(']')).map(|k| (k.to_string(), json!(v))))
        .collect();
    json!({"id": id, "status": status, "amount": form.get("amount").and_then(|a| a.parse::<i64>().ok()), "customer": form.get("customer"),
           "metadata": metadata, "last_payment_error": if status == "succeeded" { Value::Null } else { json!({"message": "Your card was declined."}) }})
}

async fn mock_stripe() -> (Mock, String) {
    use axum::extract::{Form, Path, State};
    use axum::routing::{get, post};
    use std::collections::HashMap;
    let mock: Mock = Default::default();
    let app = axum::Router::new()
        .route("/customers", post(|| async { axum::Json(json!({"id": "cus_mock", "object": "customer"})) }))
        .route("/customers/{id}", get(|Path(id): Path<String>| async move { axum::Json(json!({"id": id, "object": "customer"})) }))
        .route(
            "/checkout/sessions",
            post(|State(m): State<Mock>, Form(f): Form<HashMap<String, String>>| async move {
                let mut m = m.lock().unwrap();
                m.sessions.push(f);
                let id = format!("cs_mock_{}", m.sessions.len());
                axum::Json(json!({"id": id, "url": format!("https://checkout.stripe.test/{id}")}))
            }),
        )
        .route(
            "/payment_intents/{id}",
            // pi_..._bank was paid by bank debit, which can't be charged automatically later.
            get(|Path(id): Path<String>| async move {
                let kind = if id.ends_with("_bank") { "us_bank_account" } else { "card" };
                axum::Json(json!({"id": id, "payment_method": {"id": format!("pm_{kind}"), "type": kind}}))
            }),
        )
        .route("/payment_intents/{id}/cancel", post(|Path(id): Path<String>| async move { axum::Json(json!({"id": id, "status": "canceled"})) }))
        .route(
            "/payment_intents",
            get(|State(m): State<Mock>| async move {
                let m = m.lock().unwrap();
                axum::Json(json!({"object": "list", "data": m.intents.iter().rev().map(|(_, f, id, st)| intent_json(f, id, st)).collect::<Vec<_>>()}))
            })
            .post(|State(m): State<Mock>, headers: axum::http::HeaderMap, Form(f): Form<HashMap<String, String>>| async move {
                let mut m = m.lock().unwrap();
                m.posts += 1;
                let key = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(String::from);
                if let Some((code, body)) = key.as_ref().and_then(|k| m.replies.get(k)).cloned() {
                    return (StatusCode::from_u16(code).unwrap(), axum::Json(body));
                }
                let id = format!("pi_mock_{}", m.intents.len() + 1);
                let (code, body) = match m.mode.as_str() {
                    "conflict" => return (StatusCode::CONFLICT, axum::Json(json!({"error": {"type": "idempotency_error", "message": "request in progress"}}))),
                    "error" => (500, json!({"error": {"type": "api_error", "message": "Stripe is having a bad day"}})),
                    "decline" => {
                        m.intents.push((key.clone(), f.clone(), id.clone(), "requires_payment_method".into()));
                        (402, json!({"error": {"type": "card_error", "code": "card_declined", "message": "Your card was declined.", "payment_intent": intent_json(&f, &id, "requires_payment_method")}}))
                    }
                    "lost" => {
                        m.intents.push((key.clone(), f.clone(), id.clone(), "succeeded".into()));
                        (500, json!({"error": {"type": "api_error", "message": "connection reset"}}))
                    }
                    _ => {
                        m.intents.push((key.clone(), f.clone(), id.clone(), "succeeded".into()));
                        (200, intent_json(&f, &id, "succeeded"))
                    }
                };
                if let Some(k) = key {
                    m.replies.insert(k, (code, body.clone()));
                }
                (StatusCode::from_u16(code).unwrap(), axum::Json(body))
            }),
        )
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (mock, base)
}

fn stripe_sig(body: &str) -> String {
    use hmac::Mac;
    let t = chrono::Utc::now().timestamp();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"whsec_test").unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
}

/// Deliver a paid plan Checkout session the way Stripe's webhook would.
async fn plan_paid(h: &Harness, account: &Value, session: &str, from_tier: i16, tier: i16, cents: i64) {
    let ev = json!({"type": "checkout.session.completed", "data": {"object": {"id": session, "payment_status": "paid", "amount_total": cents,
        "payment_intent": format!("pi_{session}"), "customer": "cus_mock",
        "metadata": {"kind": "plan", "account_id": account, "tier": tier.to_string(), "from_tier": from_tier.to_string()}}}}).to_string();
    let (s, _, body) = h.raw("POST", "/webhooks/stripe", &[("stripe-signature", &stripe_sig(&ev)), ("content-type", "application/json")], &ev).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn monthly_email_plans() {
    let (mock, base) = mock_stripe().await;
    let Some(mut h) = Harness::with(|c| {
        c.stripe_secret_key = Some("sk_test_mock".into());
        c.stripe_api_base = base;
    })
    .await
    else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    let me = h.signup().await;
    let acct = me["account"]["id"].clone();
    let acct_id: uuid::Uuid = acct.as_str().unwrap().parse().unwrap();
    let renew = |h: &Harness| {
        let state = h.state.clone();
        async move { dumbmail::subscription::run_renewals(&state).await.unwrap() }
    };
    let past_due = |h: &Harness| {
        let db = h.state.db.clone();
        async move {
            sqlx::query("UPDATE accounts SET email_plan_period_end = now() - interval '1 minute'").execute(&db).await.unwrap();
        }
    };

    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "none");
    assert_eq!(b["plan_options"].as_array().unwrap().len(), 6);
    assert_eq!(b["plan_options"][5]["price"], 320.0);
    assert_eq!(b["plan_options"][5]["emails"], 960_000);
    assert_eq!(b["min_topup"], 25.0);
    let (s, _) = h.req("POST", "/api/billing/checkout", Some(json!({"amount": 10}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "top-ups start at $25");
    let (s, _) = h.req("POST", "/api/billing/plan", Some(json!({"tier": 7}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Start: Checkout for the full $10, which renews and saves the card.
    let r = h.ok("POST", "/api/billing/plan", Some(json!({"tier": 1}))).await;
    assert!(r["url"].as_str().unwrap().starts_with("https://checkout.stripe.test/"));
    {
        let m = mock.lock().unwrap();
        let sess = &m.sessions[0];
        assert_eq!(sess["line_items[0][price_data][unit_amount]"], "1000");
        assert_eq!((sess["metadata[tier]"].as_str(), sess["metadata[from_tier]"].as_str()), ("1", "0"));
        assert_eq!(sess["payment_intent_data[setup_future_usage]"], "off_session");
        assert!(sess.iter().any(|(k, v)| k.starts_with("excluded_payment_method_types") && v == "us_bank_account"), "renewals need a card that can be charged on the spot");
        assert!(sess["custom_text[submit][message]"].contains("Renews every month at $10"));
    }
    for _ in 0..2 {
        plan_paid(&h, &acct, "cs_start", 0, 1, 1000).await;
    }
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "active", "{}", b["plan"]);
    assert_eq!(b["plan"]["emails_included"], 30_000);
    assert_eq!(b["plan_payments"].as_array().unwrap().len(), 1, "a repeated webhook applies once");
    assert_eq!(b["has_payment_method"], true);
    assert_eq!(b["balance"], 0.0);

    // 30 emails with 25 free: the other 5 come from the plan, so no credit is needed.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await;
    let mut csv = String::from("email\n");
    for i in 0..30 {
        csv.push_str(&format!("p{i}@x.test\n"));
    }
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["emails_used"], 5);
    assert_eq!(b["balance"], 0.0);
    let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE status = 'sent'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sent, 30);

    // Upgrade 1 → 3 mid-month: pay the $30 difference, get the 90,000 extra emails now.
    let r = h.ok("POST", "/api/billing/plan", Some(json!({"tier": 3}))).await;
    assert!(r["url"].is_string());
    assert_eq!(mock.lock().unwrap().sessions[1]["line_items[0][price_data][unit_amount]"], "3000");
    plan_paid(&h, &acct, "cs_up", 1, 3, 3000).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!((b["plan"]["tier"].as_i64(), b["plan"]["emails_included"].as_i64()), (Some(3), Some(120_000)));
    assert_eq!(b["plan"]["emails_used"], 5);

    // Downgrade waits for the renewal; nothing to pay now.
    let r = h.ok("POST", "/api/billing/plan", Some(json!({"tier": 2}))).await;
    assert!(r["url"].is_null());
    assert_eq!(r["billing"]["plan"]["next_tier"], 2);

    // Renewal charges the saved card for the new tier and starts a fresh month.
    past_due(&h).await;
    renew(&h).await;
    renew(&h).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "active", "{}", b["plan"]);
    assert_eq!((b["plan"]["tier"].as_i64(), b["plan"]["emails_included"].as_i64(), b["plan"]["emails_used"].as_i64()), (Some(2), Some(60_000), Some(0)));
    assert!(b["plan"]["next_tier"].is_null());
    {
        let m = mock.lock().unwrap();
        assert_eq!(m.intents.len(), 1, "renewed exactly once");
        assert_eq!(m.intents[0].1["amount"], "2000");
        assert_eq!(m.intents[0].1["off_session"], "true");
        assert!(m.intents[0].0.as_deref().unwrap().starts_with("plan-renewal-"));
    }
    assert_eq!(b["plan_payments"].as_array().unwrap().len(), 3);
    let charges = |m: &Mock| m.lock().unwrap().intents.len();
    // Let pending attempts be picked up again (the scheduler waits two minutes between checks).
    let age = |h: &Harness| {
        let db = h.state.db.clone();
        async move {
            sqlx::query("UPDATE plan_renewals SET claimed_at = now() - interval '5 minutes' WHERE status = 'pending'").execute(&db).await.unwrap();
        }
    };

    // The charge goes through but the answer is lost: nothing is counted, and the next check finds the
    // payment at Stripe instead of charging again.
    mock.lock().unwrap().mode = "lost".into();
    past_due(&h).await;
    renew(&h).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "renewing");
    assert_eq!(b["plan"]["renewal_attempts"], 0);
    renew(&h).await; // too soon to look again
    assert_eq!(charges(&mock), 2);
    mock.lock().unwrap().mode = "ok".into();
    age(&h).await;
    renew(&h).await;
    assert_eq!(charges(&mock), 2, "found the lost payment; no second charge");
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "active", "{}", b["plan"]);
    assert_eq!(b["plan_payments"].as_array().unwrap().len(), 4);

    // Stripe busy (409) or down (500, which it replays for the same key): no attempt is counted.
    for mode in ["conflict", "error"] {
        mock.lock().unwrap().mode = mode.into();
        past_due(&h).await;
        renew(&h).await;
        age(&h).await;
        renew(&h).await;
        let b = h.ok("GET", "/api/billing", None).await;
        assert_eq!((b["plan"]["status"].as_str(), b["plan"]["renewal_attempts"].as_i64()), (Some("renewing"), Some(0)), "{mode}");
        mock.lock().unwrap().mode = "ok".into();
        // An attempt sent more than 10 minutes ago with nothing at Stripe gets a fresh key.
        sqlx::query("UPDATE plan_renewals SET created_at = now() - interval '11 minutes' WHERE status = 'pending'").execute(&h.state.db).await.unwrap();
        age(&h).await;
        renew(&h).await;
        assert_eq!(h.ok("GET", "/api/billing", None).await["plan"]["status"], "active", "{mode}");
    }
    assert_eq!(charges(&mock), 4);

    // Stripe's webhook can settle an attempt before the scheduler does.
    mock.lock().unwrap().mode = "lost".into();
    past_due(&h).await;
    renew(&h).await;
    let (pi, renewal) = {
        let m = mock.lock().unwrap();
        let (_, f, id, _) = m.intents.last().unwrap();
        (id.clone(), f["metadata[renewal_id]"].clone())
    };
    let ev = json!({"type": "payment_intent.succeeded", "data": {"object": {"id": pi, "status": "succeeded",
        "metadata": {"kind": "plan_renewal", "renewal_id": renewal, "account_id": acct}}}}).to_string();
    for _ in 0..2 {
        let (s, _, body) = h.raw("POST", "/webhooks/stripe", &[("stripe-signature", &stripe_sig(&ev)), ("content-type", "application/json")], &ev).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    mock.lock().unwrap().mode = "ok".into();
    age(&h).await;
    renew(&h).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "active");
    assert_eq!(b["plan_payments"].as_array().unwrap().len(), 7, "the webhook and the scheduler record it once");
    assert_eq!(charges(&mock), 5);

    // Declined three times (a day apart): plan emails stop at once, the plan ends after the third.
    mock.lock().unwrap().mode = "decline".into();
    past_due(&h).await;
    renew(&h).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "renewal_failed");
    assert_eq!(b["plan"]["renewal_attempts"], 1);
    assert_eq!(b["plan"]["emails_remaining"], 0);
    assert_eq!(dumbmail::billing::quote(&h.state, acct_id, "email", 1, false, false).await.unwrap(), 200, "while renewal fails, emails cost credit");
    renew(&h).await;
    assert_eq!(h.ok("GET", "/api/billing", None).await["plan"]["renewal_attempts"], 1, "waits a day before retrying");
    for attempt in 2..=3 {
        sqlx::query("UPDATE accounts SET email_plan_renewal_failed_at = now() - interval '25 hours'").execute(&h.state.db).await.unwrap();
        renew(&h).await;
        let b = h.ok("GET", "/api/billing", None).await;
        if attempt < 3 {
            assert_eq!(b["plan"]["renewal_attempts"], attempt);
        } else {
            assert_eq!(b["plan"]["status"], "none");
            assert_eq!(b["plan"]["ended_reason"], "renewal_failed");
        }
    }
    let keys: std::collections::HashSet<_> = mock.lock().unwrap().intents[5..].iter().map(|(k, ..)| k.clone()).collect();
    assert_eq!(keys.len(), 3, "each declined attempt is a new charge attempt");

    // A late upgrade payment for a plan that has ended becomes credit instead of vanishing.
    plan_paid(&h, &acct, "cs_late", 1, 2, 1000).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["balance"], 10.0);
    assert_eq!(b["plan"]["status"], "none");
    assert!(b["plan_payments"][0]["description"].as_str().unwrap().contains("added as credit"));

    // An old attempt that settles after the customer bought a new plan can't touch the new one:
    // one that never charged is closed without charging, and one that did becomes credit.
    mock.lock().unwrap().mode = "ok".into();
    plan_paid(&h, &acct, "cs_new1", 0, 1, 1000).await;
    for (in_flight, mode, outcome) in [("error", "decline", 0.0), ("lost", "ok", 20.0)] {
        mock.lock().unwrap().mode = in_flight.into();
        past_due(&h).await;
        renew(&h).await; // in flight, outcome unknown
        let session = format!("cs_after_{mode}");
        plan_paid(&h, &acct, &session, 0, 2, 2000).await; // the customer pays for a fresh plan meanwhile
        let before = h.ok("GET", "/api/billing", None).await;
        assert_eq!(before["plan"]["status"], "active");
        mock.lock().unwrap().mode = mode.into();
        sqlx::query("UPDATE plan_renewals SET created_at = now() - interval '11 minutes' WHERE status = 'pending'").execute(&h.state.db).await.unwrap();
        age(&h).await;
        renew(&h).await;
        let after = h.ok("GET", "/api/billing", None).await;
        assert_eq!(after["plan"]["status"], "active", "{mode}: {}", after["plan"]);
        assert_eq!(after["plan"]["period_end"], before["plan"]["period_end"], "{mode}");
        assert_eq!(after["plan"]["renewal_attempts"], 0, "{mode}");
        assert_eq!(after["balance"].as_f64().unwrap() - before["balance"].as_f64().unwrap(), outcome, "{mode}");
    }
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM plan_renewals WHERE status = 'pending'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(pending, 0);

    // A pending attempt that never reached Stripe isn't sent after the customer cancels.
    mock.lock().unwrap().mode = "error".into();
    past_due(&h).await;
    renew(&h).await;
    h.ok("POST", "/api/billing/plan/cancel", None).await;
    mock.lock().unwrap().mode = "ok".into();
    let before = charges(&mock);
    age(&h).await;
    renew(&h).await;
    renew(&h).await;
    assert_eq!(charges(&mock), before, "cancelled before anything was charged");
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!((b["plan"]["status"].as_str(), b["plan"]["ended_reason"].as_str()), (Some("none"), Some("cancelled")));
    plan_paid(&h, &acct, "cs_after_cancel", 0, 2, 2000).await;

    // Cancel at the end of the month: no charge, the plan just ends. Keeping it undoes that.
    mock.lock().unwrap().mode = "ok".into();
    assert_eq!(h.ok("POST", "/api/billing/plan/cancel", None).await["plan"]["status"], "cancelling");
    assert_eq!(h.ok("POST", "/api/billing/plan/resume", None).await["plan"]["status"], "active");
    h.ok("POST", "/api/billing/plan/cancel", None).await;
    let charges_before = charges(&mock);
    past_due(&h).await;
    renew(&h).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!(b["plan"]["status"], "none");
    assert_eq!(b["plan"]["ended_reason"], "cancelled");
    assert_eq!(charges(&mock), charges_before);

    // Cancelling while a declined renewal waits for its retry ends the plan without another charge.
    plan_paid(&h, &acct, "cs_again", 0, 1, 1000).await;
    mock.lock().unwrap().mode = "decline".into();
    past_due(&h).await;
    renew(&h).await;
    assert_eq!(h.ok("GET", "/api/billing", None).await["plan"]["status"], "renewal_failed");
    let posts = mock.lock().unwrap().posts;
    h.ok("POST", "/api/billing/plan/cancel", None).await;
    renew(&h).await;
    let b = h.ok("GET", "/api/billing", None).await;
    assert_eq!((b["plan"]["status"].as_str(), b["plan"]["ended_reason"].as_str()), (Some("none"), Some("cancelled")));
    assert_eq!(mock.lock().unwrap().posts, posts);

    // Paid by bank debit: the plan starts, but that account isn't kept for automatic charges.
    plan_paid(&h, &acct, "cs_bank", 0, 1, 1000).await;
    let pm: Option<String> = sqlx::query_scalar("SELECT stripe_payment_method_id FROM accounts WHERE id = $1").bind(acct_id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(pm.as_deref(), Some("pm_card"));
    assert_eq!(h.ok("GET", "/api/billing", None).await["plan"]["status"], "active");
}

// ---------- Sending guardrails: warm-up, imports, double opt-in, unengaged profiles ----------

#[tokio::test]
async fn warmup_limit_holds_extra_email_until_room_opens() {
    let mut h = match Harness::with(|c| c.warmup_start_daily = 5).await {
        Some(h) => h,
        None => return,
    };
    let me = h.signup().await;
    let account_id = me["account"]["id"].as_str().unwrap().to_string();
    h.credit(5.0).await;
    let list = email_list(&mut h, "warm", 8).await;
    let cid = start_email_campaign(&mut h, &list, "First send").await;
    h.drain().await;

    let pool = h.state.db.clone();
    let count = |status: &'static str| {
        let db = pool.clone();
        let cid = cid.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND status = $2 AND NOT is_test")
                .bind(cid)
                .bind(status)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(count("sent").await, 5, "a new account sends 5 in its first 24 hours here");
    assert_eq!(count("queued").await, 3, "the rest wait, they aren't dropped");
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sending");
    let wait: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM min(run_at) - now())::float8 FROM jobs WHERE kind = 'campaign_batch'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(wait > 23.0 * 3600.0, "the batch waits until the oldest send is a day old, not {wait}s");

    let s = h.ok("GET", "/api/account/sending", None).await;
    assert_eq!(s["email_limit"], json!({"limit": 5, "sent_last_24h": 5, "remaining": 0, "fixed": false, "start": 5}));

    // Tests aren't limited.
    let tc = h.ok("POST", "/api/campaigns", Some(json!({"name": "T", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]},
        "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/test", tc["id"].as_str().unwrap()), Some(json!({"to": ["me@acme.test"]}))).await;

    // An operator raises the limit: the held email goes out on the next run.
    let (st, _, body) = h
        .raw("POST", "/admin/sending-limit", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": account_id, "daily_limit": 100}).to_string())
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    h.fast_forward().await;
    assert_eq!(count("sent").await, 8);
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "sent");
    let s = h.ok("GET", "/api/account/sending", None).await;
    assert_eq!(s["email_limit"]["fixed"], true);
    assert_eq!(s["email_limit"]["limit"], 100);

    // Back to the automatic limit. Today's sends never raise it (or it could double all day long)...
    let (st, _, _) = h
        .raw("POST", "/admin/sending-limit", &[ADMIN, ("content-type", "application/json")], &json!({"account_id": account_id, "daily_limit": null}).to_string())
        .await;
    assert_eq!(st, StatusCode::OK);
    let s = h.ok("GET", "/api/account/sending", None).await;
    assert_eq!(s["email_limit"]["limit"], 5);
    assert_eq!(s["email_limit"]["fixed"], false);
    // ...but once those 8 clean sends are a finished day, the limit is twice that.
    sqlx::query("UPDATE email_sends SET sent_at = now() - interval '2 days'").execute(&h.state.db).await.unwrap();
    let s = h.ok("GET", "/api/account/sending", None).await;
    assert_eq!(s["email_limit"], json!({"limit": 16, "sent_last_24h": 0, "remaining": 16, "fixed": false, "start": 5}));
    h.cleanup().await;
}

#[tokio::test]
async fn import_requires_permission_and_skips_risky_addresses() {
    let mut h = harness!();
    h.signup().await;
    let csv = "email\nreal@shop.test\ninfo@shop.test\nno-reply@shop.test\njane@mailinator.com\n";
    let (s, v) = h.req("POST", "/api/profiles/import", Some(json!({"csv": csv, "email_consent": "subscribed"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("agreed to get email"), "{v}");

    let imp = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(imp["created"], 1, "{imp}");
    assert_eq!(imp["skipped"], 3);
    let errors = imp["errors"].to_string();
    assert!(errors.contains("Skipped 2 shared inbox addresses"), "{errors}");
    assert!(errors.contains("Skipped 1 throwaway address "), "{errors}");

    // A shared inbox is left out but the rest of its row (here an SMS subscriber) still imports.
    let imp = h
        .ok("POST", "/api/profiles/import", Some(json!({"csv": "email,phone\nsales@shop.test,+15035550142\n", "sms_consent": "subscribed", "permission_confirmed": true})))
        .await;
    assert_eq!(imp["created"], 1, "{imp}");
    let (email, sms): (Option<String>, String) =
        sqlx::query_as("SELECT email, sms_consent FROM profiles WHERE phone_number = '+15035550142'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((email, sms.as_str()), (None, "subscribed"));

    // A suppression file needs no confirmation and keeps every address, so they're never emailed.
    let imp = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "email_consent": "unsubscribed"}))).await;
    assert_eq!(imp["created"].as_i64().unwrap() + imp["updated"].as_i64().unwrap(), 4, "{imp}");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'info@shop.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "unsubscribed");

    // MCP imports need the same confirmation.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "mcp"}))).await;
    let auth = format!("Bearer {}", key["key"].as_str().unwrap());
    let call = |args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "import_profiles", "arguments": args}}).to_string();
    let (_, _, body) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &call(json!({"csv": "email\nmcp@shop.test\n"}))).await;
    assert!(body.contains("agreed to get email"), "{body}");
    let (_, _, body) = h
        .raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &call(json!({"csv": "email\nmcp@shop.test\n", "permission_confirmed": true})))
        .await;
    assert!(body.contains("created"), "{body}");
    h.cleanup().await;
}

#[tokio::test]
async fn double_opt_in_form_confirms_before_joining() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "double_opt_in": true}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list_id, "config": {"headline": "Join"}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    let subscribe = |email: &str| json!({"form_id": fid, "email": email}).to_string();
    let pool = h.state.db.clone();
    let member = |email: &'static str| {
        let db = pool.clone();
        let list_id = list_id.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM list_members lm JOIN profiles p ON p.id = lm.profile_id WHERE lm.list_id = $1::uuid AND p.email = $2)")
                .bind(list_id)
                .bind(email)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };

    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe("new@x.test")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["confirm"], true, "{v}");
    assert!(v["message"].as_str().unwrap().contains("Check your inbox"));
    assert!(!member("new@x.test").await, "not on the list until they confirm");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'new@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "pending");
    // Waiting to confirm, they get no email from anything else either (here: a campaign to another list).
    let other = h.ok("POST", "/api/lists", Some(json!({"name": "Everyone"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "list_ids": [other["id"]]}))).await;
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": other["id"]}]}}))).await;
    assert_eq!(est["count"], 0, "a pending sign-up isn't reachable");

    let mails: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "new@x.test").collect();
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].subject, "Confirm your subscription to Acme Goods");
    assert!(mails[0].unsubscribe_url.is_none());
    let link = find_link(&mails[0].html, "https://pl.test/c/");
    let path = link.trim_start_matches("https://pl.test");

    // Signing up again while waiting doesn't send another email.
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe("new@x.test")).await;
    assert_eq!(h.state.email.sent().into_iter().filter(|m| m.to == "new@x.test").count(), 1);

    // Opening the link (as a link scanner would) changes nothing; the button confirms.
    let (s, _, page) = h.raw("GET", path, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("Yes, subscribe me") && page.contains("method=\"post\""), "{page}");
    assert!(!member("new@x.test").await);
    let (s, _, page) = h.raw("POST", path, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("You're subscribed"), "{page}");
    assert!(member("new@x.test").await);
    let (consent, events): (String, i64) = sqlx::query_as(
        "SELECT p.email_consent, (SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.profile_id = p.id AND m.name = 'Subscribed to List')
         FROM profiles p WHERE p.email = 'new@x.test'",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    assert_eq!((consent.as_str(), events), ("subscribed", 1));
    let (s, _, page) = h.raw("POST", path, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("You're subscribed"));
    let (s, _, _) = h.raw("GET", "/c/not-a-real-token", &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Someone already subscribed to email joins straight away.
    h.ok("POST", "/api/profiles", Some(json!({"email": "fan@x.test", "email_consent": "subscribed"}))).await;
    let (_, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe("fan@x.test")).await;
    assert!(!body.contains("\"confirm\""), "{body}");
    assert!(member("fan@x.test").await);

    // A confirmation bounce suppresses the address like any other bounce.
    let pid: String = sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE to_address = 'new@x.test'").fetch_one(&h.state.db).await.unwrap();
    ses_event(&h, &pid, "Permanent").await;
    let suppressed: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE email = 'new@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(suppressed.as_deref(), Some("hard_bounce"));
    h.cleanup().await;
}

#[tokio::test]
async fn campaigns_skip_people_who_never_engage() {
    let mut h = harness!();
    h.signup().await;
    let list = email_list(&mut h, "eng", 3).await;
    let account: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    // eng0: 10 emails, nothing else. eng1: 10 emails and an open. eng2: 9 emails.
    for (email, n) in [("eng0@x.test", 10), ("eng1@x.test", 10), ("eng2@x.test", 9)] {
        sqlx::query(
            "INSERT INTO messages (id, account_id, profile_id, channel, to_address, status, sent_at, created_at)
             SELECT gen_random_uuid(), p.account_id, p.id, 'email', p.email, 'delivered', now() - interval '20 days', now() - interval '20 days'
             FROM profiles p, generate_series(1, $2) WHERE p.email = $1",
        )
        .bind(email)
        .bind(n)
        .execute(&h.state.db)
        .await
        .unwrap();
    }
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'eng1@x.test'").fetch_one(&h.state.db).await.unwrap();
    dumbmail::audience::record_event(
        &h.state.db,
        dumbmail::audience::NewEvent { account_id: account, profile_id: pid, metric: "Opened Email", properties: json!({}), value: None, occurred_at: None, unique_id: None, message_id: None },
    )
    .await
    .unwrap();
    let audience = json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list}]}});
    let est = h.ok("POST", "/api/campaigns/estimate", Some(audience.clone())).await;
    assert_eq!(est["count"], 2, "eng0 is skipped");
    let cid = start_email_campaign(&mut h, &list, "Engaged only").await;
    h.drain().await;
    let to: Vec<String> = sqlx::query_scalar("SELECT to_address FROM messages WHERE campaign_id = $1::uuid ORDER BY to_address").bind(&cid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(to, ["eng1@x.test", "eng2@x.test"]);

    let s = h.ok("GET", "/api/account/sending", None).await;
    assert_eq!(s["unengaged_skip"], json!({"enabled": true, "days": 180, "min_emails": 10}));
    h.ok("PATCH", "/api/account", Some(json!({"email_sunset_enabled": false}))).await;
    let est = h.ok("POST", "/api/campaigns/estimate", Some(audience)).await;
    assert_eq!(est["count"], 3, "turned off, everyone is reachable");
    h.cleanup().await;
}

#[tokio::test]
async fn double_opt_in_never_lets_anyone_in_unconfirmed() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "double_opt_in": true}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list_id, "config": {"headline": "Join"}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    let subscribe = |email: &str| json!({"form_id": fid, "email": email}).to_string();

    // No sender email: the confirmation can't go out, so the sign-up is refused, not let through.
    sqlx::query("UPDATE accounts SET from_email = NULL").execute(&h.state.db).await.unwrap();
    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe("nosender@x.test")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    let joined: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members WHERE list_id = $1::uuid").bind(&list_id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(joined, 0);
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'nosender@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_ne!(consent, "subscribed");
    sqlx::query("UPDATE accounts SET from_email = 'hello@acme.test'").execute(&h.state.db).await.unwrap();

    // An address that hard bounced before gets no confirmation email.
    h.ok("POST", "/api/profiles", Some(json!({"email": "bounced@x.test"}))).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE email = 'bounced@x.test'").execute(&h.state.db).await.unwrap();
    let (s, _, _) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe("bounced@x.test")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.state.email.sent().into_iter().filter(|m| m.to == "bounced@x.test").count(), 0);

    // Confirmation emails count toward bounce rates but never toward (or raise) the warm-up limit.
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe("fresh@x.test")).await;
    let (counted, total): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE counts_toward_limit), count(*) FROM email_sends").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((counted, total), (0, 1));

    // One inbox, however it's spelled, gets at most 3 confirmations a day.
    for e in ["Victim+1@gmail.com", "v.ictim+2@gmail.com", "victim@googlemail.com", "victim+4@gmail.com", "victim@gmail.com"] {
        let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe(e)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let to_victim = h.state.email.sent().into_iter().filter(|m| m.to.contains("ictim")).count();
    assert_eq!(to_victim, 3);
    // A made-up variant doesn't hold back the real address's confirmation.
    for e in ["ann+x@shop.test", "ann@shop.test"] {
        h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &subscribe(e)).await;
    }
    assert_eq!(h.state.email.sent().into_iter().filter(|m| m.to == "ann@shop.test").count(), 1);

    // One visitor can trigger at most 5 confirmations an hour.
    for i in 0..7 {
        let (s, _, _) = h
            .raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json"), ("x-real-ip", "203.0.113.9")], &subscribe(&format!("bot{i}@x.test")))
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    let to_bots = h.state.email.sent().into_iter().filter(|m| m.to.starts_with("bot")).count();
    assert_eq!(to_bots, 5);
    h.cleanup().await;
}

#[tokio::test]
async fn email_links_use_the_shared_link_domain() {
    let Some(mut h) = Harness::with(|c| c.link_url = Some("https://links.pl.test/".into())).await else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    h.signup().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter"}))).await;
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "Email\nbob@x.test\n", "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Fall", "channel": "email",
        "audience": {"include": [{"type": "list", "id": list["id"]}], "exclude": []},
        "content": {"subject": "Hi", "blocks": [{"type": "button", "text": "Shop", "href": "https://acme.test/fall"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "bob@x.test").expect("bob got the campaign");

    // Tracking, the open pixel and unsubscribe all point at the link domain, never at PUBLIC_URL.
    assert!(mail.unsubscribe_url.as_deref().unwrap().starts_with("https://links.pl.test/u/"));
    let click = find_link(&mail.html, "https://links.pl.test/t/c/");
    find_link(&mail.html, "https://links.pl.test/t/o/");
    assert!(!mail.html.contains("https://pl.test/t/") && !mail.html.contains("https://pl.test/u/"));
    let (s, hd, _) = h.raw("GET", &click.replace("https://links.pl.test", ""), &[("host", "links.pl.test")], "").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(hd.get("location").unwrap().to_str().unwrap().starts_with("https://acme.test/fall"));

    // The link domain serves only public pages; the app itself stays on PUBLIC_URL.
    let (s, hd, _) = h.raw("GET", "/campaigns", &[("host", "links.pl.test")], "").await;
    assert_eq!(s, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(hd.get("location").unwrap(), "https://pl.test/campaigns");
    let (s, _, _) = h.raw("GET", "/healthz", &[("host", "links.pl.test")], "").await;
    assert_eq!(s, StatusCode::OK);
    // No customer can claim it as their branded domain.
    let (s, _) = h.req("POST", "/api/account/link-domain", Some(json!({"domain": "links.pl.test"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Emails of everyone matching segment rules, sorted (via the preview endpoint).
async fn matching(h: &mut Harness, def: Value) -> Vec<String> {
    let v = h.ok("POST", "/api/segments/preview", Some(json!({"definition": def}))).await;
    let mut out: Vec<String> = v["sample"].as_array().unwrap().iter().map(|p| p["email"].as_str().unwrap().to_string()).collect();
    out.sort();
    assert_eq!(v["count"].as_u64().unwrap() as usize, out.len(), "sample covers every match in these small tests");
    out
}

fn one(c: Value) -> Value {
    json!({"match": "all", "conditions": [c]})
}

#[tokio::test]
async fn smart_segment_rules() {
    use chrono::{Datelike, Duration, Utc};
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let la = |d: i64| (Utc::now() + Duration::days(d)).with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    let order = |email: &str, days_ago: i64, value: f64, n: u32| {
        json!({"metric": "Placed Order", "profile": {"email": email}, "value": value, "unique_id": format!("{email}-{n}"),
               "time": (Utc::now() - Duration::days(days_ago)).to_rfc3339()})
    };
    // Ann: 3 orders, 30 days apart, last 40 days ago ($600). Bob: one order 10 days ago. Cat: one order 200 days ago.
    for (i, (d, v)) in [(100, 100.0), (70, 200.0), (40, 300.0)].into_iter().enumerate() {
        h.ok("POST", "/api/events", Some(order("ann@x.test", d, v, i as u32))).await;
    }
    h.ok("POST", "/api/events", Some(order("bob@x.test", 10, 50.0, 0))).await;
    h.ok("POST", "/api/events", Some(order("cat@x.test", 200, 20.0, 0))).await;
    let today = la(0);
    h.ok("POST", "/api/profiles", Some(json!({"email": "dan@x.test", "properties": {"birthday": format!("1990-{:02}-{:02}", today.month(), today.day()), "country": "CA"}}))).await;
    let far = la(180);
    h.ok("POST", "/api/profiles", Some(json!({"email": "ann@x.test", "properties": {"birthday": format!("1985-{:02}-{:02}", far.month(), far.day())}}))).await;

    let ev = |extra: Value| {
        let mut c = json!({"type": "event_value", "metric": "Placed Order", "aggregate": "sum"});
        c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        one(c)
    };
    assert_eq!(matching(&mut h, ev(json!({"op": "gte", "value": 500}))).await, ["ann@x.test"]);
    assert_eq!(matching(&mut h, ev(json!({"op": "gt", "value": 0, "within_days": 30}))).await, ["bob@x.test"]);
    assert_eq!(matching(&mut h, ev(json!({"aggregate": "average", "op": "between", "value": [150, 250]}))).await, ["ann@x.test"]);
    assert_eq!(matching(&mut h, ev(json!({"op": "eq", "value": 0}))).await, ["dan@x.test"], "no orders sum to 0");

    // Calendar window in the account's time zone: orders 75 to 35 days ago are Ann's last two.
    let window = one(json!({"type": "event", "metric": "Placed Order", "op": "at_least", "count": 2, "since": la(-75).to_string(), "until": la(-35).to_string()}));
    assert_eq!(matching(&mut h, window).await, ["ann@x.test"]);

    let last = |op: &str, value: Value| one(json!({"type": "event_time", "metric": "Placed Order", "which": "last", "op": op, "value": value}));
    assert_eq!(matching(&mut h, last("more_than_days_ago", json!(30))).await, ["ann@x.test", "cat@x.test"]);
    assert_eq!(matching(&mut h, last("after", json!(la(-20).to_string()))).await, ["bob@x.test"]);
    let first = one(json!({"type": "event_time", "metric": "Placed Order", "which": "first", "op": "between", "value": [la(-105).to_string(), la(-95).to_string()]}));
    assert_eq!(matching(&mut h, first).await, ["ann@x.test"]);

    let pred = |measure: &str, op: &str, value: Value| one(json!({"type": "prediction", "measure": measure, "op": op, "value": value}));
    assert_eq!(matching(&mut h, pred("churn_risk", "eq", json!("high"))).await, ["cat@x.test"]);
    assert_eq!(matching(&mut h, pred("churn_risk", "in", json!(["medium", "high"]))).await, ["ann@x.test", "cat@x.test"]);
    assert_eq!(matching(&mut h, pred("churn_risk", "eq", json!("low"))).await, ["bob@x.test"], "one-time buyers use a 90-day gap");
    assert_eq!(matching(&mut h, pred("expected_next_order_in_days", "lt", json!(0))).await, ["ann@x.test"]);
    assert_eq!(matching(&mut h, pred("predicted_clv", "gt", json!(1000))).await, ["ann@x.test"]);
    assert_eq!(matching(&mut h, pred("order_count", "eq", json!(0))).await, ["dan@x.test"]);
    assert_eq!(matching(&mut h, pred("historic_clv", "between", json!([40, 60]))).await, ["bob@x.test"]);

    let prop = |field: &str, op: &str, value: Value| one(json!({"type": "property", "field": field, "op": op, "value": value}));
    assert_eq!(matching(&mut h, prop("birthday", "anniversary_within_days", json!(0))).await, ["dan@x.test"]);
    assert_eq!(matching(&mut h, prop("country", "in", json!(["ca"]))).await, ["dan@x.test"]);
    assert_eq!(matching(&mut h, prop("country", "not_in", json!(["CA"]))).await.len(), 3, "missing values aren't in the list");
    assert_eq!(matching(&mut h, prop("email", "ends_with", json!("@X.TEST"))).await.len(), 4);
    assert_eq!(matching(&mut h, prop("created_at", "between", json!([la(-1).to_string(), la(1).to_string()]))).await.len(), 4);

    // Engagement and reachability.
    let engaged = |engaged: bool| one(json!({"type": "engagement", "channel": "any", "engaged": engaged, "within_days": 30}));
    assert_eq!(matching(&mut h, engaged(true)).await, ["bob@x.test"]);
    assert_eq!(matching(&mut h, engaged(false)).await.len(), 3);
    h.ok("POST", "/api/profiles", Some(json!({"email": "bob@x.test", "email_consent": "unsubscribed"}))).await;
    assert_eq!(matching(&mut h, one(json!({"type": "can_receive", "channel": "email"}))).await, ["ann@x.test", "cat@x.test", "dan@x.test"]);
    assert_eq!(matching(&mut h, one(json!({"type": "can_receive", "channel": "sms"}))).await.len(), 0);
    assert_eq!(matching(&mut h, one(json!({"type": "suppressed"}))).await.len(), 0);

    // Random samples are stable for a seed and cover everyone at 100%.
    let sample = one(json!({"type": "random", "percent": 50, "seed": "s1"}));
    let a = matching(&mut h, sample.clone()).await;
    assert_eq!(a, matching(&mut h, sample).await);
    assert_eq!(matching(&mut h, one(json!({"type": "random", "percent": 100}))).await.len(), 4);

    // Segments inside segments, live; no self-reference; no deleting a segment another one uses.
    let big = h.ok("POST", "/api/segments", Some(json!({"name": "Big spenders", "definition": ev(json!({"op": "gte", "value": 500}))}))).await;
    let big_id = big["id"].as_str().unwrap().to_string();
    assert_eq!(big["member_count"], 1);
    assert_eq!(matching(&mut h, one(json!({"type": "segment", "segment_id": big_id, "op": "not_in"}))).await, ["bob@x.test", "cat@x.test", "dan@x.test"]);
    let others = h.ok("POST", "/api/segments", Some(json!({"name": "Everyone else", "definition": one(json!({"type": "segment", "segment_id": big_id, "op": "not_in"}))}))).await;
    assert_eq!(others["member_count"], 3);
    let (s, v) = h.req("PATCH", &format!("/api/segments/{big_id}"), Some(json!({"definition": one(json!({"type": "segment", "segment_id": others["id"]}))}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a loop through another segment: {v}");
    let (s, v) = h.req("DELETE", &format!("/api/segments/{big_id}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // Bob spends more: the outer segment follows the inner rules without a refresh.
    h.ok("POST", "/api/events", Some(order("bob@x.test", 1, 600.0, 1))).await;
    let others_id = others["id"].as_str().unwrap();
    assert_eq!(h.ok("GET", &format!("/api/segments/{others_id}"), None).await["member_count"], 2);
    // The scheduler refreshes every segment.
    sqlx::query("UPDATE segments SET refreshed_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();
    jobs::scheduler_tick(&h.state, 15).await.unwrap();
    h.drain().await;
    let refreshed: i64 = sqlx::query_scalar("SELECT count(*) FROM segments WHERE refreshed_at > now() - interval '1 minute'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(refreshed, 2);

    // Sign-up forms: source and the browser's time zone.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter"}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Footer", "kind": "embed", "list_id": list["id"]}))).await;
    let form_id = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{form_id}"), Some(json!({"status": "live"}))).await;
    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")],
        &json!({"form_id": form_id, "email": "eve@x.test", "timezone": "Asia/Calcutta"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(matching(&mut h, one(json!({"type": "form", "form_id": form_id, "within_days": 1}))).await, ["eve@x.test"]);
    assert_eq!(matching(&mut h, one(json!({"type": "form"}))).await, ["eve@x.test"]);
    assert_eq!(matching(&mut h, prop("country", "eq", json!("IN"))).await, ["eve@x.test"]);
    assert_eq!(matching(&mut h, prop("timezone", "eq", json!("Asia/Calcutta"))).await, ["eve@x.test"]);

    // Every rule type together runs as valid SQL.
    let all = json!({"match": "any", "conditions": [
        {"type": "message", "action": "clicked", "campaign_id": uuid::Uuid::now_v7()},
        {"type": "message", "action": "received", "flow_id": uuid::Uuid::now_v7(), "op": "has_not", "within_days": 7},
        {"type": "suppressed", "value": false}, {"type": "consent", "channel": "sms", "status": "pending"},
        {"type": "event", "metric": "Opened Email", "op": "zero", "where": [{"field": "campaign_id", "op": "exists"}]},
        {"type": "group", "match": "all", "conditions": [{"type": "list", "list_id": list["id"]}, {"type": "segment", "segment_id": big_id}]}]});
    assert_eq!(matching(&mut h, all).await.len(), 5);
    h.cleanup().await;
}

#[tokio::test]
async fn preference_page_lets_people_choose_lists() {
    let mut h = harness!();
    h.signup().await;
    let news = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "show_on_preferences": true, "description": "Every Friday"}))).await;
    let drops = h.ok("POST", "/api/lists", Some(json!({"name": "New drops"}))).await;
    let hidden = h.ok("POST", "/api/lists", Some(json!({"name": "Internal VIP"}))).await;
    let drops_id = drops["id"].as_str().unwrap().to_string();
    let drops = h.ok("PATCH", &format!("/api/lists/{drops_id}"), Some(json!({"show_on_preferences": true}))).await;
    assert_eq!(drops["show_on_preferences"], true);
    assert_eq!(news["description"], "Every Friday");
    let ids = |v: &Value| v["id"].as_str().unwrap().to_string();
    for l in [&news, &hidden] {
        h.ok("POST", &format!("/api/lists/{}/members", ids(l)), Some(json!({"emails": ["bob@x.test"]}))).await;
    }
    h.ok("POST", "/api/profiles", Some(json!({"email": "bob@x.test", "email_consent": "subscribed"}))).await;

    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Hello", "channel": "email", "audience": {"include": [{"type": "list", "id": ids(&hidden)}], "exclude": []},
        "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", ids(&c)), None).await;
    h.drain().await;
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "bob@x.test").unwrap();
    let unsub = mail.unsubscribe_url.unwrap().replace("https://pl.test", "");

    let (s, _, body) = h.raw("GET", &unsub, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Email preferences") && body.contains("Newsletter") && body.contains("Every Friday") && body.contains("New drops"), "{body}");
    assert!(!body.contains("Internal VIP"), "only preference lists are shown");

    // Keep only New drops: leaves Newsletter, joins New drops, leaves hidden lists and consent alone.
    let form = "content-type";
    let (s, _, body) = h.raw("POST", &unsub, &[(form, "application/x-www-form-urlencoded")], &format!("action=save&list={drops_id}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Saved"), "{body}");
    let count = |v: Value| v["member_count"].as_i64().unwrap();
    assert_eq!(count(h.ok("GET", &format!("/api/lists/{}", ids(&news)), None).await), 0);
    assert_eq!(count(h.ok("GET", &format!("/api/lists/{drops_id}"), None).await), 1);
    assert_eq!(count(h.ok("GET", &format!("/api/lists/{}", ids(&hidden)), None).await), 1);
    let bob = h.ok("GET", "/api/profiles?q=bob", None).await["data"][0].clone();
    assert_eq!(bob["email_consent"], "subscribed");
    let left = one(json!({"type": "event", "metric": "Unsubscribed from List", "op": "at_least", "count": 1, "where": [{"field": "list_id", "op": "eq", "value": ids(&news)}]}));
    assert_eq!(matching(&mut h, left).await, ["bob@x.test"]);

    // Mail apps' one-click unsubscribe still unsubscribes from all email; then the page is the plain one.
    let (s, _, _) = h.raw("POST", &unsub, &[(form, "application/x-www-form-urlencoded")], "List-Unsubscribe=One-Click").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.ok("GET", "/api/profiles?q=bob", None).await["data"][0]["email_consent"], "unsubscribed");
    let (_, _, body) = h.raw("GET", &unsub, &[], "").await;
    assert!(!body.contains("Email preferences"));
    h.cleanup().await;
}

#[tokio::test]
async fn healthz_reports_the_build() {
    let h = harness!();
    let (s, _, body) = h.raw("GET", "/healthz", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert!(v["git_sha"].is_string());
    h.cleanup().await;
}

#[tokio::test]
async fn nested_segments_cannot_blow_up() {
    let mut h = harness!();
    h.signup().await;
    // Each level includes the one below 30 times: 30, then 900 uses. The second level is refused.
    let base = h.ok("POST", "/api/segments", Some(json!({"name": "Base", "definition": one(json!({"type": "consent", "channel": "email", "status": "subscribed"}))}))).await;
    let many = |id: &Value| json!({"match": "any", "conditions": (0..30).map(|_| json!({"type": "segment", "segment_id": id})).collect::<Vec<_>>()});
    let mid = h.ok("POST", "/api/segments", Some(json!({"name": "Mid", "definition": many(&base["id"])}))).await;
    let (s, v) = h.req("POST", "/api/segments", Some(json!({"name": "Top", "definition": many(&mid["id"])}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("too many times"), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn form_config_validation_and_merge() {
    let mut h = harness!();
    h.signup().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Signups"}))).await;
    let create = |config: Value| json!({"name": "Form", "kind": "popup", "list_id": list["id"], "config": config});

    // Every key in docs/FORMS.md, plus the keys forms already stored before.
    let style = json!({"layout": "image_left", "width": 640, "align": "left", "font": "DM Sans", "headline_size": 30, "headline_weight": 800, "body_size": 16,
            "background_color": "#fff", "headline_color": "#111827", "text_color": "#4b5563", "button_color": "#000000", "button_text_color": "#ffffff",
            "button_full_width": false, "input_background": "#ffffff", "input_border_color": "#d1d5db", "input_text_color": "#111827", "corner_radius": 20,
            "button_radius": 40, "input_radius": 4, "padding": 32, "border_width": 1, "border_color": "#e5e7eb", "shadow": "soft", "overlay_color": "#000000",
            "overlay_opacity": 60, "close_on_overlay": false, "close_color": "#9ca3af", "position": "top", "animation": "zoom", "page_background": "#f0fdf4",
            "custom_css": ".dm-headline { letter-spacing: -0.02em; }"});
    let full = json!({
        "headline": "Hi", "body": "Line one\nLine two", "button_text": "Join", "image_url": "https://cdn.test/a.png", "image_alt": "A scarf",
        "no_thanks_text": "No thanks", "success_message": "Done", "success_body": "Welcome aboard", "discount_code": "WELCOME10",
        "legal_text": "See our [privacy policy](https://acme.test/privacy).", "success_button_text": "Shop", "success_button_url": "https://acme.test/shop",
        "redirect_url": "https://acme.test/thanks", "auto_close_seconds": 0, "confirm_message": "Check your inbox", "sms_consent_text": "Texts ok",
        "brand_color": "#1f6f5c", "delay_seconds": 12, "collect": ["email"],
        "fields": [
            {"key": "first_name", "label": "First name"},
            {"key": "email", "required": true},
            {"key": "skin_type", "type": "select", "label": "Skin type", "options": ["Dry", "Oily"]},
            {"key": "interests", "type": "checkboxes", "options": ["Skincare", "Makeup"]}
        ],
        "phone_step": {"enabled": true, "headline": "Texts too?", "body": "Early access", "button_text": "Text me", "skip_text": "No thanks"},
        "style": style,
        "teaser": {"enabled": true, "text": "10% off", "position": "left", "show": "always"},
        "targeting": {"trigger": "scroll", "scroll_percent": 50, "devices": "desktop", "url_contains": ["/shop"], "url_excludes": ["/checkout"], "page_views": 2,
            "visitors": "new", "frequency_days": 14, "hide_for_subscribers": false, "start_at": "2026-01-01T00:00:00-08:00", "end_at": "2099-01-01T00:00:00Z"},
        "variants": [{"id": "b", "headline": "Hey", "image_alt": "Alt B", "no_thanks_text": "Later", "success_body": "B body", "discount_code": "B10"}]
    });
    let form = h.ok("POST", "/api/forms", Some(create(full.clone()))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    assert_eq!(form["config"]["collect"], json!(["first_name", "email"]), "collect follows fields");
    assert_eq!(form["config"]["style"], full["style"]);
    assert!(form["preview_url"].as_str().unwrap().starts_with(&format!("https://pl.test/f/{fid}?preview=")), "{form}");
    // A form saved the old way still validates.
    h.ok("POST", "/api/forms", Some(create(json!({"collect": ["email", "phone_number", "first_name"], "delay_seconds": 5, "sms_consent_text": "x", "confirm_message": "y",
        "targeting": {"trigger": "exit_intent", "devices": "all", "url_contains": [""], "frequency_days": 7, "hide_for_subscribers": true}, "variants": null})))).await;

    for (bad, says) in [
        (json!({"headlin": "x"}), "did you mean \"headline\""),
        (json!({"colour": "x"}), "Allowed keys"),
        (json!({"style": {"button_color": "red"}}), "config.style.button_color must be a hex color"),
        (json!({"style": {"width": 2000}}), "280 to 900"),
        (json!({"style": {"font": "Comic Sans"}}), "config.style.font must be one of"),
        (json!({"style": {"shade": "x"}}), "config.style has no key \"shade\""),
        (json!({"style": {"custom_css": "a".repeat(10_001)}}), "at most 10000"),
        (json!({"brand_color": "#12"}), "hex color"),
        (json!({"image_url": "javascript:alert(1)"}), "http(s) URL"),
        (json!({"headline": "x".repeat(201)}), "at most 200"),
        (json!({"auto_close_seconds": 61}), "0 to 60"),
        (json!({"targeting": {"start_at": "next tuesday"}}), "ISO 8601"),
        (json!({"targeting": {"delay_seconds": 3}}), "top-level"),
        (json!({"teaser": {"position": "middle"}}), "config.teaser.position"),
        (json!({"phone_step": {"enabled": "yes"}}), "true or false"),
        (json!({"fields": [{"key": "email"}, {"key": "skin", "type": "dropdown"}]}), "config.fields[1].type must be one of"),
        (json!({"fields": [{"key": "email"}, {"key": "skin", "type": "select"}]}), "options is required"),
        (json!({"fields": [{"key": "email"}, {"key": "skin", "options": ["a"]}]}), "only applies to select"),
        (json!({"fields": [{"key": "first_name"}]}), "email or phone_number"),
        (json!({"fields": [{"key": "email"}, {"key": "email"}]}), "twice"),
        (json!({"fields": [{"key": "email"}, {"key": "email_consent"}]}), "reserved"),
        (json!({"fields": [{"key": "email", "type": "text"}]}), "custom fields"),
        (json!({"fields": (0..13).map(|i| json!({"key": if i == 0 { "email".to_string() } else { format!("q{i}") }})).collect::<Vec<_>>()}), "at most 12"),
        (json!({"variants": [{"id": "b", "legal_text": "x"}]}), "form variants can change only"),
        (json!({"variants": [{"id": "b", "discount_code": "x".repeat(61)}]}), "config.variants[0].discount_code"),
        (json!({"collect": ["email", "birthday"]}), "config.collect"),
        (json!("headline"), "config must be an object"),
    ] {
        let (s, v) = h.req("POST", "/api/forms", Some(create(bad.clone()))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains(says), "{bad}: {msg}");
    }

    // Updates merge one level deep: sent keys replace, others stay, null resets to the default.
    let path = format!("/api/forms/{fid}");
    let f = h.ok("PATCH", &path, Some(json!({"config": {"style": {"width": 500}, "headline": null, "discount_code": null}}))).await;
    assert_eq!(f["config"]["style"], json!({"width": 500}), "objects are replaced whole");
    assert!(f["config"].get("headline").is_none() && f["config"].get("discount_code").is_none());
    assert_eq!(f["config"]["teaser"], full["teaser"], "keys not sent are kept");
    let f = h.ok("PATCH", &path, Some(json!({"config": {"fields": [{"key": "phone_number", "required": true}, {"key": "last_name"}]}}))).await;
    assert_eq!(f["config"]["collect"], json!(["phone_number"]));
    let f = h.ok("PATCH", &path, Some(json!({"config": {"fields": null, "style": {"align": null, "width": 300}}}))).await;
    assert!(f["config"].get("fields").is_none());
    assert_eq!(f["config"]["collect"], json!(["phone_number"]), "collect keeps the last synced fields");
    assert_eq!(f["config"]["style"], json!({"width": 300}), "nulls inside an object are dropped");
    let (s, v) = h.req("PATCH", &path, Some(json!({"config": {"style": {"width": 100}}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(h.ok("GET", &path, None).await["config"]["style"], json!({"width": 300}), "a rejected update changes nothing");
    h.cleanup().await;
}

async fn live_form(h: &mut Harness, config: Value) -> (String, String, String) {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Signups"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let f = h.ok("POST", "/api/forms", Some(json!({"name": "Form", "kind": "embed", "list_id": list_id, "config": config, "status": "live"}))).await;
    assert_eq!(f["status"], "live");
    let pk: String = sqlx::query_scalar("SELECT public_key FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    (f["id"].as_str().unwrap().to_string(), list_id, pk)
}

#[tokio::test]
async fn discount_code_only_after_sign_up() {
    let mut h = harness!();
    h.signup().await;
    let (fid, _, pk) = live_form(&mut h, json!({"discount_code": "SECRET10", "variants": [{"id": "b", "discount_code": "BSECRET"}]})).await;
    let (_, _, feed) = h.raw("GET", &format!("/client/forms?k={pk}"), &[], "").await;
    assert!(!feed.contains("SECRET"), "the live feed leaks the code: {feed}");
    let (_, _, html) = h.raw("GET", &format!("/f/{fid}"), &[], "").await;
    assert!(!html.contains("SECRET"), "the hosted page leaks the code");
    let f = h.ok("GET", &format!("/api/forms/{fid}"), None).await;
    let preview = f["preview_url"].as_str().unwrap().trim_start_matches("https://pl.test").to_string();
    let (_, _, html) = h.raw("GET", &preview, &[], "").await;
    assert!(html.contains("SECRET10"), "a preview shows the code for checking");
    let sub = |email: &str, variant: &str| json!({"form_id": fid, "email": email, "variant": variant}).to_string();
    for (email, variant, code) in [("da@x.test", "a", "SECRET10"), ("db@x.test", "b", "BSECRET"), ("dz@x.test", "z", "SECRET10")] {
        let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &sub(email, variant)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["discount_code"], code, "version {variant}");
    }
    let (_, _, page) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/x-www-form-urlencoded")], &format!("form_id={fid}&email=dn%40x.test")).await;
    assert!(page.contains("SECRET10"), "the no-JavaScript thank-you page shows the code");
}

#[tokio::test]
async fn form_custom_fields_and_phone_step() {
    let mut h = harness!();
    h.signup().await;
    let (fid, list_id, pk) = live_form(&mut h, json!({"fields": [
        {"key": "email"},
        {"key": "first_name"},
        {"key": "last_name"},
        {"key": "birthday"},
        {"key": "skin_type", "type": "select", "label": "Skin type", "required": true, "options": ["Dry ", "Oily"]},
        {"key": "interests", "type": "checkboxes", "options": ["Skincare", "Makeup", "Hair"]},
        {"key": "age", "type": "number"},
        {"key": "agree", "type": "checkbox", "label": "I agree", "required": true},
        {"key": "note", "type": "text"}
    ]})).await;
    let url = format!("/client/subscribe?k={pk}");
    let post = |h: &Harness, body: Value| {
        let url = url.clone();
        let router = h.router.clone();
        async move {
            let req = Request::builder().method("POST").uri(&url).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
            let res = router.oneshot(req).await.unwrap();
            let s = res.status();
            let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null);
            (s, v)
        }
    };
    let base = |extra: Value| {
        let mut b = json!({"form_id": fid, "email": "skin@x.test", "fields": {"skin_type": "Dry", "agree": true}});
        for (k, v) in extra.as_object().unwrap() {
            if k == "fields" {
                for (fk, fv) in v.as_object().unwrap() {
                    b["fields"][fk] = fv.clone();
                }
            } else {
                b[k] = v.clone();
            }
        }
        b
    };
    for (body, says) in [
        (json!({"form_id": fid, "fields": {"skin_type": "Dry", "agree": true}}), "enter your email"),
        (base(json!({"fields": {"skin_type": null}})), "choose an answer for \"Skin type\""),
        (base(json!({"fields": {"skin_type": "Normal"}})), "Skin type must be one of: Dry, Oily"),
        (base(json!({"fields": {"agree": false}})), "tick \"I agree\""),
        (base(json!({"fields": {"interests": ["Skincare", "Nails"]}})), "\"Nails\" isn't a choice"),
        (base(json!({"fields": {"birthday": "21/04/1990"}})), "Birthday must be a date"),
        (base(json!({"fields": {"age": "old"}})), "Age must be a number"),
        (base(json!({"fields": {"note": "x".repeat(501)}})), "too long"),
        (base(json!({"email": "not-an-email"})), "valid email"),
    ] {
        let (s, v) = post(&h, body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(says), "{body}: {v}");
    }
    let (s, v) = post(&h, base(json!({"first_name": "Sky", "last_name": "Lee", "fields": {"birthday": "1990-04-21", "interests": ["Makeup", "Skincare", "Makeup"], "age": "31", "note": " hi ", "hacker": "ignored", "email_consent": "x"}}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = h.ok("GET", "/api/profiles?q=skin", None).await["data"][0].clone();
    assert_eq!((p["first_name"].as_str(), p["last_name"].as_str()), (Some("Sky"), Some("Lee")));
    let props = &p["properties"];
    assert_eq!(props["birthday"], "1990-04-21");
    assert_eq!(props["skin_type"], "Dry");
    assert_eq!(props["interests"], json!(["Skincare", "Makeup"]), "saved once each, in the form's order");
    assert_eq!(props["age"], 31);
    assert_eq!(props["agree"], true);
    assert_eq!(props["note"], "hi");
    assert!(props.get("hacker").is_none() && props.get("email_consent").is_none(), "{props}");
    // Sign-ups only fill blanks.
    let (s, _) = post(&h, base(json!({"first_name": "Mallory", "fields": {"skin_type": "Oily", "note": "changed"}}))).await;
    assert_eq!(s, StatusCode::OK);
    let p = h.ok("GET", "/api/profiles?q=skin", None).await["data"][0].clone();
    assert_eq!(p["first_name"], "Sky");
    assert_eq!(p["properties"]["skin_type"], "Dry");
    assert_eq!(p["properties"]["note"], "hi");
    // The no-JavaScript form posts the same answers; repeated checkboxes become a list.
    let (s, _, body) = h.raw("POST", &url, &[("content-type", "application/x-www-form-urlencoded")],
        &format!("form_id={fid}&email=plain%40x.test&skin_type=Oily&agree=1&interests=Hair&interests=Skincare&birthday=2001-02-03")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!body.contains("Something's off"), "{body}");
    let p = h.ok("GET", "/api/profiles?q=plain", None).await["data"][0].clone();
    assert_eq!(p["properties"]["interests"], json!(["Skincare", "Hair"]));
    assert_eq!(p["properties"]["agree"], true);
    assert_eq!(p["properties"]["birthday"], "2001-02-03");

    // Phone step: only when the form turns it on.
    let phone = |email: &str, consent: bool| json!({"form_id": fid, "step": "phone", "email": email, "phone_number": "(503) 555-0142", "sms_consent": consent});
    let (s, v) = post(&h, phone("skin@x.test", true)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("phone step"));
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"phone_step": {"enabled": true}}}))).await;
    let events = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Subscribed to List'").fetch_one(&db).await.unwrap() }
    };
    let (s, v) = post(&h, phone("skin@x.test", false)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("agree to texts"));
    let (s, v) = post(&h, json!({"form_id": fid, "step": "phone", "phone_number": "12", "sms_consent": true})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = post(&h, json!({"form_id": fid, "step": "other"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // An email alone can't attach a number: someone could type a stranger's address. It stands alone.
    let (s, v) = post(&h, phone("skin@x.test", true)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = h.ok("GET", "/api/profiles?q=skin", None).await["data"][0].clone();
    assert!(p["phone_number"].is_null(), "no token: the email's profile is untouched");
    let alone: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE phone_number = '+15035550142' AND email IS NULL AND sms_consent = 'subscribed'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(alone, 1);
    // The first screen's token ties the number to the person who just signed up.
    let (s, first) = post(&h, json!({"form_id": fid, "email": "tok@x.test", "fields": {"skin_type": "Dry", "agree": true}})).await;
    assert_eq!(s, StatusCode::OK, "{first}");
    let token = first["phone_token"].as_str().expect("phone_token when the phone step is on").to_string();
    // Someone typing an existing person's email gets no token for that person's profile.
    let (s, again) = post(&h, json!({"form_id": fid, "email": "skin@x.test", "fields": {"skin_type": "Dry", "agree": true}})).await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert!(again.get("phone_token").is_none(), "no token for a profile this sign-up didn't create: {again}");
    let before = h.ok("GET", &format!("/api/forms/{fid}"), None).await["stats"].clone();
    let events_before = events(&h).await;
    let members = h.ok("GET", &format!("/api/lists/{list_id}"), None).await["member_count"].clone();
    for bad in ["nope".to_string(), format!("{token}x"), token.replace("pstep:", "pstap:")] {
        let (s, _) = post(&h, json!({"form_id": fid, "step": "phone", "phone_token": bad, "phone_number": "+15035550150", "sms_consent": true})).await;
        assert_eq!(s, StatusCode::OK);
    }
    let p = h.ok("GET", "/api/profiles?q=tok", None).await["data"][0].clone();
    assert!(p["phone_number"].is_null(), "bad tokens attach nothing");
    let (s, v) = post(&h, json!({"form_id": fid, "step": "phone", "phone_token": token, "phone_number": "(503) 555-0144", "sms_consent": true})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = h.ok("GET", "/api/profiles?q=tok", None).await["data"][0].clone();
    assert_eq!(p["phone_number"], "+15035550144", "linked to the person who signed up");
    assert_eq!(p["sms_consent"], "subscribed");
    assert_eq!(h.ok("GET", &format!("/api/forms/{fid}"), None).await["stats"], before, "the phone step isn't a second sign-up");
    assert_eq!(events(&h).await, events_before);
    assert_eq!(h.ok("GET", &format!("/api/lists/{list_id}"), None).await["member_count"], members);
    // The same token with a different number for someone who now has one stands alone instead.
    let (s, _) = post(&h, json!({"form_id": fid, "step": "phone", "phone_token": token, "phone_number": "+15035550143", "sms_consent": true})).await;
    assert_eq!(s, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE phone_number = '+15035550143' AND email IS NULL AND sms_consent = 'subscribed'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn form_schedule_preview_and_hosted_page() {
    let mut h = harness!();
    h.signup().await;
    let soon = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    let (fid, _, pk) = live_form(&mut h, json!({"headline": "Big </script><script>alert(1)</script> sale", "targeting": {"start_at": soon}})).await;
    let in_feed = |h: &Harness| {
        let (router, pk, fid) = (h.router.clone(), pk.clone(), fid.clone());
        async move {
            let res = router.oneshot(Request::builder().uri(format!("/client/forms?k={pk}")).body(Body::empty()).unwrap()).await.unwrap();
            let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
            v["data"].as_array().unwrap().iter().any(|f| f["id"] == fid.as_str())
        }
    };
    assert!(!in_feed(&h).await, "not open yet");
    let (s, _, html) = h.raw("GET", &format!("/f/{fid}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("isn't open right now"), "{html}");
    // Sign-ups follow the schedule too: refused before the start, and after the end once two hours' grace pass.
    let sign = |h: &Harness, email: &'static str| {
        let (router, pk, fid) = (h.router.clone(), pk.clone(), fid.clone());
        async move {
            let req = Request::builder().method("POST").uri(format!("/client/subscribe?k={pk}")).header("content-type", "application/json")
                .body(Body::from(json!({"form_id": fid, "email": email}).to_string())).unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(sign(&h, "early@x.test").await, StatusCode::CONFLICT);
    let ended = |hours: i64| json!({"config": {"targeting": {"end_at": (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()}}});
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(ended(1))).await;
    assert_eq!(sign(&h, "grace@x.test").await, StatusCode::OK, "within the grace after the end");
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(ended(3))).await;
    assert_eq!(sign(&h, "late@x.test").await, StatusCode::CONFLICT);
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"targeting": {"start_at": soon}}}))).await;
    let f = h.ok("GET", &format!("/api/forms/{fid}"), None).await;
    let preview = f["preview_url"].as_str().unwrap().trim_start_matches("https://pl.test").to_string();
    let (s, _, html) = h.raw("GET", &preview, &[], "").await;
    assert_eq!(s, StatusCode::OK, "a preview ignores the schedule");
    assert!(html.contains(r#"<div id="dm-form"></div>"#) && html.contains("\"submit\":null"), "{html}");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"targeting": {"start_at": past}}}))).await;
    assert!(in_feed(&h).await);
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"targeting": {"end_at": past}}}))).await;
    assert!(!in_feed(&h).await, "closed");
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"targeting": null}}))).await;

    // The hosted page mounts the shared renderer, keeps a no-JavaScript copy and counts the view.
    let (s, _, html) = h.raw("GET", &format!("/f/{fid}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    for needle in [
        r#"<div id="dm-form"></div>"#,
        "<noscript>",
        r#"<script src="https://pl.test/client/form-render.js"></script>"#,
        "DumbMailFormRenderer",
        "R.mount(d.form, opts)",
        &format!("\"submit\":\"https://pl.test/client/subscribe?k={pk}\""),
        &format!(r#"action="https://pl.test/client/subscribe?k={pk}""#),
        r#"<input type="email" name="email""#,
        "<title>Acme Goods</title>",
        "\"kind\":\"page\"",
    ] {
        assert!(html.contains(needle), "{needle} missing: {html}");
    }
    assert!(!html.contains("</script><script>alert(1)"), "form text can't break out of the data script");
    assert!(html.contains(r"</script>"));
    assert_eq!(h.ok("GET", &format!("/api/forms/{fid}"), None).await["stats"]["views"], 1);

    // Drafts: only the preview link shows them, and it counts nothing.
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "draft"}))).await;
    let (s, _, _) = h.raw("GET", &format!("/f/{fid}"), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h.raw("GET", &format!("/f/{fid}?preview=fpreview:{}.AAAAAAAAAAAAAAAA", fid.replace('-', "")), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a made-up token is no token");
    let other = h.ok("POST", "/api/forms", Some(json!({"name": "Other"}))).await;
    let other_preview = other["preview_url"].as_str().unwrap();
    let token = other_preview.split("preview=").nth(1).unwrap();
    let (s, _, _) = h.raw("GET", &format!("/f/{fid}?preview={token}"), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "another form's token doesn't work");
    let (s, _, html) = h.raw("GET", &preview, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("\"submit\":null") && html.contains("aren't saved") && html.contains("noindex"), "{html}");
    assert!(!html.contains("action="), "the preview's plain form doesn't post anywhere");
    assert_eq!(h.ok("GET", &format!("/api/forms/{fid}"), None).await["stats"]["views"], 1, "previews aren't views");

    // The renderer, on its own and inside the website script.
    let renderer = include_str!("../src/form_render.js").trim();
    let renderer = renderer.strip_suffix(';').unwrap_or(renderer).trim_end();
    let (s, hd, js) = h.raw("GET", "/client/form-render.js", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(hd["content-type"].to_str().unwrap().starts_with("application/javascript"));
    assert_eq!(hd["access-control-allow-origin"], "*");
    assert_eq!(hd["cache-control"], "public, max-age=300");
    assert_eq!(js, format!("window.DumbMailFormRenderer = {renderer};\n"));
    for k in [pk.as_str(), "__RENDERER__"] {
        let (_, _, js) = h.raw("GET", &format!("/onsite.js?k={k}"), &[], "").await;
        assert!(js.contains(&format!("KEY = \"{k}\"")), "{k}");
        if include_str!("../src/onsite.js").contains("__RENDERER__") {
            assert!(js.contains(&format!("var FORMS_UI = {renderer};")), "renderer inlined");
            assert_eq!(js.matches("__RENDERER__").count(), usize::from(k == "__RENDERER__"), "only the JSON-quoted key can say __RENDERER__");
        }
    }
    h.cleanup().await;
}

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
    0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn images_upload_serve_and_limits() {
    let mut h = harness!();
    let me = h.signup().await;
    let owner = h.cookie.clone();
    let (s, _, body) = h.bytes("POST", "/api/images", "image/jpeg", PNG.to_vec()).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let img: Value = serde_json::from_slice(&body).unwrap();
    let id = img["id"].as_str().unwrap().to_string();
    assert_eq!(img["content_type"], "image/png", "the claimed type is ignored");
    assert_eq!(img["size"], PNG.len());
    assert_eq!(img["url"], format!("https://pl.test/i/{id}"));
    let (_, _, again) = h.bytes("POST", "/api/images", "image/png", PNG.to_vec()).await;
    assert_eq!(serde_json::from_slice::<Value>(&again).unwrap()["id"], id.as_str(), "the same file is stored once");

    let (s, _, body) = h.bytes("POST", "/api/images", "image/png", b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>".to_vec()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("isn't a PNG, JPEG, GIF or WebP"));
    let (s, _, _) = h.bytes("POST", "/api/images", "text/plain", PNG.to_vec()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, body) = h.bytes("POST", "/api/images", "image/png", [PNG, &vec![0u8; 5 * 1024 * 1024]].concat()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("at most 5 MB"));
    let (s, v) = h.req("POST", "/api/images", Some(json!({"url": "ftp://x.test/a.png"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = h.req("POST", "/api/images", Some(json!({"data_base64": "!!!"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let gif = h.ok("POST", "/api/images", Some(json!({"data_base64": "data:image/gif;base64,R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw=="}))).await;
    assert_eq!(gif["content_type"], "image/gif");
    let listed = h.ok("GET", "/api/images", None).await;
    assert_eq!(listed["total"], 2);
    assert_eq!(listed["data"][0]["id"], gif["id"], "newest first");

    // Served publicly, as an image only.
    let (s, hd, bytes) = h.bytes("GET", &format!("/i/{id}"), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(bytes, PNG);
    assert_eq!(hd["content-type"], "image/png");
    assert_eq!(hd["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(hd["x-content-type-options"], "nosniff");
    assert_eq!(hd["content-security-policy"], "default-src 'none'");
    let (s, _, _) = h.raw("GET", "/i/nope", &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h.raw("GET", &format!("/i/{}", uuid::Uuid::new_v4()), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // 500 per account; an image it already has still comes back.
    let account: uuid::Uuid = me["account"]["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("INSERT INTO images (id, account_id, content_type, size, sha256, bytes) SELECT gen_random_uuid(), $1, 'image/png', 1, md5(i::text), '\\x00'::bytea FROM generate_series(1, 498) i")
        .bind(account)
        .execute(&h.state.db)
        .await
        .unwrap();
    let (s, _, body) = h.bytes("POST", "/api/images", "image/png", [PNG, b"x"].concat()).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&body).contains("500 images"), "{}", String::from_utf8_lossy(&body));
    let (s, _, _) = h.bytes("POST", "/api/images", "image/png", PNG.to_vec()).await;
    assert_eq!(s, StatusCode::OK);

    // Another account can't see or delete them, and keeps its own copy of the same file.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other Co"}))).await;
    assert_eq!(h.ok("GET", "/api/images", None).await["total"], 0);
    let (s, _) = h.req("DELETE", &format!("/api/images/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, _, mine) = h.bytes("POST", "/api/images", "image/png", PNG.to_vec()).await;
    assert_ne!(serde_json::from_slice::<Value>(&mine).unwrap()["id"], id.as_str());
    h.cookie = owner;
    h.ok("DELETE", &format!("/api/images/{id}"), None).await;
    let (s, _, _) = h.raw("GET", &format!("/i/{id}"), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn forms_and_images_over_mcp() {
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Claude"}))).await["key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIP"}))).await;
    let router = h.router.clone();
    let mut n = 0;
    let mut rpc = move |method: &str, params: Value| {
        n += 1;
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"jsonrpc": "2.0", "id": n, "method": method, "params": params}).to_string()))
            .unwrap();
        let router = router.clone();
        async move { serde_json::from_slice::<Value>(&router.oneshot(req).await.unwrap().into_body().collect().await.unwrap().to_bytes()).unwrap() }
    };
    let tools = rpc("tools/list", json!({})).await;
    let tools = tools["result"]["tools"].as_array().unwrap().clone();
    for name in ["get_form", "upload_image", "list_images", "create_form", "update_form"] {
        assert!(tools.iter().any(|t| t["name"] == name), "{name}");
    }
    let create = tools.iter().find(|t| t["name"] == "create_form").unwrap();
    assert_eq!(create["inputSchema"]["properties"]["config"]["properties"]["style"]["properties"]["shadow"]["enum"][0], "none");
    let res = rpc("resources/read", json!({"uri": "dumbmail://docs/forms"})).await;
    assert!(res["result"]["contents"][0]["text"].as_str().unwrap().contains("## Phone step"));

    let up = rpc("tools/call", json!({"name": "upload_image", "arguments": {"data_base64": "R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw=="}})).await;
    let img = up["result"]["structuredContent"].clone();
    assert_eq!(img["content_type"], "image/gif", "{up}");
    let made = rpc("tools/call", json!({"name": "create_form", "arguments": {"list_id": list["id"], "kind": "page", "config": {"image_url": img["url"], "style": {"layout": "background"}}}})).await;
    let made = &made["result"]["structuredContent"];
    assert_eq!(made["status"], "draft", "{made}");
    let fid = made["form"]["id"].as_str().unwrap().to_string();
    let bad = rpc("tools/call", json!({"name": "update_form", "arguments": {"id": fid, "config": {"style": {"layout": "sideways"}}}})).await;
    assert_eq!(bad["result"]["isError"], true);
    assert!(bad["result"]["content"][0]["text"].as_str().unwrap().contains("config.style.layout must be one of"), "{bad}");
    let up = rpc("tools/call", json!({"name": "update_form", "arguments": {"id": fid, "status": "live", "config": {"variants": [{"id": "b", "headline": "B"}]}}})).await;
    assert_eq!(up["result"]["structuredContent"]["form"]["status"], "live", "{up}");
    let got = rpc("tools/call", json!({"name": "get_form", "arguments": {"id": fid}})).await;
    let got = &got["result"]["structuredContent"];
    assert_eq!(got["form"]["config"]["image_url"], img["url"]);
    assert_eq!(got["form"]["variant_stats"].as_array().unwrap().len(), 2);
    assert!(got["preview_url"].as_str().unwrap().contains("?preview="), "{got}");
    assert_eq!(got["hosted_url"], format!("https://pl.test/f/{fid}"));
    let listed = rpc("tools/call", json!({"name": "list_images", "arguments": {}})).await;
    assert_eq!(listed["result"]["structuredContent"]["total"], 1);
    let del = rpc("tools/call", json!({"name": "delete", "arguments": {"type": "image", "id": img["id"]}})).await;
    assert_eq!(del["result"]["structuredContent"]["deleted"], true, "{del}");
    let listed = rpc("tools/call", json!({"name": "list_images", "arguments": {}})).await;
    assert_eq!(listed["result"]["structuredContent"]["total"], 0);
    h.cleanup().await;
}

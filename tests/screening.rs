//! Sender screening: standing, the first check, domains, imports, content, engagement and the
//! operator's decisions. The Claude call goes to a loopback stand-in; websites, domain ages and
//! mail servers are canned (`screening::fetch::testing`).
#![allow(unused_macros)]

#[macro_use]
mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use common::*;
use dumbmail::features::screening::fetch::testing as net;
use serde_json::{json, Value};
use uuid::Uuid;

const ADMIN: &str = "Bearer test-admin-token-0123456789abcdef0123";

/// Requests the stand-in got (bodies).
type Calls = Arc<Mutex<Vec<Value>>>;

/// A stand-in for `POST /v1/messages`: a pass, unless the material contains VERDICT_REVIEW,
/// VERDICT_BLOCK or REFUSE_THIS.
async fn stand_in() -> (Calls, String) {
    let calls: Calls = Default::default();
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(|axum::extract::State(c): axum::extract::State<Calls>, axum::Json(body): axum::Json<Value>| async move {
                c.lock().unwrap().push(body.clone());
                let text = body["messages"][0]["content"].as_str().unwrap_or("").to_string();
                if text.contains("REFUSE_THIS") {
                    return axum::Json(json!({"model": "claude-opus-5-5", "stop_reason": "refusal", "content": [], "usage": {"input_tokens": 5, "output_tokens": 0}}));
                }
                let verdict = if text.contains("VERDICT_BLOCK") { "block" } else if text.contains("VERDICT_REVIEW") { "review" } else { "pass" };
                let reasons: Vec<&str> = if verdict == "pass" { vec![] } else { vec!["The model had concerns."] };
                let answer = json!({"verdict": verdict, "reasons": reasons, "operator_notes": "stand-in"}).to_string();
                axum::Json(json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "stop_reason": "end_turn",
                    "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": answer}], "usage": {"input_tokens": 120, "output_tokens": 30}}))
            }),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (calls, format!("http://{addr}"))
}

fn canned_web() {
    net::offline();
    net::page("acme.test", Some(("Acme Goods - handmade soap from Portland", "We make soap by hand in Portland from local ingredients. Shop bars, gift sets and refills, shipped free over $40. Join our newsletter for new scents.")));
}

/// A screening harness: screening on, the stand-in as the API (or no key).
async fn screened(key: bool) -> Option<(Harness, Calls)> {
    canned_web();
    let (calls, url) = stand_in().await;
    let h = Harness::with(|c| {
        c.screen_senders = true;
        c.anthropic_api_key = key.then(|| "sk-ant-test-0123456789".into());
        c.anthropic_api_base = Some(url.clone());
    })
    .await?;
    Some((h, calls))
}

macro_rules! screened {
    ($key:expr) => {
        match screened($key).await {
            Some(x) => x,
            None => return,
        }
    };
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn standing(h: &Harness) -> String {
    sqlx::query_scalar("SELECT sending_review FROM accounts WHERE id = $1").bind(account_id(h).await).fetch_one(&h.state.db).await.unwrap()
}

async fn verify_domain(h: &Harness, domain: &str) {
    sqlx::query("INSERT INTO sending_domains (id, account_id, domain, status, verified_at) VALUES ($1, $2, $3, 'verified', now())")
        .bind(Uuid::now_v7())
        .bind(account_id(h).await)
        .bind(domain)
        .execute(&h.state.db)
        .await
        .unwrap();
}

async fn answers(h: &mut Harness, business: &str) -> Value {
    h.ok("POST", "/api/screening/request", Some(json!({"business": business, "list_source": "People sign up on our website's newsletter form and at checkout.", "monthly_volume": 2000}))).await
}

async fn approve_directly(h: &Harness) {
    sqlx::query("UPDATE accounts SET sending_review = 'approved' WHERE id = $1").bind(account_id(h).await).execute(&h.state.db).await.unwrap();
}

/// A list of subscribed people and an email campaign to it. Returns the campaign id.
async fn campaign(h: &mut Harness, subject: &str, html: &str, people: &[&str]) -> String {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": format!("L {}", Uuid::now_v7().simple())}))).await;
    for p in people {
        h.ok("POST", "/api/profiles", Some(json!({"email": p, "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    }
    let c = h
        .ok("POST", "/api/campaigns", Some(json!({"name": format!("C {subject}"), "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
            "content": {"subject": subject, "blocks": [{"type": "text", "html": html}]}})))
        .await;
    c["id"].as_str().unwrap().to_string()
}

fn soap() -> &'static str {
    "<p>Our fall scents are here: cedar, pear and smoke. <a href=\"https://acme.test/fall\">Shop the collection</a></p>"
}

#[tokio::test]
async fn a_new_account_sends_only_after_its_check_passes() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    assert_eq!(standing(&h).await, "pending");
    let cid = campaign(&mut h, "Fall scents", soap(), &["ann@x.test"]).await;
    // First the three questions.
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("tell us about your business"), "{v}");
    // Then a verified sending domain: the check waits for one.
    let r = answers(&mut h, "We make handmade soap in Portland and sell it online.").await;
    assert_eq!(r["status"], "pending");
    assert_eq!(r["waiting_for"], "sending_domain", "{r}");
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v.to_string().contains("verify a sending domain"), "{v}");
    assert!(calls.lock().unwrap().is_empty(), "no model call before the check can run");
    verify_domain(&h, "acme.test").await;
    let sent = h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(sent["status"], "sending");
    assert_eq!(standing(&h).await, "approved");
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"), "the campaign went out");
    assert!(h.state.email.sent().iter().any(|m| m.to == "owner@acme.test" && m.subject.contains("approved")), "the owner was told");
    // One account check and one content check, both with the expected request shape.
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for c in &calls {
        assert_eq!(c["model"], "claude-opus-5-5");
        assert_eq!(c["output_config"]["format"]["type"], "json_schema");
        assert_eq!(c["output_config"]["effort"], "medium");
        assert_eq!(c["fallbacks"], "default");
        assert!(c.get("temperature").is_none());
        assert_eq!(c["messages"].as_array().unwrap().len(), 1, "no prefill");
        let user = c["messages"][0]["content"].as_str().unwrap();
        assert!(user.starts_with("<customer_material>"));
        assert!(!user.contains("owner@acme.test"), "the owner's address only as its domain");
        assert!(!user.contains("ann@x.test"), "no contact's address");
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM screenings WHERE source = 'ai' AND model = 'claude-opus-5-5' AND input_tokens = 120").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(rows, 2);
    // The customer sees the outcome, never the operator's signals.
    let r = h.ok("GET", "/api/screening", None).await;
    assert_eq!(r["status"], "approved");
    assert!(!r.to_string().contains("signals") && !r.to_string().contains("stand-in"), "{r}");
    h.cleanup().await;
}

#[tokio::test]
async fn with_no_api_key_a_person_decides() {
    let (mut h, _) = screened!(false);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    let r = answers(&mut h, "We make handmade soap in Portland and sell it online.").await;
    assert_eq!(r["status"], "review", "{r}");
    assert!(h.state.email.sent().iter().any(|m| m.to == "ops@pl.test" && m.text.contains("no ANTHROPIC_API_KEY")), "the operator alert says why");
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v.to_string().contains("a person is checking it"), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_model_pass_never_overrides_the_rules() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    // No website: the rules say review, the model says pass.
    h.ok("PATCH", "/api/account", Some(json!({"website": ""}))).await;
    verify_domain(&h, "acme.test").await;
    let r = answers(&mut h, "We make handmade soap.").await;
    assert_eq!(r["status"], "review", "{r}");
    assert_eq!(calls.lock().unwrap().len(), 1, "the model was asked");
    h.cleanup().await;

    // A brand lookalike among the verified domains: blocked by the rules alone.
    let (mut h, calls) = screened!(true);
    h.signup().await;
    verify_domain(&h, "paypal-secure-login.com").await;
    let r = answers(&mut h, "We make handmade soap.").await;
    assert_eq!(r["status"], "blocked", "{r}");
    assert!(calls.lock().unwrap().is_empty(), "rules that block don't ask the model");
    assert!(h.state.email.sent().iter().any(|m| m.to == "owner@acme.test" && m.subject.contains("can't send")));
    h.cleanup().await;
}

#[tokio::test]
async fn a_throwaway_owner_address_blocks() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    sqlx::query("UPDATE users SET email = 'owner@mailinator.com'").execute(&h.state.db).await.unwrap();
    verify_domain(&h, "acme.test").await;
    let r = answers(&mut h, "We make handmade soap.").await;
    assert_eq!(r["status"], "blocked", "{r}");
    let reasons: Value = sqlx::query_scalar("SELECT reasons FROM screenings WHERE kind = 'account'").fetch_one(&h.state.db).await.unwrap();
    assert!(reasons.to_string().contains("throwaway"), "{reasons}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_brand_lookalike_domain_is_refused_before_the_provider() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    let (s, v) = h.req("POST", "/api/account/domains", Some(json!({"domain": "paypa1-verify.com"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("impersonates paypal"), "{v}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sending_domains").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(rows, 0, "nothing was set up");
    assert_eq!(standing(&h).await, "blocked", "a pending account is blocked");
    // The same for a link domain on an approved account: refused, and the account goes to review.
    let (mut h2, _) = screened!(true);
    h2.signup().await;
    approve_directly(&h2).await;
    let (s, _) = h2.req("POST", "/api/account/link-domain", Some(json!({"domain": "links.netflix-billing.co"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(standing(&h2).await, "review");
    // A brand-new domain is only noted.
    net::registered("brandnew-soap.test", chrono::Utc::now() - chrono::Duration::days(3));
    let (mut h3, _) = screened!(true);
    h3.signup().await;
    h3.ok("POST", "/api/account/domains", Some(json!({"domain": "mail.brandnew-soap.test"}))).await;
    let v: String = sqlx::query_scalar("SELECT verdict FROM screenings WHERE kind = 'domain'").fetch_one(&h3.state.db).await.unwrap();
    assert_eq!(v, "review");
    assert_eq!(standing(&h3).await, "pending");
    h.cleanup().await;
    h2.cleanup().await;
    h3.cleanup().await;
}

#[tokio::test]
async fn bad_imports_write_nothing() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    let csv = "Email,First Name\nann@x.test,Ann\nbob@x.test,Bob\n";
    let (s, v) = h.req("POST", "/api/profiles/import", Some(json!({"csv": csv, "file_name": "apollo_export.csv", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("bought or scraped"), "{v}");
    let (profiles, lists): (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM profiles), (SELECT count(*) FROM lists)").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((profiles, lists), (0, 0), "nothing written");
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(standing(&h).await, "blocked");
    h.cleanup().await;

    // 20% shared inboxes, on an approved account: refused, and the account goes to review.
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let mut csv = String::from("email\n");
    for i in 0..40 {
        csv.push_str(&format!("person{i}@shop{i}.test\n"));
    }
    for r in ["info", "sales", "support", "contact", "office", "hello", "admin", "billing", "marketing", "help"] {
        csv.push_str(&format!("{r}@corp-{r}.test\n"));
    }
    let (s, v) = h.req("POST", "/api/profiles/import", Some(json!({"csv": csv, "file_name": "customers.csv", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(profiles, 0);
    assert_eq!(standing(&h).await, "review");
    // A suppression file is never checked.
    let csv = "Email\ninfo@a.test\nsales@b.test\n";
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "file_name": "purchased-unsubscribes.csv", "email_consent": "unsubscribed"}))).await;
    h.cleanup().await;
}

#[tokio::test]
async fn an_import_check_sends_no_contact_details() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    net::mail_server("deadmail.test", false);
    let csv = "Email,First Name,Last Name,Phone,Signup Date\nzelda.quinn@shopper.test,Zelda,Quinn,+15035550123,2026-01-02\nmarcus.vale@shopper.test,Marcus,Vale,+15035550124,2026-02-03\nnobody@deadmail.test,No,Body,,2026-03-04\n";
    let r = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "file_name": "newsletter.csv", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(r["created"], 3);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let body = calls[0].to_string();
    for secret in ["zelda", "Zelda", "quinn", "marcus", "Vale", "5550123", "nobody@"] {
        assert!(!body.contains(secret), "{secret} reached the model: {body}");
    }
    assert!(body.contains("shopper.test") && body.contains("Signup Date"), "domains and column names do");
    let stats: Value = sqlx::query_scalar("SELECT signals->'stats' FROM screenings WHERE kind = 'import'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stats["no_mx"], 1, "{stats}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_blocked_account_stops_its_running_campaign() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test", "bob@x.test"]).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let id: Uuid = cid.parse().unwrap();
    dumbmail::campaigns::prepare_send(&h.state, id).await.unwrap();
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1 AND status = 'queued'").bind(id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(queued, 2);
    let (s, _, body) = h
        .raw("POST", "/admin/screening/decide", &[("authorization", ADMIN), ("content-type", "application/json")], &json!({"account_id": account_id(&h).await, "decision": "block", "note": "Cold outreach."}).to_string())
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    h.drain().await;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as("SELECT status, error FROM messages WHERE campaign_id = $1").bind(id).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(rows, vec![("skipped".to_string(), Some("account blocked".to_string())); 2]);
    let (status, error): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM campaigns WHERE id = $1").bind(id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("can't send"));
    assert!(!h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    // Tests are refused too; billing and export still answer.
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["owner@acme.test"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = h.req("GET", "/api/billing", None).await;
    assert_eq!(s, StatusCode::OK);
    h.cleanup().await;
}

#[tokio::test]
async fn a_held_flow_message_waits_then_is_skipped_after_three_days() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    answers(&mut h, "We make handmade soap.").await;
    // No verified sending domain yet: the account stays pending, and the flow can still go live.
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "Welcome", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e", "nodes": [
            {"id": "e", "type": "email", "content": {"subject": "Welcome", "blocks": [{"type": "text", "html": "<p>Thanks for joining.</p>"}]}, "next": null}]}})))
        .await;
    let fid = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    assert_eq!(standing(&h).await, "pending");
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined", "profile": {"email": "new@x.test"}}))).await;
    h.drain().await;
    assert!(!h.state.email.sent().iter().any(|m| m.to == "new@x.test"), "it waits");
    let waits: i64 = sqlx::query_scalar("SELECT count(*) FROM screening_waits").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(waits, 1);
    let next: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT run_at FROM jobs WHERE kind = 'flow_step' ORDER BY id DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert!(next > chrono::Utc::now() + chrono::Duration::minutes(14), "retried no sooner than 15 minutes");
    // Three days on, it's skipped with the reason.
    sqlx::query("UPDATE screening_waits SET since = now() - interval '3 days 1 minute'").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    let (status, error): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM messages WHERE to_address = 'new@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), error.as_deref()), ("skipped", Some("account not approved")));
    assert!(!h.state.email.sent().iter().any(|m| m.to == "new@x.test"));
    h.cleanup().await;
}

#[tokio::test]
async fn a_campaign_nobody_opens_pauses_the_account() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let aid = account_id(&h).await;
    let cid = Uuid::now_v7();
    sqlx::query("INSERT INTO campaigns (id, account_id, name, channel, status, audience, content, send_at) VALUES ($1, $2, 'Spring blast', 'email', 'sent', '{}', '{}', now() - interval '2 days')")
        .bind(cid)
        .bind(aid)
        .execute(&h.state.db)
        .await
        .unwrap();
    for i in 0..300 {
        let pid = Uuid::now_v7();
        sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, $3, 'subscribed')").bind(pid).bind(aid).bind(format!("p{i}@x.test")).execute(&h.state.db).await.unwrap();
        sqlx::query(
            "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, to_address, status, sent_at, opened_at)
             VALUES ($1, $2, $3, 'email', $4, $5, 'delivered', now() - interval '2 days', CASE WHEN $6 THEN now() - interval '1 day' END)",
        )
        .bind(Uuid::now_v7())
        .bind(aid)
        .bind(pid)
        .bind(cid)
        .bind(format!("p{i}@x.test"))
        .bind(i < 6)
        .execute(&h.state.db)
        .await
        .unwrap();
    }
    dumbmail::features::screening::engagement::check_account(&h.state, aid).await.unwrap();
    let reason: Option<String> = sqlx::query_scalar("SELECT sending_paused_reason FROM accounts WHERE id = $1").bind(aid).fetch_one(&h.state.db).await.unwrap();
    let reason = reason.expect("email is paused");
    assert!(reason.contains("unopened rate 98.0%") && reason.contains("Spring blast"), "{reason}");
    assert_eq!(standing(&h).await, "review");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM screenings WHERE kind = 'engagement' AND subject_id = $1").bind(cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    // Checked at most hourly, and never twice for the same campaign.
    sqlx::query("UPDATE screening_engagement_checks SET checked_at = now() - interval '2 hours'").execute(&h.state.db).await.unwrap();
    dumbmail::features::screening::engagement::check_account(&h.state, aid).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM screenings WHERE kind = 'engagement'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn unchanged_content_is_judged_once() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let at = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    assert_eq!(calls.lock().unwrap().len(), 1, "no second model call");
    // Changed content is judged again.
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"content": {"subject": "Fall, now with pear", "blocks": [{"type": "text", "html": soap()}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    assert_eq!(calls.lock().unwrap().len(), 2);
    h.cleanup().await;
}

#[tokio::test]
async fn held_content_goes_out_once_a_person_passes_it() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", "<p>VERDICT_REVIEW our new scents. <a href=\"https://acme.test\">Shop</a></p>", &["ann@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v.to_string().contains("A person is checking this"), "{v}");
    assert_eq!(standing(&h).await, "approved", "the account stays approved");
    assert!(h.state.email.sent().iter().any(|m| m.to == "ops@pl.test" && m.subject.starts_with("Content held")));
    let r = h.ok("GET", "/api/screening", None).await;
    let held = r["held"].as_array().unwrap();
    assert_eq!(held.len(), 1, "{r}");
    let sid = held[0]["screening_id"].as_str().unwrap().to_string();
    // The operator lets it go; the next launch passes without another model call.
    let (s, _, _) = h.raw("POST", "/admin/screening/pass", &[("authorization", ADMIN), ("content-type", "application/json")], &json!({"screening_id": sid}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(h.ok("GET", "/api/screening", None).await["held"].as_array().unwrap().is_empty());
    // Phishing is refused outright by the rules, and the account goes to review.
    let cid = campaign(&mut h, "Action needed", "<p>We saw an unusual sign-in. <a href=\"https://login.evil-site.test\">Verify your account</a></p>", &["bob@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v.to_string().contains("can't be sent"), "{v}");
    assert_eq!(standing(&h).await, "review");
    h.cleanup().await;
}

#[tokio::test]
async fn the_operator_approves_and_the_account_sends() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    let r = answers(&mut h, "VERDICT_REVIEW We sell things.").await;
    assert_eq!(r["status"], "review");
    // No token, a wrong token: nothing.
    let body = json!({"account_id": account_id(&h).await, "decision": "approve"}).to_string();
    let (s, _, _) = h.raw("POST", "/admin/screening/decide", &[("content-type", "application/json")], &body).await;
    assert_ne!(s, StatusCode::OK);
    let (s, _, _) = h.raw("GET", "/admin/screening", &[("authorization", "Bearer nope-nope-nope-nope-nope-nope-nope-nope")], "").await;
    assert_ne!(s, StatusCode::OK);
    let (s, _, list) = h.raw("GET", "/admin/screening", &[("authorization", ADMIN)], "").await;
    assert_eq!(s, StatusCode::OK);
    let list: Value = serde_json::from_str(&list).unwrap();
    assert_eq!(list["accounts"][0]["status"], "review");
    assert!(list["accounts"][0]["screenings"][0]["signals"].is_object(), "the operator sees the signals");
    assert_eq!(list["accounts"][0]["answers"]["monthly_volume"], 2000);
    // A pause is lifted by the same decision.
    sqlx::query("UPDATE accounts SET sending_paused_at = now(), sending_paused_reason = 'x'").execute(&h.state.db).await.unwrap();
    let (s, _, out) = h.raw("POST", "/admin/screening/decide", &[("authorization", ADMIN), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    let out: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["was_paused"], true);
    assert_eq!(standing(&h).await, "approved");
    let src: String = sqlx::query_scalar("SELECT source FROM screenings ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(src, "operator");
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    h.cleanup().await;
}

#[tokio::test]
async fn tests_from_a_pending_account_reach_only_members() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "owner@acme.test"}))).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["ann@x.test"]}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v.to_string().contains("until your account is approved"), "{v}");
    h.ok("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["owner@acme.test"]}))).await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "owner@acme.test" && m.subject.starts_with("[Test]")));
    assert!(!h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    h.cleanup().await;
}

#[tokio::test]
async fn a_refusal_or_a_full_day_goes_to_a_person() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    let r = answers(&mut h, "REFUSE_THIS").await;
    assert_eq!(r["status"], "review");
    h.cleanup().await;
    // Past 30 calls a day, no call is made and a person decides.
    let (mut h, calls2) = screened!(true);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    sqlx::query("INSERT INTO screening_ai_usage (account_id, day, calls) SELECT id, (now() AT TIME ZONE timezone)::date, 30 FROM accounts").execute(&h.state.db).await.unwrap();
    let r = answers(&mut h, "We make handmade soap.").await;
    assert_eq!(r["status"], "review");
    assert!(calls2.lock().unwrap().is_empty());
    drop(calls);
    h.cleanup().await;
}

#[tokio::test]
async fn accounts_from_before_screening_stay_approved() {
    let Ok(admin_url) = std::env::var("TEST_DATABASE_URL") else { return };
    let admin = sqlx::PgPool::connect(&admin_url).await.unwrap();
    let dbname = format!("dumbmail_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {dbname}")).execute(&admin).await.unwrap();
    let url = format!("{}/{dbname}", admin_url.rsplit_once('/').unwrap().0);
    let db = sqlx::PgPool::connect(&url).await.unwrap();
    let before = sqlx::migrate::Migrator { migrations: std::borrow::Cow::Owned(dumbmail::MIGRATOR.iter().filter(|m| m.version < 102).cloned().collect()), ..dumbmail::MIGRATOR };
    before.run(&db).await.unwrap();
    let old = Uuid::now_v7();
    sqlx::query("INSERT INTO accounts (id, public_key, name) VALUES ($1, 'pk_old', 'Old Co')").bind(old).execute(&db).await.unwrap();
    dumbmail::MIGRATOR.run(&db).await.unwrap();
    let new = Uuid::now_v7();
    sqlx::query("INSERT INTO accounts (id, public_key, name) VALUES ($1, 'pk_new', 'New Co')").bind(new).execute(&db).await.unwrap();
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, sending_review FROM accounts ORDER BY created_at").fetch_all(&db).await.unwrap();
    assert_eq!(rows, vec![(old, "approved".to_string()), (new, "pending".to_string())]);
    db.close().await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)")).execute(&admin).await.unwrap();

    // And an approved account sends as before (its content is still checked).
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    assert_eq!(calls.lock().unwrap().len(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn a_scheduled_campaign_edited_after_its_check_is_checked_again_when_it_sends() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let at = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    let (s, v) = h
        .req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"content": {"subject": "Action needed", "blocks": [{"type": "text", "html": "<p>Unusual sign-in. <a href=\"https://login.evil-site.test\">Verify your account</a></p>"}]}})))
        .await;
    if s.is_success() {
        // Still scheduled with the new content: it comes due and is checked before anything goes out.
        sqlx::query("UPDATE campaigns SET send_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
        dumbmail::jobs::scheduler_tick(&h.state, 1).await.unwrap();
        h.fast_forward().await;
        let (status, error): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM campaigns WHERE id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(status, "failed", "{error:?}");
        assert!(error.unwrap_or_default().contains("can't be sent"));
        let skipped: Vec<Option<String>> = sqlx::query_scalar("SELECT error FROM messages WHERE campaign_id = $1::uuid").bind(&cid).fetch_all(&h.state.db).await.unwrap();
        assert!(skipped.iter().all(|e| e.as_deref() == Some("content refused")), "{skipped:?}");
        assert!(!h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
        assert_eq!(standing(&h).await, "review");
    } else {
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn a_saved_block_change_is_checked_for_the_live_flows_that_use_it() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let (s, sb) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Footer", "blocks": [{"type": "text", "html": "<p>Thanks for reading.</p>"}]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{sb}");
    let id = sb["id"].as_str().unwrap().to_string();
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "Welcome", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e", "nodes": [
            {"id": "e", "type": "email", "content": {"subject": "Welcome", "blocks": [{"type": "text", "html": "<p>Hi</p>"}, {"type": "saved", "saved_block_id": id, "blocks": []}]}, "next": null}]}})))
        .await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    let (s, v) = h.req("PATCH", &format!("/api/content/saved-blocks/{id}"), Some(json!({"blocks": [{"type": "text", "html": "<p>VERDICT_REVIEW new footer</p>"}]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(calls.lock().unwrap().len(), 2);
    let html: Value = sqlx::query_scalar("SELECT blocks FROM saved_blocks WHERE id = $1::uuid").bind(&id).fetch_one(&h.state.db).await.unwrap();
    assert!(!html.to_string().contains("VERDICT_REVIEW"), "the edit wasn't saved");
    h.ok("PATCH", &format!("/api/content/saved-blocks/{id}"), Some(json!({"blocks": [{"type": "text", "html": "<p>Thanks, and see you soon.</p>"}]}))).await;
    h.cleanup().await;
}

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
/// When set, the stand-in answers 500 (the model is unavailable).
type Down = Arc<std::sync::atomic::AtomicBool>;

/// A stand-in for `POST /v1/messages`: a pass, unless the material contains VERDICT_REVIEW,
/// VERDICT_BLOCK or REFUSE_THIS; SLOW_MODEL answers after 3 seconds.
async fn stand_in() -> (Calls, String, Down) {
    let calls: Calls = Default::default();
    let down: Down = Default::default();
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(|axum::extract::State((c, down)): axum::extract::State<(Calls, Down)>, axum::Json(body): axum::Json<Value>| async move {
                use axum::response::IntoResponse;
                c.lock().unwrap().push(body.clone());
                if down.load(std::sync::atomic::Ordering::SeqCst) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"type": "error"}))).into_response();
                }
                let text = body["messages"][0]["content"].as_str().unwrap_or("").to_string();
                if text.contains("SLOW_MODEL") {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                if text.contains("REFUSE_THIS") {
                    return axum::Json(json!({"model": "claude-opus-5-5", "stop_reason": "refusal", "content": [], "usage": {"input_tokens": 5, "output_tokens": 0}})).into_response();
                }
                let verdict = if text.contains("VERDICT_BLOCK") { "block" } else if text.contains("VERDICT_REVIEW") { "review" } else { "pass" };
                let codes: Vec<&str> = if verdict == "pass" { vec![] } else { vec!["needs_person"] };
                let answer = json!({"verdict": verdict, "reason_codes": codes, "operator_notes": "stand-in notes MODEL-WORDS"}).to_string();
                axum::Json(json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "stop_reason": "end_turn",
                    "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": answer}], "usage": {"input_tokens": 120, "output_tokens": 30}}))
                .into_response()
            }),
        )
        .with_state((calls.clone(), down.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (calls, format!("http://{addr}"), down)
}

fn canned_web() {
    net::offline();
    net::page("acme.test", Some(("Acme Goods - handmade soap from Portland", "We make soap by hand in Portland from local ingredients. Shop bars, gift sets and refills, shipped free over $40. Join our newsletter for new scents.")));
}

/// A screening harness: screening on, the stand-in as the API (or no key).
async fn screened(key: bool) -> Option<(Harness, Calls)> {
    screened_down(key).await.map(|(h, c, _)| (h, c))
}

async fn screened_down(key: bool) -> Option<(Harness, Calls, Down)> {
    canned_web();
    let (calls, url, down) = stand_in().await;
    let h = Harness::with(|c| {
        c.screen_senders = true;
        c.anthropic_api_key = key.then(|| "sk-ant-test-0123456789".into());
        c.anthropic_api_base = Some(url.clone());
    })
    .await?;
    Some((h, calls, down))
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

#[tokio::test]
async fn a_texts_real_link_destinations_are_judged() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    h.credit(5.0).await;
    h.claim_number().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texts"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550142", "sms_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let c = h
        .ok("POST", "/api/campaigns", Some(json!({"name": "T", "channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]},
            "content": {"body": "Unusual sign-in on your account. Verify your account: https://login.evil-site.test/x"}})))
        .await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("can't be sent"), "{v}");
    assert!(calls.lock().unwrap().is_empty(), "the rules refused it");
    h.cleanup().await;
}

// ---------- Review round: one test (or more) per item ----------

use dumbmail::sending::{Conversational, Outcome, SendSpec};

/// A time zone where it's the middle of the day now, so texts aren't held for quiet hours.
fn daytime_tz() -> String {
    (0..24)
        .map(|o| format!("Etc/GMT{}{}", if o >= 12 { "-" } else { "+" }, if o >= 12 { o - 12 } else { o }))
        .find(|tz| {
            let t: chrono_tz::Tz = tz.parse().unwrap();
            (11..17).contains(&chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&t)))
        })
        .unwrap()
}

fn spec<'a>(account: &'a dumbmail::models::Account, profile: &'a dumbmail::models::Profile, channel: &'a str, content: &'a Value) -> SendSpec<'a> {
    SendSpec { account, profile, channel, content, campaign_id: None, flow_id: None, flow_node_id: None, event: None, is_test: false, message_id: None, to_override: None }
}

async fn profile(h: &mut Harness, body: Value) -> dumbmail::models::Profile {
    let p = h.ok("POST", "/api/profiles", Some(body)).await;
    let id: Uuid = p["id"].as_str().unwrap().parse().unwrap();
    dumbmail::api::audience::load_profile(&h.state, account_id(h).await, id).await.unwrap()
}

/// The account as an earlier read saw it (`standing`), whatever the database says now.
async fn snapshot(h: &Harness, standing: &str) -> dumbmail::models::Account {
    let mut a = dumbmail::campaigns::load_account(&h.state.db, account_id(h).await).await.unwrap();
    a.sending_review = Some(standing.into());
    a
}

async fn set_standing(h: &Harness, standing: &str) {
    sqlx::query("UPDATE accounts SET sending_review = $1").bind(standing).execute(&h.state.db).await.unwrap();
}

fn email_content() -> Value {
    json!({"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Our fall scents are here.</p>"}]})
}

// Item 1: texts the account writes itself, and flow notifications.
#[tokio::test]
async fn item1_inbox_texts_the_account_writes_wait_for_approval() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    h.credit(5.0).await;
    let ours = h.claim_number().await;
    let them = "+15035550177";
    let p = profile(&mut h, json!({"phone_number": them})).await;
    let a = dumbmail::campaigns::load_account(&h.state.db, account_id(&h).await).await.unwrap();
    let body = json!({"body": "We're open till 6."});
    let conv = |kind: &'static str| Conversational { kind, sent_by: None, to: them.into(), from: ours.clone() };
    // A reply typed in the inbox: refused with the reason for the reply box.
    let err = dumbmail::sending::send_conversational(&h.state, spec(&a, &p, "sms", &body), &conv("reply")).await.unwrap_err();
    assert!(err.public_message().contains("Not sent") && err.public_message().contains("checked before its first send"), "{}", err.public_message());
    // Keyword and automatic replies don't go out.
    for kind in ["keyword_reply", "auto_reply"] {
        let out = dumbmail::sending::send_conversational(&h.state, spec(&a, &p, "sms", &body), &conv(kind)).await.unwrap();
        assert!(matches!(out, Outcome::Skipped("account not approved")), "{kind}: {out:?}");
    }
    // DumbMail's fixed compliance texts aren't held by the standing (a confirmation only in its
    // default wording).
    let fixed = json!({"body": "Acme VIP: Thanks! You're signed up for texts. Up to 4 msgs/month. Msg & data rates may apply. Reply HELP for help, STOP to cancel."});
    for kind in ["help", "confirmation", "optin_request"] {
        let out = dumbmail::sending::send_conversational(&h.state, spec(&a, &p, "sms", &fixed), &conv(kind)).await;
        assert!(!matches!(out, Ok(Outcome::Skipped("account not approved" | "account blocked"))), "{kind}: {out:?}");
    }
    let out = dumbmail::sending::send_conversational(&h.state, spec(&a, &p, "sms", &body), &conv("confirmation")).await.unwrap();
    assert!(matches!(out, Outcome::Skipped("account not approved")), "a keyword's own words aren't fixed wording: {out:?}");
    // While blocked nothing goes, compliance texts included.
    set_standing(&h, "blocked").await;
    let blocked = snapshot(&h, "blocked").await;
    let out = dumbmail::sending::send_conversational(&h.state, spec(&blocked, &p, "sms", &fixed), &conv("help")).await.unwrap();
    assert!(matches!(out, Outcome::Skipped("account blocked")), "{out:?}");
    set_standing(&h, "pending").await;
    // The final claim reads the standing again: an approved snapshot doesn't let a keyword reply through.
    let stale = snapshot(&h, "approved").await;
    let out = dumbmail::sending::send_conversational(&h.state, spec(&stale, &p, "sms", &body), &conv("keyword_reply")).await.unwrap();
    assert!(matches!(out, Outcome::Skipped("account not approved")), "{out:?}");
    h.cleanup().await;
}

#[tokio::test]
async fn item1_flow_notifications_reach_only_members_until_approved() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    let def = json!({"entry": "n", "nodes": [{"id": "n", "type": "notify", "to": ["owner@acme.test", "pm@outside.test"], "subject": "New lead: {{ person.email }}", "body": "Call them.", "next": null}]});
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "N", "trigger": {"type": "metric", "metric": "Lead"}, "definition": def}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    sqlx::query("UPDATE flow_notify_addresses SET confirmed_at = now()").execute(&h.state.db).await.unwrap();
    let notes = |h: &Harness, to: &str| h.state.email.sent().into_iter().filter(|e| e.to == to && e.subject.starts_with("New lead")).count();
    h.ok("POST", "/api/events", Some(json!({"metric": "Lead", "profile": {"email": "g1@x.test"}}))).await;
    h.drain().await;
    assert_eq!(notes(&h, "owner@acme.test"), 1, "members still get them");
    assert_eq!(notes(&h, "pm@outside.test"), 0, "outsiders wait for approval");
    let why: Vec<Option<String>> = sqlx::query_scalar("SELECT error FROM flow_notifications WHERE to_email = 'pm@outside.test'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(why, vec![Some("account not approved".to_string())]);
    // Blocked: nobody.
    set_standing(&h, "blocked").await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Lead", "profile": {"email": "g2@x.test"}}))).await;
    h.drain().await;
    assert_eq!(notes(&h, "owner@acme.test"), 1);
    // Approved: everyone confirmed.
    set_standing(&h, "approved").await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Lead", "profile": {"email": "g3@x.test"}}))).await;
    h.drain().await;
    assert_eq!((notes(&h, "owner@acme.test"), notes(&h, "pm@outside.test")), (2, 1));
    h.cleanup().await;
}

// Item 2: the final claim reads the standing again.
#[tokio::test]
async fn item2_the_final_claim_reads_the_standing_again() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await;
    h.ok("PATCH", "/api/account", Some(json!({"timezone": daytime_tz()}))).await;
    let p = profile(&mut h, json!({"email": "ann@x.test", "email_consent": "subscribed", "phone_number": "+15035550188", "sms_consent": "subscribed"})).await;
    set_standing(&h, "blocked").await;
    // The send started from an approved read of the account; the claim sees it's blocked now.
    let stale = snapshot(&h, "approved").await;
    let content = email_content();
    let out = dumbmail::sending::send(&h.state, spec(&stale, &p, "email", &content)).await.unwrap();
    assert!(matches!(out, Outcome::Skipped("account blocked")), "{out:?}");
    let sms = json!({"body": "New scents today."});
    let out = dumbmail::sending::send(&h.state, spec(&stale, &p, "sms", &sms)).await.unwrap();
    assert!(matches!(out, Outcome::Skipped("account blocked")), "{out:?}");
    assert!(!h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    assert!(h.state.sms.sent().is_empty());
    // A test email while pending: only to a current member, checked at the end too.
    set_standing(&h, "pending").await;
    let mut test = spec(&stale, &p, "email", &content);
    test.is_test = true;
    test.to_override = Some("stranger@x.test".into());
    let out = dumbmail::sending::send(&h.state, test).await.unwrap();
    assert!(matches!(out, Outcome::Skipped("account not approved")), "{out:?}");
    let mut test = spec(&stale, &p, "email", &content);
    test.is_test = true;
    test.to_override = Some("owner@acme.test".into());
    let out = dumbmail::sending::send(&h.state, test).await.unwrap();
    assert!(matches!(out, Outcome::Sent(_)), "{out:?}");
    assert!(!h.state.email.sent().iter().any(|m| m.to == "stranger@x.test"));
    h.cleanup().await;
}

// Item 3: decisions are one conditional statement; a check and its content check write one decision.
#[tokio::test]
async fn item3_an_operator_decision_never_erases_something_newer() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    set_standing(&h, "review").await;
    let aid = account_id(&h).await;
    let (_, _, list) = h.raw("GET", "/admin/screening", &[("authorization", ADMIN)], "").await;
    let seen = serde_json::from_str::<Value>(&list).unwrap()["accounts"][0]["checked_at"].clone();
    // An engagement pause lands after the operator looked.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    sqlx::query("UPDATE accounts SET sending_paused_at = now(), sending_paused_reason = 'unopened rate 97%'").execute(&h.state.db).await.unwrap();
    let body = json!({"account_id": aid, "decision": "approve", "note": "", "seen_at": seen}).to_string();
    let (s, _, out) = h.raw("POST", "/admin/screening/decide", &[("authorization", ADMIN), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::CONFLICT, "{out}");
    let (st, paused): (String, Option<String>) = sqlx::query_as("SELECT sending_review, sending_paused_reason FROM accounts").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((st.as_str(), paused.as_deref()), ("review", Some("unopened rate 97%")), "nothing was erased");
    h.cleanup().await;
}

#[tokio::test]
async fn item3_a_first_check_and_its_content_check_write_one_decision() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    h.ok("POST", "/api/screening/request", Some(json!({"business": "Soap", "list_source": "Our website form", "monthly_volume": 500}))).await;
    // The answers alone don't run the check here (claimed elsewhere): reset to pending with answers.
    set_standing(&h, "pending").await;
    sqlx::query("DELETE FROM screenings").execute(&h.state.db).await.unwrap();
    h.state.email.clear_sent();
    let cid = campaign(&mut h, "Action needed", "<p>Unusual sign-in. <a href=\"https://login.evil-site.test\">Verify your account</a></p>", &["ann@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(standing(&h).await, "review");
    assert!(!h.state.email.sent().iter().any(|m| m.to == "owner@acme.test" && m.subject.contains("approved")), "approval was never published");
    let account_verdicts: Vec<String> = sqlx::query_scalar("SELECT verdict FROM screenings WHERE kind = 'account'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(account_verdicts, vec!["review".to_string()]);
    h.cleanup().await;
}

// Item 4: what passes is what sends.
#[tokio::test]
async fn item4_every_version_language_and_hidden_block_is_judged() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "ann@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let aud = json!({"include": [{"type": "list", "id": list["id"]}]});
    let clean = json!({"type": "text", "html": "<p>Fall scents.</p>"});
    let tries = [
        // An A/B version.
        json!({"content": {"subject": "A", "blocks": [clean]}, "ab_test": {"variants": [{"id": "b", "content": {"subject": "B", "blocks": [{"type": "text", "html": "<p>VERDICT_REVIEW b</p>"}]}}]}}),
        // A language version.
        json!({"content": {"subject": "A", "blocks": [{"type": "text", "html": "<p>Fall scents.</p>", "i18n": {"fr": {"html": {"text": "<p>VERDICT_REVIEW fr</p>", "status": "reviewed", "source": dumbmail::features::translations::fields::hash("<p>Fall scents.</p>")}}}}]}}),
        // One side of a template condition the sample person never sees.
        json!({"content": {"subject": "A", "blocks": [{"type": "text", "html": "<p>{% if person.properties.vip %}VERDICT_REVIEW vip{% else %}Hi{% endif %}</p>"}]}}),
    ];
    for (i, t) in tries.iter().enumerate() {
        let mut body = json!({"name": format!("C{i}"), "channel": "email", "audience": aud});
        for (k, v) in t.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, c) = h.req("POST", "/api/campaigns", Some(body)).await;
        assert!(s.is_success(), "{i}: {c}");
        let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{i}: {v}");
        assert!(v.to_string().contains("A person is checking this"), "{i}: {v}");
    }
    assert_eq!(calls.lock().unwrap().len(), 3);
    h.cleanup().await;
}

#[tokio::test]
async fn item4_saved_blocks_and_review_request_steps_are_judged() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let (s, sb) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Footer", "blocks": [{"type": "text", "html": "<p>VERDICT_REVIEW footer</p>"}]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{sb}");
    let content = json!({"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hi</p>"}, {"type": "saved", "saved_block_id": sb["id"], "blocks": []}]});
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "ann@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "S", "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // A review request step.
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "R", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "r", "nodes": [
            {"id": "r", "type": "review_request", "channel": "email", "content": {"subject": "How was it?", "blocks": [{"type": "text", "html": "<p>VERDICT_REVIEW tell us</p>"}]}, "next": null}]}})))
        .await;
    let (s, v) = h.req("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn item4_more_than_a_check_covers_is_held_for_a_person() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let n = dumbmail::features::screening::content::MAX_MESSAGES + 1;
    let nodes: Vec<Value> = (0..n)
        .map(|i| json!({"id": format!("e{i}"), "type": "email", "content": {"subject": format!("Step {i}"), "blocks": [{"type": "text", "html": format!("<p>Hello {i}</p>")}]}, "next": if i + 1 < n { json!(format!("e{}", i + 1)) } else { Value::Null }}))
        .collect();
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "Long", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e0", "nodes": nodes}}))).await;
    let (s, v) = h.req("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("too large for our automatic check"), "{v}");
    assert!(calls.lock().unwrap().is_empty(), "nothing cut short and sent");
    h.cleanup().await;
}

#[tokio::test]
async fn item4_a_live_flow_changed_outside_a_check_waits_for_one() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "W", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e", "nodes": [
            {"id": "e", "type": "email", "content": {"subject": "Welcome", "blocks": [{"type": "text", "html": "<p>Thanks for joining.</p>"}]}, "next": null}]}})))
        .await;
    let fid = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    // Changed by a path no editor check saw (a translation promotion, a direct write).
    sqlx::query("UPDATE flows SET definition = jsonb_set(definition, '{nodes,0,content,blocks,0,html}', '\"<p>VERDICT_REVIEW new words</p>\"') WHERE id = $1::uuid").bind(&fid).execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined", "profile": {"email": "new@x.test"}}))).await;
    h.drain().await;
    assert!(!h.state.email.sent().iter().any(|m| m.to == "new@x.test"), "unchecked content doesn't go out");
    assert_eq!(calls.lock().unwrap().len(), 2, "checked on the background lane");
    let held: String = sqlx::query_scalar("SELECT verdict FROM screenings WHERE kind = 'content' ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(held, "review");
    h.fast_forward().await;
    assert!(!h.state.email.sent().iter().any(|m| m.to == "new@x.test"), "held content keeps waiting");
    h.cleanup().await;
}

// Item 5: a "suppression" file that subscribes texts is an ordinary import.
#[tokio::test]
async fn item5_a_suppression_import_cant_subscribe_anyone() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    let csv = "email,phone\nann@x.test,+15035550101\n";
    let (s, v) = h.req("POST", "/api/profiles/import", Some(json!({"csv": csv, "email_consent": "unsubscribed", "sms_consent": "subscribed"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "permission is needed: {v}");
    let (s, v) = h.req("POST", "/api/profiles/import", Some(json!({"csv": csv, "file_name": "apollo_export.csv", "email_consent": "unsubscribed", "sms_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "and it's screened: {v}");
    assert!(v.to_string().contains("bought or scraped"), "{v}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);
    h.cleanup().await;
}

// Item 6: only allow-listed, redacted fields reach the model; customers see our own words.
#[tokio::test]
async fn item6_no_address_phone_or_token_reaches_the_model() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    let r = h
        .ok("POST", "/api/screening/request", Some(json!({"business": "VERDICT_REVIEW Soap. Call me on +1 503 555 0188 or write jane.doe@corp.test", "list_source": "Our website form", "monthly_volume": 500})))
        .await;
    assert_eq!(r["status"], "review");
    approve_directly(&h).await;
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nann@x.test\n", "file_name": "from bob.smith@corp.test.csv", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let cid = campaign(&mut h, "Fall", "<p>Shop <a href=\"https://acme.test/fall?token=SECRET123TOKEN&amp;u=ann@x.test\">now</a></p>", &["zed@x.test"]).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    let all = serde_json::to_string(&calls).unwrap();
    for bad in ["555 0188", "5550188", "jane.doe@corp.test", "bob.smith@corp.test", "SECRET123TOKEN", "ann@x.test", "zed@x.test"] {
        assert!(!all.contains(bad), "{bad} reached the model");
    }
    // The model's own words never reach the customer.
    set_standing(&h, "review").await;
    let mine = h.ok("GET", "/api/screening", None).await.to_string();
    assert!(!mine.contains("MODEL-WORDS") && !mine.contains("stand-in"), "{mine}");
    assert!(mine.contains("A person needs to look at this."), "our own sentence: {mine}");
    h.cleanup().await;
}

// Item 7: a brand exception needs proof.
#[tokio::test]
async fn item7_a_typed_website_isnt_proof_of_a_brand() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    h.ok("PATCH", "/api/account", Some(json!({"website": "https://www.paypal.com"}))).await;
    let (s, v) = h.req("POST", "/api/account/domains", Some(json!({"domain": "paypal-secure-login.com"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // An operator confirmed the account belongs to the brand.
    let aid = account_id(&h).await;
    let body = json!({"account_id": aid, "decision": "approve", "note": "PayPal's own team", "affiliated_brands": ["paypal"]}).to_string();
    let (s, _, out) = h.raw("POST", "/admin/screening/decide", &[("authorization", ADMIN), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    h.ok("POST", "/api/account/domains", Some(json!({"domain": "paypal-secure-login.com"}))).await;
    h.cleanup().await;
}

// Item 8: real send times, delivery events, and flow mail waiting during an engagement review.
async fn engagement_campaign(h: &Harness, send_at: &str, sent_at: &str, status: &str, n: usize, opened: usize) -> Uuid {
    let aid = account_id(h).await;
    let cid = Uuid::now_v7();
    sqlx::query(&format!("INSERT INTO campaigns (id, account_id, name, channel, status, audience, content, send_at) VALUES ($1, $2, 'Blast', 'email', 'sent', '{{}}', '{{}}', now() - interval '{send_at}')"))
        .bind(cid)
        .bind(aid)
        .execute(&h.state.db)
        .await
        .unwrap();
    for i in 0..n {
        let pid = Uuid::now_v7();
        sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, $3, 'subscribed')").bind(pid).bind(aid).bind(format!("{cid}-{i}@x.test")).execute(&h.state.db).await.unwrap();
        sqlx::query(&format!(
            "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, to_address, status, sent_at, opened_at) VALUES ($1, $2, $3, 'email', $4, 'x@x.test', $5, now() - interval '{sent_at}', CASE WHEN $6 THEN now() END)"
        ))
        .bind(Uuid::now_v7())
        .bind(aid)
        .bind(pid)
        .bind(cid)
        .bind(status)
        .bind(i < opened)
        .execute(&h.state.db)
        .await
        .unwrap();
    }
    cid
}

#[tokio::test]
async fn item8_engagement_uses_real_send_times_and_deliveries() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let aid = account_id(&h).await;
    // Started two days ago, but every email went out an hour ago: too soon to judge.
    engagement_campaign(&h, "2 days", "1 hour", "delivered", 300, 0).await;
    // Accepted by the provider two days ago, no delivery event: not counted as delivered.
    engagement_campaign(&h, "2 days", "2 days", "sent", 300, 0).await;
    dumbmail::features::screening::engagement::check_account(&h.state, aid).await.unwrap();
    let paused: Option<String> = sqlx::query_scalar("SELECT sending_paused_reason FROM accounts").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(paused, None);
    h.cleanup().await;
}

#[tokio::test]
async fn item8_flow_email_waits_through_an_engagement_review_per_run() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "W", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e", "nodes": [
            {"id": "e", "type": "email", "content": {"subject": "Welcome", "blocks": [{"type": "text", "html": "<p>Thanks for joining.</p>"}]}, "next": null}]}})))
        .await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    // An engagement check paused email and put the account in review.
    sqlx::query("UPDATE accounts SET sending_review = 'review', sending_paused_at = now(), sending_paused_reason = 'unopened rate 98%'").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined", "profile": {"email": "new@x.test"}}))).await;
    h.drain().await;
    let skipped: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE to_address = 'new@x.test' AND status = 'skipped'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(skipped, 0, "it waits instead of being skipped by the pause");
    let (key, run): (String, Uuid) = sqlx::query_as("SELECT w.run_key, r.id FROM screening_waits w JOIN flow_runs r ON r.flow_id = w.flow_id").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(key, run.to_string(), "tracked per flow run and step");
    h.cleanup().await;
}

// Item 9: self-hosted is untouched.
#[tokio::test]
async fn item9_self_hosted_screens_nothing() {
    canned_web();
    let (calls, url, _) = stand_in().await;
    let Some(mut h) = Harness::with(|c| {
        c.hosted = false;
        c.screen_senders = true;
        c.anthropic_api_key = Some("sk-ant-test-0123456789".into());
        c.anthropic_api_base = Some(url.clone());
    })
    .await
    else {
        return;
    };
    h.signup().await;
    assert_eq!(standing(&h).await, "pending", "the column's default, which nothing reads here");
    for (m, p) in [("GET", "/admin/screening"), ("POST", "/admin/screening/decide"), ("POST", "/admin/screening/pass")] {
        let (s, _, _) = h.raw(m, p, &[("authorization", ADMIN), ("content-type", "application/json")], "{}").await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {p}");
    }
    let cid = campaign(&mut h, "Fall", "<p>VERDICT_BLOCK whatever</p>", &["ann@x.test"]).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    assert!(calls.lock().unwrap().is_empty());
    h.cleanup().await;
}

// Item 10: limits and deadlines.
#[tokio::test]
async fn item10_the_daily_limit_uses_a_fixed_pacific_day() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    // A time zone whose date differs from Pacific's right now.
    let tz: String = sqlx::query_scalar(
        "SELECT z FROM unnest(ARRAY['Pacific/Kiritimati', 'Etc/GMT+12']) z WHERE (now() AT TIME ZONE z)::date <> (now() AT TIME ZONE 'America/Los_Angeles')::date LIMIT 1",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    h.ok("PATCH", "/api/account", Some(json!({"timezone": tz}))).await;
    sqlx::query("INSERT INTO screening_ai_usage (account_id, day, calls) SELECT id, (now() AT TIME ZONE 'America/Los_Angeles')::date, 30 FROM accounts").execute(&h.state.db).await.unwrap();
    let r = answers(&mut h, "We make handmade soap.").await;
    assert_eq!(r["status"], "review");
    assert!(calls.lock().unwrap().is_empty(), "changing the time zone doesn't reset the day");
    h.cleanup().await;
}

#[tokio::test]
async fn item10_a_due_campaign_is_checked_on_the_background_lane() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let at = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    sqlx::query("UPDATE campaigns SET content = jsonb_set(content, '{subject}', '\"Fall, now with pear\"'), send_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
    dumbmail::jobs::scheduler_tick(&h.state, 1).await.unwrap();
    // Run the sending lane only: the batch puts itself off and asks the background lane.
    loop {
        let job: Option<(i64, String, Value)> = sqlx::query_as("SELECT id, kind, payload FROM jobs WHERE lane = 'main' AND run_at <= now() ORDER BY id LIMIT 1").fetch_optional(&h.state.db).await.unwrap();
        let Some((id, kind, payload)) = job else { break };
        sqlx::query("DELETE FROM jobs WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
        match kind.as_str() {
            "campaign_send" => dumbmail::campaigns::prepare_send(&h.state, payload["campaign_id"].as_str().unwrap().parse().unwrap()).await.unwrap(),
            "campaign_batch" => {
                let ids: Vec<Uuid> = serde_json::from_value(payload["message_ids"].clone()).unwrap();
                dumbmail::campaigns::send_batch(&h.state, payload["campaign_id"].as_str().unwrap().parse().unwrap(), &ids).await.unwrap()
            }
            _ => {}
        }
    }
    assert_eq!(calls.lock().unwrap().len(), 1, "no Claude call on the sending lane");
    assert!(!h.state.email.sent().iter().any(|m| m.to == "ann@x.test"));
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE lane = 'background' AND kind = 'screening_content_check'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(queued, 1);
    // The check runs; the batch, waiting a minute between looks, goes once it passed.
    h.fast_forward().await;
    h.fast_forward().await;
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"), "it goes once the check passes");
    h.cleanup().await;
}

#[tokio::test]
async fn item10_a_rules_only_pass_is_judged_again_when_claude_is_back() {
    let Some((mut h, calls, down)) = screened_down(true).await else { return };
    h.signup().await;
    approve_directly(&h).await;
    // An established sender: 1,000 real emails, the first 15 days ago.
    let aid = account_id(&h).await;
    let pid = profile(&mut h, json!({"email": "old@x.test", "email_consent": "subscribed"})).await.id;
    sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address, status, sent_at, created_at) SELECT gen_random_uuid(), $1, $2, 'email', 'old@x.test', 'delivered', now() - interval '15 days', now() - interval '15 days' FROM generate_series(1, 1000)")
        .bind(aid)
        .bind(pid)
        .execute(&h.state.db)
        .await
        .unwrap();
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let at = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    let src: String = sqlx::query_scalar("SELECT source FROM screenings WHERE kind = 'content' ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(src, "rules");
    down.store(false, std::sync::atomic::Ordering::SeqCst);
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    let before = calls.lock().unwrap().len();
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;
    assert_eq!(calls.lock().unwrap().len(), before + 1, "judged again now that Claude answers");
    h.cleanup().await;
}

#[tokio::test]
async fn item10_the_deadline_covers_the_whole_launch() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    dumbmail::features::screening::content::testing::deadline(account_id(&h).await, 1000);
    let cid = campaign(&mut h, "Fall", "<p>SLOW_MODEL scents</p>", &["ann@x.test"]).await;
    let t = std::time::Instant::now();
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert!(!s.is_success());
    assert!(t.elapsed() < std::time::Duration::from_millis(2500), "{:?}", t.elapsed());
    h.cleanup().await;
}

// Item 11: a failed lookup is unknown, not "no mail server".
#[tokio::test]
async fn item11_failed_mail_lookups_are_unknown() {
    let (mut h, _) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    let mut csv = String::from("email\n");
    for i in 0..30 {
        let d = format!("slowdns{i}.test");
        net::mail_lookup_fails(&d);
        csv.push_str(&format!("p{i}@{d}\n"));
    }
    for i in 0..20 {
        let d = format!("deadmx{i}.test");
        if i < 5 {
            net::mail_server(&d, false);
        }
        csv.push_str(&format!("q{i}@{d}\n"));
    }
    // 5 of the 20 checked (25%) can't receive mail, but only 40% of the file could be checked.
    let r = h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "file_name": "customers.csv", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(r["created"], 50, "not blocked: {r}");
    let (verdict, checked): (String, i64) = sqlx::query_as("SELECT verdict, (signals->'stats'->>'mx_checked')::bigint FROM screenings WHERE kind = 'import'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((verdict.as_str(), checked), ("review", 20));
    h.cleanup().await;
}

// ---------- Rulings: nothing live waits after deploy; no key isn't an outage ----------

#[tokio::test]
async fn ruling1_content_live_at_deploy_sends_at_once_and_later_edits_are_checked() {
    let (mut h, calls) = screened!(true);
    h.signup().await;
    approve_directly(&h).await;
    // Live before screening started (turned on and scheduled directly, with no check).
    let f = h
        .ok("POST", "/api/flows", Some(json!({"name": "W", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e", "nodes": [
            {"id": "e", "type": "email", "content": {"subject": "Welcome", "blocks": [{"type": "text", "html": "<p>Thanks for joining.</p>"}]}, "next": null}]}})))
        .await;
    let fid = f["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE flows SET status = 'live' WHERE id = $1::uuid").bind(&fid).execute(&h.state.db).await.unwrap();
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    sqlx::query("UPDATE campaigns SET status = 'scheduled', send_at = now() + interval '1 day' WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
    // The deploy step (once; running it again records nothing more).
    let n = dumbmail::features::screening::content::record_existing(&h.state).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(dumbmail::features::screening::content::record_existing(&h.state).await.unwrap(), 0, "once");
    let src: Vec<String> = sqlx::query_scalar("SELECT source FROM screenings WHERE kind = 'content'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(src, vec!["existing".to_string(), "existing".to_string()]);
    // Both send at once, with no model call.
    h.ok("POST", "/api/profiles", Some(json!({"email": "new@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined", "profile": {"email": "new@x.test"}}))).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "new@x.test"), "the live flow sends at once");
    sqlx::query("UPDATE campaigns SET send_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
    dumbmail::jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"), "the scheduled campaign sends at once");
    assert!(calls.lock().unwrap().is_empty());
    // An edit afterwards is checked as usual.
    sqlx::query("UPDATE flows SET definition = jsonb_set(definition, '{nodes,0,content,blocks,0,html}', '\"<p>New words.</p>\"') WHERE id = $1::uuid").bind(&fid).execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/profiles", Some(json!({"email": "next@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined", "profile": {"email": "next@x.test"}}))).await;
    h.drain().await;
    assert_eq!(calls.lock().unwrap().len(), 1, "the edited content was checked");
    h.cleanup().await;
}

#[tokio::test]
async fn ruling2_with_no_key_an_approved_accounts_content_is_judged_by_the_rules() {
    let (mut h, calls) = screened!(false);
    h.signup().await;
    approve_directly(&h).await;
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test"), "a rules pass goes out");
    let src: String = sqlx::query_scalar("SELECT source FROM screenings WHERE kind = 'content'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(src, "rules");
    // A rules review still holds it.
    let cid = campaign(&mut h, "Re: our call", "<p>Following up.</p>", &["bob@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(standing(&h).await, "approved");
    assert!(calls.lock().unwrap().is_empty());
    // The operator page says so.
    let (_, _, list) = h.raw("GET", "/admin/screening", &[("authorization", ADMIN)], "").await;
    assert!(list.contains("No ANTHROPIC_API_KEY is set"), "{list}");
    h.cleanup().await;
}

#[tokio::test]
async fn ruling2_with_no_key_a_pending_accounts_first_check_goes_to_a_person() {
    let (mut h, _) = screened!(false);
    h.signup().await;
    verify_domain(&h, "acme.test").await;
    let r = answers(&mut h, "We make handmade soap.").await;
    assert_eq!(r["status"], "review");
    h.cleanup().await;
}

#[tokio::test]
async fn ruling2_a_key_with_claude_failing_keeps_the_established_rule() {
    let Some((mut h, _, down)) = screened_down(true).await else { return };
    h.signup().await;
    approve_directly(&h).await;
    down.store(true, std::sync::atomic::Ordering::SeqCst);
    // Not established (no sending history): held for a person.
    let cid = campaign(&mut h, "Fall", soap(), &["ann@x.test"]).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("A person is checking this"), "{v}");
    h.cleanup().await;
}

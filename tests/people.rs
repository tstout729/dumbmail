//! Profile tools: consent history, suppression list, merge, bulk actions, privacy requests.

#[macro_use]
mod common;
use common::*;

use std::collections::HashSet;

use axum::http::StatusCode;
use dumbmail::features::people::merge::PROFILE_TABLES;
use serde_json::{json, Value};
use uuid::Uuid;

async fn methods(h: &Harness, email: &str) -> Vec<(String, String, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT e.method, e.channel || ':' || e.field, e.new_value, e.ip FROM consent_events e JOIN profiles p ON p.id = e.profile_id
         WHERE p.email = $1 ORDER BY e.occurred_at, e.id",
    )
    .bind(email)
    .fetch_all(&h.state.db)
    .await
    .unwrap()
}

async fn pid(h: &Harness, email: &str) -> String {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap();
    id.to_string()
}

async fn send_campaign(h: &mut Harness, list_id: &Value) {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list_id}]}, "content": {"subject": "Spring sale", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
}

fn unsub_path(h: &Harness, to: &str) -> String {
    let mail = h.state.email.sent().into_iter().rev().find(|m| m.to == to).unwrap_or_else(|| panic!("no mail to {to}"));
    mail.unsubscribe_url.unwrap().trim_start_matches("https://pl.test").to_string()
}

async fn ses(h: &Harness, to: &str, kind: &str) {
    let pid: String = sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE to_address = $1 AND NOT is_test ORDER BY created_at DESC LIMIT 1").bind(to).fetch_one(&h.state.db).await.unwrap();
    let msg = if kind == "Complaint" {
        json!({"notificationType": "Complaint", "mail": {"messageId": pid}, "complaint": {}})
    } else {
        json!({"notificationType": "Bounce", "mail": {"messageId": pid}, "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}})
    };
    let sns = json!({"Type": "Notification", "Message": msg.to_string()});
    let (s, _, body) = h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &sns.to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

async fn mcp(h: &Harness, key: &str, tool: &str, args: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}).to_string();
    let (s, _, out) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["result"]["isError"], false, "{tool}: {v}");
    v["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn consent_history_records_every_path() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.credit(10.0).await;
    let our_number = h.claim_number().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News", "show_on_preferences": true}))).await;

    // Form sign-up, with the visitor's address and browser.
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list["id"], "config": {"collect": ["email", "phone_number"]}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    let (s, _, body) = h
        .raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json"), ("x-real-ip", "203.0.113.9"), ("user-agent", "TestBrowser/1")],
            &json!({"form_id": fid, "email": "form@x.test", "phone_number": "+15035550111", "sms_consent": true}).to_string())
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let m = methods(&h, "form@x.test").await;
    assert!(m.iter().any(|r| r.0 == "form" && r.1 == "email:consent" && r.2.as_deref() == Some("subscribed") && r.3.as_deref() == Some("203.0.113.9")), "{m:?}");
    assert!(m.iter().any(|r| r.0 == "form" && r.1 == "sms:consent"), "{m:?}");
    let ua: Option<String> = sqlx::query_scalar("SELECT user_agent FROM consent_events WHERE method = 'form' LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ua.as_deref(), Some("TestBrowser/1"));

    // CSV import, API key, dashboard edit (by this member), MCP.
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nimp@x.test\n", "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    assert_eq!(methods(&h, "imp@x.test").await[0].0, "import");
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "k"}))).await["key"].as_str().unwrap().to_string();
    let (s, _, _) = h.raw("POST", "/api/profiles", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &json!({"email": "api@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(methods(&h, "api@x.test").await[0].0, "api");
    let api_id = pid(&h, "api@x.test").await;
    h.ok("PATCH", &format!("/api/profiles/{api_id}"), Some(json!({"sms_consent": "unsubscribed", "email_consent": "subscribed"}))).await;
    let edit: (String, Option<Uuid>) = sqlx::query_as("SELECT method, actor_user_id FROM consent_events WHERE profile_id = $1::uuid AND channel = 'sms'").bind(&api_id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(edit.0, "dashboard");
    assert_eq!(edit.1.map(|u| u.to_string()), me["user"]["id"].as_str().map(String::from), "the member who edited");
    assert_eq!(methods(&h, "api@x.test").await.len(), 2, "an unchanged email consent isn't a change");
    mcp(&h, &key, "upsert_profile", json!({"email": "mcp@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]})).await;
    assert_eq!(methods(&h, "mcp@x.test").await[0].0, "mcp");
    mcp(&h, &key, "import_profiles", json!({"csv": "email\nmcpimp@x.test\n", "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true})).await;
    let via: (String, String) = sqlx::query_as("SELECT e.method, e.via FROM consent_events e JOIN profiles p ON p.id = e.profile_id WHERE p.email = 'mcpimp@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(via, ("import".to_string(), "mcp".to_string()));

    // Unsubscribe link, preference page, one-click.
    send_campaign(&mut h, &list["id"]).await;
    let (s, _, _) = h.raw("POST", &unsub_path(&h, "form@x.test"), &[("x-real-ip", "198.51.100.7")], "").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = h.raw("POST", &unsub_path(&h, "imp@x.test"), &[("content-type", "application/x-www-form-urlencoded")], "action=all").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = h.raw("POST", &unsub_path(&h, "api@x.test"), &[("content-type", "application/x-www-form-urlencoded")], "List-Unsubscribe=One-Click").await;
    assert_eq!(s, StatusCode::OK);
    let last = |m: Vec<(String, String, Option<String>, Option<String>)>| m.last().cloned().unwrap();
    let u = last(methods(&h, "form@x.test").await);
    assert_eq!((u.0.as_str(), u.2.as_deref(), u.3.as_deref()), ("unsubscribe_link", Some("unsubscribed"), Some("198.51.100.7")));
    assert_eq!(last(methods(&h, "imp@x.test").await).0, "preference_page");
    assert_eq!(last(methods(&h, "api@x.test").await).0, "one_click");
    let subject: Option<String> = sqlx::query_scalar("SELECT m.subject FROM consent_events e JOIN messages m ON m.id = e.message_id WHERE e.method = 'one_click'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(subject.as_deref(), Some("Spring sale"), "the unsubscribe names the message");

    // STOP and START replies.
    let text = |word: &str| json!({"data": {"event_type": "message.received", "payload": {"text": word, "from": {"phone_number": "+15035550111"}, "to": [{"phone_number": our_number}]}}}).to_string();
    h.raw("POST", "/webhooks/telnyx/hooksecret", &[("content-type", "application/json")], &text("STOP")).await;
    h.raw("POST", "/webhooks/telnyx/hooksecret", &[("content-type", "application/json")], &text("start")).await;
    let sms: Vec<(String, Option<String>)> = sqlx::query_as("SELECT method, new_value FROM consent_events WHERE channel = 'sms' AND method = 'sms_keyword' ORDER BY occurred_at").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(sms, vec![("sms_keyword".into(), Some("unsubscribed".into())), ("sms_keyword".into(), Some("subscribed".into()))]);

    // Hard bounce and spam complaint.
    ses(&h, "mcp@x.test", "Permanent").await;
    ses(&h, "mcpimp@x.test", "Complaint").await;
    let b = last(methods(&h, "mcp@x.test").await);
    assert_eq!((b.0.as_str(), b.1.as_str(), b.2.as_deref()), ("hard_bounce", "email:suppression", Some("hard_bounce")));
    let c = methods(&h, "mcpimp@x.test").await;
    assert!(c.iter().any(|r| r.0 == "spam_complaint" && r.1 == "email:suppression"));
    assert!(c.iter().any(|r| r.0 == "spam_complaint" && r.1 == "email:consent" && r.2.as_deref() == Some("unsubscribed")));

    // Double opt-in: pending on sign-up, then the confirmation.
    let dlist = h.ok("POST", "/api/lists", Some(json!({"name": "Double", "double_opt_in": true}))).await;
    let dform = h.ok("POST", "/api/forms", Some(json!({"name": "D", "list_id": dlist["id"], "config": {"headline": "Join"}}))).await;
    let dfid = dform["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{dfid}"), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": dfid, "email": "doi@x.test"}).to_string()).await;
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "doi@x.test").unwrap();
    let link = find_link(&mail.html, "https://pl.test/c/");
    h.raw("POST", link.trim_start_matches("https://pl.test"), &[], "").await;
    let d = methods(&h, "doi@x.test").await;
    assert_eq!(d.iter().map(|r| (r.0.as_str(), r.2.as_deref())).collect::<Vec<_>>(), vec![("form", Some("pending")), ("double_opt_in", Some("subscribed"))], "{d:?}");

    // The profile page and the MCP tool read it, with words for what and how.
    let form_pid = pid(&h, "form@x.test").await;
    let summary = h.ok("GET", &format!("/api/people/profiles/{form_pid}"), None).await;
    let hist = summary["consent_history"].as_array().unwrap();
    let email_unsub = hist.iter().find(|e| e["channel"] == "email").unwrap();
    assert_eq!(email_unsub["how"], "Unsubscribe link in “Spring sale”", "{hist:?}");
    assert_eq!(email_unsub["summary"], "Email consent: subscribed → unsubscribed");
    assert!(hist.iter().any(|e| e["summary"] == "SMS consent: never subscribed → subscribed"));
    assert!(hist.iter().any(|e| e["how"] == "Sign-up form “Popup”"));
    assert!(summary["history_started_at"].is_string());
    assert_eq!(summary["reachability"]["email"]["can_send"], false);
    assert_eq!(summary["reachability"]["email"]["reasons"][0]["code"], "unsubscribed");
    let via_mcp = mcp(&h, &key, "get_consent_history", json!({"email": "form@x.test"})).await;
    assert_eq!(via_mcp["total"].as_i64(), Some(hist.len() as i64));

    // Every instrumented path left a row.
    let all: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT method FROM consent_events").fetch_all(&h.state.db).await.unwrap().into_iter().collect();
    for m in ["form", "import", "api", "dashboard", "mcp", "unsubscribe_link", "preference_page", "one_click", "sms_keyword", "hard_bounce", "spam_complaint", "double_opt_in"] {
        assert!(all.contains(m), "no consent history row for {m}: {all:?}");
    }
    assert!(!all.contains("other"), "every change says how it happened: {all:?}");
    h.cleanup().await;
}

#[tokio::test]
async fn consent_history_rows_cant_be_changed() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "a@x.test", "email_consent": "subscribed"}))).await;
    let err = sqlx::query("UPDATE consent_events SET new_value = 'unsubscribed'").execute(&h.state.db).await.unwrap_err();
    assert!(err.to_string().contains("cannot be changed"), "{err}");
    h.cleanup().await;
}

/// Every write of a consent or suppression column outside the instrumented places fails this test:
/// record it with `features::people::consent` (see its setters) instead of a raw UPDATE.
#[test]
fn consent_writes_go_through_the_history() {
    let allowed = ["src/audience.rs", "src/features/people/consent.rs", "src/features/people/merge.rs"];
    let cols = ["email_consent =", "sms_consent =", "email_suppressed_reason =", "sms_suppressed_reason =", "ads_opt_out ="];
    let mut stack = vec![std::path::PathBuf::from("src")];
    let mut offenders = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path.to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            for (i, _) in text.match_indices("UPDATE profiles").chain(text.match_indices("INSERT INTO profiles")) {
                let stmt: String = text[i..].chars().take_while(|c| *c != '"').collect();
                if cols.iter().any(|c| stmt.contains(c)) && !allowed.contains(&rel.as_str()) {
                    offenders.push(format!("{rel}: {}", stmt.chars().take(80).collect::<String>()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "consent written without recording history:\n{}", offenders.join("\n"));
    let audience = std::fs::read_to_string("src/audience.rs").unwrap();
    assert!(audience.contains("people::consent::after_upsert"), "the shared upsert must record its consent changes");
}

#[tokio::test]
async fn profile_tables_cover_every_profile_column() {
    let h = harness!();
    let cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.table_name::text, c.column_name::text FROM information_schema.columns c
         JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name AND t.table_type = 'BASE TABLE'
         WHERE c.table_schema = 'public' AND (c.column_name LIKE '%profile_id' OR c.column_name LIKE 'profile\\_%' ESCAPE '\\')
         UNION
         SELECT kcu.table_name::text, kcu.column_name::text FROM information_schema.referential_constraints rc
         JOIN information_schema.key_column_usage kcu ON kcu.constraint_name = rc.constraint_name AND kcu.constraint_schema = rc.constraint_schema
         JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = rc.unique_constraint_name AND ccu.constraint_schema = rc.unique_constraint_schema
         WHERE ccu.table_name = 'profiles'",
    )
    .fetch_all(&h.state.db)
    .await
    .unwrap();
    let listed: HashSet<(String, String)> = PROFILE_TABLES.iter().map(|t| (t.table.to_string(), t.column.to_string())).collect();
    let missing: Vec<_> = cols.iter().filter(|c| !listed.contains(c) && c.0 != "profiles").collect();
    assert!(missing.is_empty(), "add these to features::people::merge::PROFILE_TABLES so merges and deletions cover them: {missing:?}");
    for t in PROFILE_TABLES {
        assert!(cols.iter().any(|c| c.0 == t.table && c.1 == t.column), "{}.{} is listed but doesn't exist", t.table, t.column);
        if t.on_merge == dumbmail::features::people::merge::OnMerge::Custom {
            assert!(dumbmail::features::people::merge::CUSTOM_MERGE.contains(&t.table), "{} needs merge code", t.table);
        }
    }
    h.cleanup().await;
}

#[tokio::test]
async fn suppression_list_rules() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    h.claim_number().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await;
    for e in ["sub@x.test", "keep@x.test"] {
        h.ok("POST", "/api/profiles", Some(json!({"email": e, "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    }
    h.ok("POST", "/api/profiles", Some(json!({"email": "left@x.test", "email_consent": "unsubscribed", "list_ids": [list["id"]]}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550123", "sms_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE email = 'keep@x.test'").execute(&h.state.db).await.unwrap();

    // Dry run first; then suppress (one address is new, one is junk).
    let dry = h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": ["sub@x.test", "new@x.test, (503) 555-0123", "not an address"]}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["count"], 3);
    let r = h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": ["sub@x.test", "new@x.test, (503) 555-0123", "not an address"], "confirm": true}))).await;
    assert_eq!(r["suppressed"], 3, "{r}");
    assert_eq!(r["invalid"][0], "not an address");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'new@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed", "suppressing creates the profile but never subscribes it");

    // Nobody suppressed is reachable.
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}}))).await;
    assert_eq!(est["count"], 0, "{est}");
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}}))).await;
    assert_eq!(est["count"], 0, "a suppressed number isn't texted");

    let all = h.ok("GET", "/api/people/suppressions", None).await;
    assert_eq!(all["total"], 5, "{all}");
    let manual = h.ok("GET", "/api/people/suppressions?reason=manual", None).await;
    assert_eq!(manual["total"], 3);
    let search = h.ok("GET", "/api/people/suppressions?q=left", None).await;
    assert_eq!(search["data"][0]["reason"], "unsubscribed");
    assert_eq!(search["data"][0]["can_unsuppress"], false);
    assert!(search["data"][0]["why_not"].as_str().unwrap().contains("unsubscribed themselves"));
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).format("%Y-%m-%d").to_string();
    assert_eq!(h.ok("GET", &format!("/api/people/suppressions?from={today}&to={today}"), None).await["total"], 5);
    assert_eq!(h.ok("GET", "/api/people/suppressions?to=2020-01-01", None).await["total"], 0);
    let (s, hd, csv) = h.bytes("GET", "/api/people/suppressions/export?channel=email", "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(hd.get("content-disposition").unwrap().to_str().unwrap().contains("attachment"));
    assert_eq!(String::from_utf8(csv).unwrap().lines().count(), 5, "header + 4 email rows");

    // Lifting: manual only; consent stays as it was.
    let sub = pid(&h, "sub@x.test").await;
    let (s, v) = h.req("POST", "/api/people/suppressions/unsuppress", Some(json!({"profile_id": pid(&h, "keep@x.test").await, "channel": "email", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("hard bounced"));
    let (s, _) = h.req("POST", "/api/people/suppressions/unsuppress", Some(json!({"profile_id": pid(&h, "left@x.test").await, "channel": "email", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.ok("POST", "/api/people/suppressions/unsuppress", Some(json!({"profile_id": sub, "channel": "email", "confirm": true}))).await;
    let c: (String, Option<String>) = sqlx::query_as("SELECT email_consent, email_suppressed_reason FROM profiles WHERE email = 'sub@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, ("subscribed".to_string(), None));
    let lifted: i64 = sqlx::query_scalar("SELECT count(*) FROM consent_events WHERE profile_id = $1::uuid AND field = 'suppression' AND new_value IS NULL AND method = 'suppression_list'").bind(&sub).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(lifted, 1);

    // A long list goes to a background job.
    let many: Vec<String> = (0..450).map(|i| format!("bulk{i}@x.test")).collect();
    let r = h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": many, "confirm": true}))).await;
    assert_eq!(r["bulk_action"]["status"], "queued", "{r}");
    h.drain().await;
    let job = h.ok("GET", &format!("/api/people/bulk/{}", r["bulk_action"]["id"].as_str().unwrap()), None).await;
    assert_eq!((job["status"].as_str(), job["processed"].as_i64(), job["succeeded"].as_i64()), (Some("done"), Some(450), Some(450)), "{job}");

    // Another account sees none of it and can't lift it.
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    assert_eq!(h.ok("GET", "/api/people/suppressions", None).await["total"], 0);
    let (s, _) = h.req("POST", "/api/people/suppressions/unsuppress", Some(json!({"profile_id": pid(&h, "new@x.test").await, "channel": "email", "confirm": true}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn merge_moves_everything_and_keeps_the_strictest_consent() {
    let mut h = harness!();
    h.signup().await;
    let l1 = h.ok("POST", "/api/lists", Some(json!({"name": "One"}))).await;
    let l2 = h.ok("POST", "/api/lists", Some(json!({"name": "Two"}))).await;
    let keep = h.ok("POST", "/api/profiles", Some(json!({"email": "keep@x.test", "first_name": "Kay", "email_consent": "subscribed", "properties": {"city": "Portland", "vip": true}, "list_ids": [l1["id"]]}))).await;
    let other = h.ok("POST", "/api/profiles", Some(json!({"email": "old@x.test", "phone_number": "+15035550142", "first_name": "Katherine", "email_consent": "subscribed", "sms_consent": "subscribed", "properties": {"city": "Seattle", "size": "M"}, "list_ids": [l1["id"], l2["id"]]}))).await;
    let (kid, oid) = (keep["id"].as_str().unwrap().to_string(), other["id"].as_str().unwrap().to_string());
    // Both got the same campaign; then the other one complained.
    send_campaign(&mut h, &l1["id"]).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'complaint', email_consent = 'unsubscribed' WHERE id = $1::uuid").bind(&oid).execute(&h.state.db).await.unwrap();
    for (who, m) in [(&keep, "Viewed Product"), (&other, "Placed Order"), (&other, "Placed Order")] {
        h.ok("POST", "/api/events", Some(json!({"metric": m, "profile": {"id": who["id"]}}))).await;
    }
    h.ok("POST", &format!("/api/people/profiles/{oid}/notes"), Some(json!({"body": "Called about a refund"}))).await;

    // Dry run: shows the result, changes nothing.
    let dry = h.ok("POST", "/api/people/merge", Some(json!({"keep_id": kid, "merge_id": oid, "choices": {"first_name": "merge"}}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["result"]["first_name"], "Katherine");
    assert_eq!(dry["result"]["email"], "keep@x.test");
    assert_eq!(dry["result"]["phone_number"], "+15035550142", "filled from the other profile");
    assert_eq!(dry["result"]["properties"], json!({"city": "Portland", "vip": true, "size": "M"}), "kept profile wins on conflict");
    assert_eq!(dry["result"]["email_consent"], "unsubscribed");
    assert_eq!(dry["result"]["email_suppressed_reason"], "complaint");
    assert!(dry["rows_moving"]["events"].as_i64().unwrap() >= 2, "{dry}");
    assert!(dry["fields"].as_array().unwrap().iter().any(|f| f["field"] == "properties.city" && f["conflict"] == true));
    assert!(sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM profiles WHERE id = $1::uuid)").bind(&oid).fetch_one(&h.state.db).await.unwrap());

    // Another account can't merge them.
    let mut h2_cookie = h.cookie.clone();
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let (s, _) = h.req("POST", "/api/people/merge", Some(json!({"keep_id": kid, "merge_id": oid, "confirm": true}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    std::mem::swap(&mut h.cookie, &mut h2_cookie);

    let done = h.ok("POST", "/api/people/merge", Some(json!({"keep_id": kid, "merge_id": oid, "choices": {"first_name": "merge"}, "confirm": true}))).await;
    assert_eq!(done["merged"], true);
    let p = h.ok("GET", &format!("/api/profiles/{kid}"), None).await;
    assert_eq!(p["profile"]["first_name"], "Katherine");
    assert_eq!(p["profile"]["phone_number"], "+15035550142");
    // Most restrictive: email unsubscribed (other), SMS: subscribed (other) vs never (keep) = never.
    assert_eq!(p["profile"]["email_consent"], "unsubscribed");
    assert_eq!(p["profile"]["sms_consent"], "never_subscribed");
    assert_eq!(p["lists"].as_array().unwrap().len(), 2);
    assert_eq!(p["events"].as_array().unwrap().iter().filter(|e| e["metric"] == "Placed Order").count(), 2);
    let msgs = p["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "both messages stay in the history");
    assert_eq!(msgs.iter().filter(|m| m["campaign_id"].is_string()).count(), 1, "one counts toward the campaign");
    let sup: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE id = $1::uuid").bind(&kid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sup.as_deref(), Some("complaint"), "suppression carries over");
    let summary = h.ok("GET", &format!("/api/people/profiles/{kid}"), None).await;
    assert_eq!(summary["notes"][0]["body"], "Called about a refund");
    assert!(summary["consent_history"].as_array().unwrap().iter().any(|e| e["method"] == "merge" && e["new_value"] == "unsubscribed"));
    assert!(summary["consent_history"].as_array().unwrap().iter().any(|e| e["detail"]["merged_from"] == json!(oid)), "the other profile's history comes along");

    // Nothing points at the merged profile any more.
    for t in PROFILE_TABLES {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {} WHERE {} = $1::uuid", t.table, t.column)).bind(&oid).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(n, 0, "{}.{} still points at the merged profile", t.table, t.column);
    }
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members WHERE list_id = $1::uuid").bind(l1["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(members, 1, "one membership per list");

    // The dropped address (old@) had unsubscribed: importing it again doesn't make it reachable.
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nold@x.test\n", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let back: (String, Option<String>) = sqlx::query_as("SELECT email_consent, email_suppressed_reason FROM profiles WHERE email = 'old@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(back, ("unsubscribed".to_string(), Some("complaint".to_string())));
    h.cleanup().await;
}

#[tokio::test]
async fn duplicates_finder_suggests_and_never_merges() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.ok("POST", "/api/profiles", Some(json!({"email": "Jane.Doe@gmail.com"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "janedoe+shop@googlemail.com"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "jane.doe@work.test"}))).await;
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    // Older or integration-written rows can hold numbers and ids in other formats.
    for (phone, ext) in [("+15035550177", "CUST-1"), ("503-555-0177", "cust-1 ")] {
        sqlx::query("INSERT INTO profiles (id, account_id, phone_number, external_id) VALUES ($1, $2, $3, $4)").bind(Uuid::now_v7()).bind(acct).bind(phone).bind(ext).execute(&h.state.db).await.unwrap();
    }
    // An email on one profile and a phone number on another, sent together.
    h.ok("POST", "/api/profiles", Some(json!({"email": "pat@x.test"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550188"}))).await;
    let (s, _) = h.req("POST", "/api/profiles", Some(json!({"email": "pat@x.test", "phone_number": "+15035550188"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "the upsert behaves as before");
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "F", "config": {"collect": ["email", "phone_number"]}}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", form["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": form["id"], "email": "pat@x.test", "phone_number": "+15035550188"}).to_string()).await;

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    let d = h.ok("GET", "/api/people/duplicates", None).await;
    let groups = d["data"].as_array().unwrap();
    let find = |reason: &str| groups.iter().find(|g| g["reason"] == reason).unwrap_or_else(|| panic!("no {reason} group: {groups:?}"));
    assert_eq!(find("email")["profiles"].as_array().unwrap().len(), 2, "work address isn't the same inbox");
    assert_eq!(find("phone_number")["profiles"].as_array().unwrap().len(), 2);
    assert_eq!(find("external_id")["profiles"].as_array().unwrap().len(), 2);
    let linked = find("linked_identifiers");
    assert_eq!(linked["key"], json!({"email": "pat@x.test", "phone_number": "+15035550188"}));
    assert_eq!(linked["times_seen"], 2);
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(before, after, "finding duplicates merges nothing");
    h.cleanup().await;
}

#[tokio::test]
async fn bulk_actions_act_on_exactly_the_counted_set() {
    let mut h = harness!();
    h.signup().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIP"}))).await;
    let target = h.ok("POST", "/api/lists", Some(json!({"name": "Target"}))).await;
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    // 1,203 members (more than two batches), one of them unsubscribed.
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) SELECT gen_random_uuid(), $1, 'p' || i || '@x.test', CASE WHEN i = 1 THEN 'unsubscribed' ELSE 'never_subscribed' END FROM generate_series(1, 1203) i")
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO list_members (list_id, profile_id) SELECT $1::uuid, id FROM profiles WHERE account_id = $2").bind(list["id"].as_str().unwrap()).bind(acct).execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/profiles", Some(json!({"email": "outside@x.test"}))).await;

    let sel = json!({"list_id": list["id"]});
    let dry = h.ok("POST", "/api/people/bulk", Some(json!({"action": "set_property", "key": "tier", "value": "gold", "selection": sel}))).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["count"].as_i64()), (Some(true), Some(1203)));
    let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "subscribe", "selection": sel, "confirm": true, "expected_count": 1203}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "there's no bulk subscribe: {v}");
    let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "set_property", "key": "tier", "value": "gold", "selection": sel, "confirm": true, "expected_count": 1202}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "count changed: {v}");
    let started = h.ok("POST", "/api/people/bulk", Some(json!({"action": "set_property", "key": "tier", "value": "gold", "selection": sel, "confirm": true, "expected_count": 1203}))).await;
    assert_eq!(started["total"], 1203);
    // One at a time.
    let (s, _) = h.req("POST", "/api/people/bulk", Some(json!({"action": "export", "selection": sel, "confirm": true, "expected_count": 1203}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Someone joining the list after it started isn't part of it.
    let late = h.ok("POST", "/api/profiles", Some(json!({"email": "late@x.test", "list_ids": [list["id"]]}))).await;
    h.drain().await;
    let job = h.ok("GET", &format!("/api/people/bulk/{}", started["id"].as_str().unwrap()), None).await;
    assert_eq!((job["status"].as_str(), job["processed"].as_i64(), job["succeeded"].as_i64()), (Some("done"), Some(1203), Some(1203)), "{job}");
    let gold: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE properties->>'tier' = 'gold'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(gold, 1203);
    let late_tier: Value = sqlx::query_scalar("SELECT properties FROM profiles WHERE id = $1::uuid").bind(late["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert!(late_tier.get("tier").is_none());

    // Selected profiles: add to a list, then export.
    let some: Vec<String> = sqlx::query_scalar("SELECT id::text FROM profiles WHERE email IN ('p2@x.test', 'p3@x.test', 'outside@x.test')").fetch_all(&h.state.db).await.unwrap();
    h.ok("POST", "/api/people/bulk", Some(json!({"action": "add_to_list", "list_id": target["id"], "selection": {"profile_ids": some}, "confirm": true, "expected_count": 3}))).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/lists/{}", target["id"].as_str().unwrap()), None).await["member_count"], 3);
    let ex = h.ok("POST", "/api/people/bulk", Some(json!({"action": "export", "selection": {"q": "outside"}, "confirm": true, "expected_count": 1}))).await;
    h.drain().await;
    let ex = h.ok("GET", &format!("/api/people/bulk/{}", ex["id"].as_str().unwrap()), None).await;
    let (s, _, csv) = h.bytes("GET", ex["file_url"].as_str().unwrap(), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    let csv = String::from_utf8(csv).unwrap();
    assert_eq!(csv.lines().count(), 2, "{csv}");
    assert!(csv.contains("outside@x.test"));

    // Delete everyone on the list (exact count typed); the unsubscribed one stays suppressed if they come back.
    let n = h.ok("POST", "/api/people/bulk", Some(json!({"action": "delete", "selection": sel}))).await["count"].as_i64().unwrap();
    assert_eq!(n, 1204);
    h.ok("POST", "/api/people/bulk", Some(json!({"action": "delete", "selection": sel, "confirm": true, "expected_count": n}))).await;
    h.drain().await;
    let left: Vec<String> = sqlx::query_scalar("SELECT email FROM profiles ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(left, vec!["outside@x.test".to_string()]);
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\np1@x.test\np2@x.test\n", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let back: Vec<(String, String)> = sqlx::query_as("SELECT email, email_consent FROM profiles WHERE email IN ('p1@x.test', 'p2@x.test') ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(back, vec![("p1@x.test".into(), "unsubscribed".into()), ("p2@x.test".into(), "subscribed".into())]);

    // Another account's ids are never touched.
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let mine = h.ok("POST", "/api/profiles", Some(json!({"email": "mine@b.test"}))).await;
    let foreign: Vec<String> = sqlx::query_scalar("SELECT id::text FROM profiles WHERE email = 'outside@x.test'").fetch_all(&h.state.db).await.unwrap();
    let dry = h.ok("POST", "/api/people/bulk", Some(json!({"action": "delete", "selection": {"profile_ids": [foreign[0], mine["id"]]}}))).await;
    assert_eq!(dry["count"], 1, "only this account's profile counts");
    h.cleanup().await;
}

#[tokio::test]
async fn privacy_export_and_delete() {
    let mut h = harness!();
    h.signup().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "ann@x.test", "first_name": "<b>Ann</b>", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "bob@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let id = p["id"].as_str().unwrap().to_string();
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 42}))).await;
    h.ok("POST", &format!("/api/people/profiles/{id}/notes"), Some(json!({"body": "Prefers texts"}))).await;
    send_campaign(&mut h, &list["id"]).await;
    h.raw("POST", &unsub_path(&h, "ann@x.test"), &[], "").await;

    // A bulk export that includes Ann.
    let bulk = h.ok("POST", "/api/people/bulk", Some(json!({"action": "export", "selection": {"q": "ann@x.test"}, "confirm": true, "expected_count": 1}))).await;
    h.drain().await;
    let bulk_url = h.ok("GET", &format!("/api/people/bulk/{}", bulk["id"].as_str().unwrap()), None).await["file_url"].as_str().unwrap().to_string();
    let ex = h.ok("POST", "/api/people/privacy/export", Some(json!({"email": "ANN@x.test"}))).await;
    assert_eq!(ex["found"], 1);
    let files = ex["files"].as_array().unwrap();
    let (s, hd, body) = h.bytes("GET", files[0]["url"].as_str().unwrap(), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("cache-control").unwrap(), "no-store");
    let data: Value = serde_json::from_slice(&body).unwrap();
    let mine = &data["profiles"][0];
    assert_eq!(mine["profile"]["email"], "ann@x.test");
    assert_eq!(mine["events"].as_array().unwrap().iter().filter(|e| e["metric"] == "Placed Order").count(), 1);
    assert_eq!(mine["messages"][0]["subject"], "Spring sale");
    assert_eq!(mine["lists"][0]["name"], "News");
    assert_eq!(mine["internal_notes"][0]["note"], "Prefers texts");
    assert!(mine["consent_history"].as_array().unwrap().len() >= 2);
    assert!(!String::from_utf8_lossy(&body).contains("bob@x.test"), "no one else's data");
    let (_, _, html) = h.bytes("GET", files[1]["url"].as_str().unwrap(), "text/plain", vec![]).await;
    let html = String::from_utf8(html).unwrap();
    assert!(html.contains("&lt;b&gt;Ann") && !html.contains("<b>Ann"), "escaped");
    assert!(html.contains("Spring sale"));
    // Downloads need sign-in, and only this account's.
    let (s, _, _) = h.raw("GET", files[0]["url"].as_str().unwrap(), &[], "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Delete: dry run, then everything goes but a one-way code.
    let dry = h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "ann@x.test"}))).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["found"].as_i64()), (Some(true), Some(1)));
    let del = h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "ann@x.test", "confirm": true}))).await;
    assert_eq!(del["deleted"], 1);
    assert_eq!(del["export_files_deleted"], 3, "both privacy files and the bulk export that held them");
    let (s, _, _) = h.bytes("GET", &bulk_url, "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let q: Value = sqlx::query_scalar("SELECT selection FROM bulk_actions WHERE id = $1::uuid").bind(bulk["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert!(q.get("q").is_none(), "a saved search naming them is cleared: {q}");
    for t in PROFILE_TABLES {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {} WHERE {} = $1::uuid", t.table, t.column)).bind(&id).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(n, 0, "{}.{} still holds the deleted person", t.table, t.column);
    }
    let (s, _, _) = h.bytes("GET", files[0]["url"].as_str().unwrap(), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the earlier export went too");
    // Nothing identifying is left in the request log or the suppression record.
    let log = h.ok("GET", "/api/people/privacy/requests", None).await;
    assert_eq!(log["total"], 2);
    assert_eq!(log["data"][0]["kind"], "delete");
    assert_eq!(log["data"][0]["requested_by"], "owner@acme.test");
    for table in ["privacy_requests", "suppression_tombstones"] {
        let dump: String = sqlx::query_scalar(&format!("SELECT COALESCE(string_agg(t::text, ' '), '') FROM {table} t")).fetch_one(&h.state.db).await.unwrap();
        assert!(!dump.contains("ann"), "{table} keeps no personal data: {dump}");
    }

    // Re-imported later (even as Ann.Doe+x), they stay unsubscribed; a sign-up of their own would be their choice.
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nann+promo@x.test\nann@x.test\n", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let back: Vec<String> = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email LIKE 'ann%' ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(back, vec!["unsubscribed".to_string(), "unsubscribed".to_string()]);
    let how: Vec<String> = sqlx::query_scalar("SELECT method FROM consent_events e JOIN profiles p ON p.id = e.profile_id WHERE p.email = 'ann@x.test' ORDER BY e.occurred_at").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(how, vec!["import".to_string(), "deleted_profile".to_string()]);

    // An existing profile that takes on a deleted person's address gets their suppression too.
    let carl = h.ok("POST", "/api/profiles", Some(json!({"email": "carl@x.test", "email_consent": "subscribed"}))).await;
    sqlx::query("DELETE FROM profiles WHERE email LIKE 'ann%'").execute(&h.state.db).await.unwrap();
    h.ok("PATCH", &format!("/api/profiles/{}", carl["id"].as_str().unwrap()), Some(json!({"email": "ann@x.test"}))).await;
    let c: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1::uuid").bind(carl["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, "unsubscribed", "an address edit can't shed a deleted person's unsubscribe");

    // The plain profile delete keeps the record too.
    let bob = pid(&h, "bob@x.test").await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE id = $1::uuid").bind(&bob).execute(&h.state.db).await.unwrap();
    h.ok("DELETE", &format!("/api/profiles/{bob}"), None).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "bob@x.test", "email_consent": "subscribed"}))).await;
    let r: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE email = 'bob@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(r.as_deref(), Some("hard_bounce"));

    // Nobody found: logged, nothing exported.
    let none = h.ok("POST", "/api/people/privacy/export", Some(json!({"phone_number": "+15035550000"}))).await;
    assert_eq!(none["found"], 0);
    h.cleanup().await;
}

#[tokio::test]
async fn do_not_sell_notes_and_reachability() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Viewed Product", "profile": {"email": "gpc@x.test"}, "gpc": true}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    let id = pid(&h, "gpc@x.test").await;
    let sum = h.ok("GET", &format!("/api/people/profiles/{id}"), None).await;
    assert_eq!(sum["ads_opt_out"], true);
    assert_eq!(sum["consent_history"][0]["method"], "gpc");
    h.ok("PATCH", &format!("/api/people/profiles/{id}/privacy"), Some(json!({"ads_opt_out": false}))).await;
    let sum = h.ok("GET", &format!("/api/people/profiles/{id}"), None).await;
    assert_eq!(sum["ads_opt_out"], false);
    assert_eq!(sum["consent_history"][0]["method"], "dashboard");

    // Why they can't be reached.
    let r = &sum["reachability"];
    assert_eq!(r["email"]["can_send"], true, "{r}");
    assert_eq!(r["sms"]["reasons"][0]["code"], "no_address");
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    let r = h.ok("GET", &format!("/api/people/profiles/{id}"), None).await["reachability"].clone();
    assert_eq!(r["email"]["reasons"][0]["code"], "hard_bounce");

    // Notes.
    let (s, _) = h.req("POST", &format!("/api/people/profiles/{id}/notes"), Some(json!({"body": "  "}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let n = h.ok("POST", &format!("/api/people/profiles/{id}/notes"), Some(json!({"body": "VIP, call before shipping"}))).await;
    assert_eq!(n["author"], "owner@acme.test");
    h.ok("DELETE", &format!("/api/people/profiles/{id}/notes/{}", n["id"].as_str().unwrap()), None).await;
    assert_eq!(h.ok("GET", &format!("/api/people/profiles/{id}/notes"), None).await["data"].as_array().unwrap().len(), 0);
    // Not another account's.
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let (s, _) = h.req("GET", &format!("/api/people/profiles/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.req("POST", &format!("/api/people/profiles/{id}/notes"), Some(json!({"body": "x"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn mcp_tools_dry_run_until_confirmed() {
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "k"}))).await["key"].as_str().unwrap().to_string();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
    for t in ["get_consent_history", "list_suppressions", "suppress_profiles", "unsuppress_profile", "find_duplicate_profiles", "merge_profiles", "start_bulk_action", "get_bulk_action", "export_person_data", "delete_person_data"] {
        assert!(out.contains(&format!("\"name\":\"{t}\"")), "missing {t}");
    }
    let a = h.ok("POST", "/api/profiles", Some(json!({"email": "a@x.test", "email_consent": "subscribed"}))).await;
    let b = h.ok("POST", "/api/profiles", Some(json!({"email": "b@x.test"}))).await;

    let r = mcp(&h, &key, "suppress_profiles", json!({"addresses": ["a@x.test"]})).await;
    assert_eq!(r["dry_run"], true);
    let r = mcp(&h, &key, "merge_profiles", json!({"keep_id": a["id"], "merge_id": b["id"]})).await;
    assert_eq!(r["dry_run"], true);
    let r = mcp(&h, &key, "start_bulk_action", json!({"action": "delete", "selection": {"q": "x.test"}})).await;
    assert_eq!((r["dry_run"].as_bool(), r["count"].as_i64()), (Some(true), Some(2)));
    let r = mcp(&h, &key, "delete_person_data", json!({"email": "a@x.test"})).await;
    assert_eq!(r["dry_run"], true);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap(), 2, "dry runs change nothing");

    mcp(&h, &key, "suppress_profiles", json!({"addresses": ["a@x.test"], "confirm": true})).await;
    let l = mcp(&h, &key, "list_suppressions", json!({"reason": "manual"})).await;
    assert_eq!(l["total"], 1);
    let r = mcp(&h, &key, "unsuppress_profile", json!({"profile_id": a["id"], "channel": "email", "confirm": true})).await;
    assert_eq!(r["ok"], true);
    let src: (String, String) = sqlx::query_as("SELECT method, via FROM consent_events WHERE field = 'suppression' ORDER BY occurred_at LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(src, ("suppression_list".to_string(), "mcp".to_string()));
    let started = mcp(&h, &key, "start_bulk_action", json!({"action": "set_property", "key": "k", "value": 1, "selection": {"q": "x.test"}, "confirm": true, "expected_count": 2})).await;
    h.drain().await;
    let got = mcp(&h, &key, "get_bulk_action", json!({"id": started["id"]})).await;
    assert_eq!(got["status"], "done");
    let ex = mcp(&h, &key, "export_person_data", json!({"email": "a@x.test"})).await;
    assert_eq!(ex["data"]["profiles"][0]["profile"]["email"], "a@x.test");
    let del = mcp(&h, &key, "delete_person_data", json!({"email": "b@x.test", "confirm": true})).await;
    assert_eq!(del["deleted"], 1);
    let log = mcp(&h, &key, "list_privacy_requests", json!({})).await;
    assert_eq!(log["total"], 2);
    assert_eq!(log["requests"][0]["via"], "mcp");
    h.cleanup().await;
}

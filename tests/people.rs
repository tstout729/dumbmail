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

/// A lint, not proof: it finds raw SQL in `src` that writes a consent, suppression or opt-out column
/// outside the files that record history (an UPDATE ... SET col = ..., or an INSERT INTO profiles
/// naming one in its column list), in any case and spacing. It can't see SQL built at runtime from
/// column names; those paths need behaviour tests (see `consent_history_records_every_path`).
#[test]
fn consent_writes_go_through_the_history() {
    let allowed = ["src/audience.rs", "src/features/people/consent.rs", "src/features/people/merge.rs"];
    let cols = ["email_consent", "sms_consent", "email_suppressed_reason", "sms_suppressed_reason", "ads_opt_out", "marketing_hold"];
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
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
            if allowed.contains(&rel.as_str()) {
                continue;
            }
            let text = squash(&std::fs::read_to_string(&path).unwrap());
            for start in ["update profiles", "insert into profiles"] {
                for (i, _) in text.match_indices(start) {
                    // The statement up to the end of its string literal.
                    let stmt: String = text[i..].chars().take_while(|c| *c != '"').collect();
                    let writes = cols.iter().any(|c| {
                        if start == "update profiles" {
                            stmt.contains(&format!("{c} =")) || stmt.contains(&format!("{c}="))
                        } else {
                            stmt.split(')').next().is_some_and(|list| list.contains(c))
                        }
                    });
                    if writes {
                        offenders.push(format!("{rel}: {}", stmt.chars().take(100).collect::<String>()));
                    }
                }
            }
        }
    }
    assert!(offenders.is_empty(), "consent written without recording history (use features::people::consent):\n{}", offenders.join("\n"));
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

/// Preview, then confirm the saved set with the same action and settings.
async fn bulk(h: &mut Harness, body: Value) -> Value {
    let dry = h.ok("POST", "/api/people/bulk", Some(body.clone())).await;
    assert_eq!(dry["dry_run"], true, "{dry}");
    let mut confirm = body;
    confirm["confirm"] = json!(true);
    confirm["set_id"] = dry["set_id"].clone();
    h.ok("POST", "/api/people/bulk", Some(confirm)).await
}

async fn export_ready(h: &mut Harness, body: Value) -> Value {
    let started = h.ok("POST", "/api/people/privacy/export", Some(body)).await;
    assert_eq!(started["status"], "building", "{started}");
    assert!(started.get("data").is_none(), "nothing is returned inline");
    h.drain().await;
    let r = h.ok("GET", &format!("/api/people/privacy/requests/{}", started["request_id"].as_str().unwrap()), None).await;
    assert_eq!(r["status"], "ready", "{r}");
    r
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
    let set = json!({"action": "set_property", "key": "tier", "value": "gold", "selection": sel});
    let dry = h.ok("POST", "/api/people/bulk", Some(set.clone())).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["count"].as_i64()), (Some(true), Some(1203)));
    assert!(dry["set_id"].is_string() && dry["expires_at"].is_string(), "{dry}");
    let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "subscribe", "selection": sel}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "there's no bulk subscribe: {v}");
    // Confirming needs the set, and exactly the previewed action and settings.
    let (s, _) = h.req("POST", "/api/people/bulk", Some(json!({"action": "set_property", "key": "tier", "value": "gold", "selection": sel, "confirm": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "set_property", "key": "tier", "value": "platinum", "selection": sel, "confirm": true, "set_id": dry["set_id"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "another value than previewed: {v}");
    let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "delete", "selection": sel, "confirm": true, "set_id": dry["set_id"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "another action than previewed: {v}");
    // Reserved keys can't be set or cleared.
    for key in ["email_consent", "marketing_hold", "ads_opt_out", "$source", "email"] {
        let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "set_property", "key": key, "value": "x", "selection": sel}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{key}: {v}");
    }
    let mut confirm = set.clone();
    confirm["confirm"] = json!(true);
    confirm["set_id"] = dry["set_id"].clone();
    let started = h.ok("POST", "/api/people/bulk", Some(confirm.clone())).await;
    assert_eq!(started["total"], 1203);
    let (s, _) = h.req("POST", "/api/people/bulk", Some(confirm)).await;
    assert_eq!(s, StatusCode::CONFLICT, "a set runs once");
    // One at a time.
    let ex = h.ok("POST", "/api/people/bulk", Some(json!({"action": "export", "selection": sel}))).await;
    let (s, _) = h.req("POST", "/api/people/bulk", Some(json!({"action": "export", "selection": sel, "confirm": true, "set_id": ex["set_id"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Someone joining the list after the preview isn't part of it.
    let late = h.ok("POST", "/api/profiles", Some(json!({"email": "late@x.test", "list_ids": [list["id"]]}))).await;
    h.drain().await;
    let job = h.ok("GET", &format!("/api/people/bulk/{}", started["id"].as_str().unwrap()), None).await;
    assert_eq!((job["status"].as_str(), job["processed"].as_i64(), job["succeeded"].as_i64()), (Some("done"), Some(1203), Some(1203)), "{job}");
    let gold: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE properties->>'tier' = 'gold'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(gold, 1203);
    let late_tier: Value = sqlx::query_scalar("SELECT properties FROM profiles WHERE id = $1::uuid").bind(late["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert!(late_tier.get("tier").is_none());
    // An expired preview can't be confirmed.
    let old = h.ok("POST", "/api/people/bulk", Some(json!({"action": "clear_property", "key": "tier", "selection": sel}))).await;
    sqlx::query("UPDATE bulk_actions SET expires_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(old["set_id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", "/api/people/bulk", Some(json!({"action": "clear_property", "key": "tier", "selection": sel, "confirm": true, "set_id": old["set_id"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("expired"));

    // Selected profiles: add to a list, then export (the file is ready only once the job is done).
    let some: Vec<String> = sqlx::query_scalar("SELECT id::text FROM profiles WHERE email IN ('p2@x.test', 'p3@x.test', 'outside@x.test')").fetch_all(&h.state.db).await.unwrap();
    bulk(&mut h, json!({"action": "add_to_list", "list_id": target["id"], "selection": {"profile_ids": some}})).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/lists/{}", target["id"].as_str().unwrap()), None).await["member_count"], 3);
    let ex = bulk(&mut h, json!({"action": "export", "selection": {"q": "outside"}})).await;
    assert!(ex["file_url"].is_null(), "no download before it's done");
    h.drain().await;
    let ex = h.ok("GET", &format!("/api/people/bulk/{}", ex["id"].as_str().unwrap()), None).await;
    let (s, hd, csv) = h.bytes("GET", ex["file_url"].as_str().unwrap(), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(hd.get("content-disposition").unwrap().to_str().unwrap().contains("profiles-"));
    let csv = String::from_utf8(csv).unwrap();
    assert_eq!(csv.lines().count(), 2, "{csv}");
    assert!(csv.contains("outside@x.test"));

    // Delete everyone on the list; the unsubscribed one stays suppressed if they come back.
    let deleted = bulk(&mut h, json!({"action": "delete", "selection": sel})).await;
    assert_eq!(deleted["total"], 1204);
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
async fn bulk_confirm_acts_on_the_previewed_people_not_whoever_matches_now() {
    let mut h = harness!();
    h.signup().await;
    let a = h.ok("POST", "/api/profiles", Some(json!({"email": "a@x.test", "properties": {"plan": "trial"}}))).await;
    let b = h.ok("POST", "/api/profiles", Some(json!({"email": "b@x.test", "properties": {"plan": "paid"}}))).await;
    let c = h.ok("POST", "/api/profiles", Some(json!({"email": "c@x.test", "properties": {"plan": "trial"}}))).await;
    let d = h.ok("POST", "/api/profiles", Some(json!({"email": "d@x.test", "properties": {"plan": "paid"}}))).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Trials", "definition": {"match": "all", "conditions": [{"type": "property", "field": "plan", "op": "eq", "value": "trial"}]}}))).await;
    let body = json!({"action": "delete", "selection": {"segment_id": seg["id"]}});
    let dry = h.ok("POST", "/api/people/bulk", Some(body.clone())).await;
    assert_eq!(dry["count"], 2, "{dry}");
    // Between preview and confirm, A leaves the segment and B joins it: the count is unchanged.
    h.ok("PATCH", &format!("/api/profiles/{}", a["id"].as_str().unwrap()), Some(json!({"properties": {"plan": "paid"}}))).await;
    h.ok("PATCH", &format!("/api/profiles/{}", b["id"].as_str().unwrap()), Some(json!({"properties": {"plan": "trial"}}))).await;
    // And C is merged into D before the job runs.
    h.ok("POST", "/api/people/merge", Some(json!({"keep_id": d["id"], "merge_id": c["id"], "confirm": true}))).await;
    let mut confirm = body;
    confirm["confirm"] = json!(true);
    confirm["set_id"] = dry["set_id"].clone();
    let started = h.ok("POST", "/api/people/bulk", Some(confirm)).await;
    h.drain().await;
    let job = h.ok("GET", &format!("/api/people/bulk/{}", started["id"].as_str().unwrap()), None).await;
    assert_eq!((job["status"].as_str(), job["succeeded"].as_i64(), job["skipped"].as_i64()), (Some("done"), Some(1), Some(1)), "C was merged away: skipped. {job}");
    let left: Vec<String> = sqlx::query_scalar("SELECT email FROM profiles ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(left, vec!["b@x.test".to_string(), "d@x.test".to_string()], "A (previewed) is deleted; B (joined later) and D (kept in the merge) aren't");
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

    // A bulk export that includes Ann, and one that doesn't.
    let with_ann = bulk(&mut h, json!({"action": "export", "selection": {"q": "ann@x.test"}})).await;
    h.drain().await;
    let without = bulk(&mut h, json!({"action": "export", "selection": {"q": "bob@x.test"}})).await;
    h.drain().await;
    let file_of = |v: &Value| format!("/api/people/bulk/{}", v["id"].as_str().unwrap());
    let with_url = h.ok("GET", &file_of(&with_ann), None).await["file_url"].as_str().unwrap().to_string();
    let without_url = h.ok("GET", &file_of(&without), None).await["file_url"].as_str().unwrap().to_string();
    let ex = export_ready(&mut h, json!({"email": "ANN@x.test"})).await;
    assert_eq!(ex["profiles_found"], 1);
    let files = ex["files"].as_array().unwrap();
    let json_url = files.iter().find(|f| f["name"].as_str().unwrap().ends_with(".json")).unwrap()["url"].as_str().unwrap().to_string();
    let html_url = files.iter().find(|f| f["name"].as_str().unwrap().ends_with(".html")).unwrap()["url"].as_str().unwrap().to_string();
    let (s, hd, body) = h.bytes("GET", &json_url, "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("cache-control").unwrap(), "no-store");
    let data: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(data["complete"], true, "nothing was cut off");
    let mine = &data["profiles"][0];
    assert_eq!(mine["profile"]["email"], "ann@x.test");
    assert_eq!(mine["events"].as_array().unwrap().iter().filter(|e| e["metric"] == "Placed Order").count(), 1);
    assert_eq!(mine["messages"][0]["subject"], "Spring sale");
    assert_eq!(mine["lists"][0]["name"], "News");
    assert_eq!(mine["internal_notes"][0]["note"], "Prefers texts");
    assert!(mine["consent_history"].as_array().unwrap().len() >= 2);
    assert!(!String::from_utf8_lossy(&body).contains("bob@x.test"), "no one else's data");
    let (_, _, html) = h.bytes("GET", &html_url, "text/plain", vec![]).await;
    let html = String::from_utf8(html).unwrap();
    assert!(html.contains("&lt;b&gt;Ann") && !html.contains("<b>Ann"), "escaped");
    assert!(html.contains("Spring sale"));
    // Downloads need sign-in, and only this account's.
    let (s, _, _) = h.raw("GET", &json_url, &[], "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Delete: dry run, then everything goes but a one-way code.
    let dry = h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "ann@x.test"}))).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["found"].as_i64()), (Some(true), Some(1)));
    let del = h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "ann@x.test", "confirm": true}))).await;
    assert_eq!(del["deleted"], 1);
    assert_eq!(del["export_files_deleted"], 4, "both personal files and every stored bulk export: {del}");
    for url in [&with_url, &without_url, &json_url, &html_url] {
        let (s, _, _) = h.bytes("GET", url, "text/plain", vec![]).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{url} is gone");
    }
    let q: Value = sqlx::query_scalar("SELECT selection FROM bulk_actions WHERE id = $1::uuid").bind(with_ann["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert!(q.get("q").is_none(), "a saved search naming them is cleared: {q}");
    for t in PROFILE_TABLES {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {} WHERE {} = $1::uuid", t.table, t.column)).bind(&id).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(n, 0, "{}.{} still holds the deleted person", t.table, t.column);
    }
    // Nothing identifying is left in the request log or the suppression record.
    let log = h.ok("GET", "/api/people/privacy/requests", None).await;
    assert_eq!(log["total"], 2);
    assert_eq!(log["data"][0]["kind"], "delete");
    assert_eq!(log["data"][0]["requested_by"], "owner@acme.test");
    assert_eq!(log["data"][1]["files"].as_array().unwrap().len(), 0, "the export's links are gone");
    for table in ["privacy_requests", "suppression_tombstones", "jobs"] {
        let dump: String = sqlx::query_scalar(&format!("SELECT COALESCE(string_agg(t::text, ' '), '') FROM {table} t")).fetch_one(&h.state.db).await.unwrap();
        assert!(!dump.contains("ann"), "{table} keeps no personal data: {dump}");
    }

    // Re-imported later (even as ann+promo), they stay unsubscribed.
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
    assert!(r["set_id"].is_string(), "the dry run returns the saved set: {r}");
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
    let dry = mcp(&h, &key, "start_bulk_action", json!({"action": "set_property", "key": "k", "value": 1, "selection": {"q": "x.test"}})).await;
    let started = mcp(&h, &key, "start_bulk_action", json!({"action": "set_property", "key": "k", "value": 1, "selection": {"q": "x.test"}, "confirm": true, "set_id": dry["set_id"]})).await;
    h.drain().await;
    let got = mcp(&h, &key, "get_bulk_action", json!({"id": started["id"]})).await;
    assert_eq!(got["status"], "done");
    let ex = mcp(&h, &key, "export_person_data", json!({"email": "a@x.test"})).await;
    assert_eq!(ex["status"], "building");
    assert!(ex.get("data").is_none(), "no personal data inline: {ex}");
    h.drain().await;
    let req = mcp(&h, &key, "list_privacy_requests", json!({"id": ex["request_id"]})).await;
    assert_eq!(req["status"], "ready");
    assert_eq!(req["files"].as_array().unwrap().len(), 2);
    let del = mcp(&h, &key, "delete_person_data", json!({"email": "b@x.test", "confirm": true})).await;
    assert_eq!(del["deleted"], 1);
    let log = mcp(&h, &key, "list_privacy_requests", json!({})).await;
    assert_eq!(log["total"], 2);
    assert_eq!(log["requests"][0]["via"], "mcp");
    h.cleanup().await;
}

#[tokio::test]
async fn erasing_someone_waiting_in_a_suppress_job_keeps_them_suppressed() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "late@x.test", "email_consent": "subscribed"}))).await;
    let mut many: Vec<String> = (0..300).map(|i| format!("s{i}@x.test")).collect();
    many.push("late@x.test".into());
    let r = h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": many, "confirm": true}))).await;
    assert_eq!(r["bulk_action"]["status"], "queued");
    // Erased before the job reaches them.
    h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "late@x.test", "confirm": true}))).await;
    h.drain().await;
    assert!(!sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM profiles WHERE email = 'late@x.test')").fetch_one(&h.state.db).await.unwrap(), "the job didn't bring them back");
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nlate@x.test\n", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let r: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE email = 'late@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(r.as_deref(), Some("manual"), "the suppression they were waiting for still applies");
    h.cleanup().await;
}

#[tokio::test]
async fn erasing_someone_clears_every_stored_export_and_finds_their_files_after_a_merge() {
    let mut h = harness!();
    h.signup().await;
    let a = h.ok("POST", "/api/profiles", Some(json!({"email": "dana@x.test", "phone_number": "+15035550160"}))).await;
    let b = h.ok("POST", "/api/profiles", Some(json!({"external_id": "D-1"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "other@x.test"}))).await;
    // Exported by phone from the first profile, which is then merged into the second.
    let ex = export_ready(&mut h, json!({"phone_number": "+15035550160"})).await;
    let personal: Vec<String> = ex["files"].as_array().unwrap().iter().map(|f| f["url"].as_str().unwrap().to_string()).collect();
    let other = bulk(&mut h, json!({"action": "export", "selection": {"q": "other"}})).await;
    h.drain().await;
    let other_url = h.ok("GET", &format!("/api/people/bulk/{}", other["id"].as_str().unwrap()), None).await["file_url"].as_str().unwrap().to_string();
    // A reports CSV export of the account.
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let report = Uuid::now_v7();
    sqlx::query("INSERT INTO exports (id, account_id, kind, status, filename) VALUES ($1, $2, 'profiles', 'done', 'profiles.csv')").bind(report).bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO export_chunks (export_id, seq, bytes) VALUES ($1, 0, 'id,email\n'::bytea)").bind(report).execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/people/merge", Some(json!({"keep_id": b["id"], "merge_id": a["id"], "confirm": true}))).await;

    // Erased by email now (the export was asked for by phone, on the other profile).
    let del = h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "dana@x.test", "confirm": true}))).await;
    assert_eq!(del["deleted"], 1, "{del}");
    for url in personal.iter().chain([&other_url]) {
        let (s, _, _) = h.bytes("GET", url, "text/plain", vec![]).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{url} is gone");
    }
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM people_files").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 0);
    let reports: i64 = sqlx::query_scalar("SELECT count(*) FROM exports").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(reports, 0, "report exports may hold them too");
    h.cleanup().await;
}

#[tokio::test]
async fn an_export_finishing_after_an_erasure_publishes_nothing() {
    let mut h = harness!();
    h.signup().await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "eve@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "eve@x.test"}}))).await;
    h.drain().await;
    let started = h.ok("POST", "/api/people/privacy/export", Some(json!({"email": "eve@x.test"}))).await;
    let request: Uuid = started["request_id"].as_str().unwrap().parse().unwrap();
    let payload: Value = sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'people_privacy_export'").fetch_one(&h.state.db).await.unwrap();
    // The erasure holds her row while the export job builds; the job can only publish after it.
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let mut tx = h.state.db.begin().await.unwrap();
    dumbmail::features::people::privacy::erase_in(&mut tx, acct, &[p["id"].as_str().unwrap().parse().unwrap()]).await.unwrap();
    let state = h.state.clone();
    let job = tokio::spawn(async move { dumbmail::features::people::privacy::run_export_job(&state, &payload).await });
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    tx.commit().await.unwrap();
    job.await.unwrap().unwrap();
    let r = h.ok("GET", &format!("/api/people/privacy/requests/{request}"), None).await;
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["files"].as_array().unwrap().len(), 0);
    let files: i64 = sqlx::query_scalar("SELECT count(*) FROM people_files").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(files, 0, "no copy of her data is left behind");

    // A bulk export running when someone is erased throws its file away too.
    let acct_id = acct;
    sqlx::query("INSERT INTO profiles (id, account_id, email) SELECT gen_random_uuid(), $1, 'b' || i || '@x.test' FROM generate_series(1, 700) i").bind(acct_id).execute(&h.state.db).await.unwrap();
    let ex = bulk(&mut h, json!({"action": "export", "selection": {"q": "@x.test"}})).await;
    assert!(dumbmail::jobs::run_one(&h.state).await.unwrap(), "first batch");
    h.ok("POST", "/api/people/privacy/delete", Some(json!({"email": "b7@x.test", "confirm": true}))).await;
    h.drain().await;
    let job = h.ok("GET", &format!("/api/people/bulk/{}", ex["id"].as_str().unwrap()), None).await;
    assert_eq!(job["status"], "failed", "{job}");
    assert!(job["error"].as_str().unwrap().contains("erased"), "{job}");
    assert!(job["file_url"].is_null());
    let files: i64 = sqlx::query_scalar("SELECT count(*) FROM people_files").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(files, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn a_failed_history_write_never_loses_an_opt_out() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.credit(10.0).await;
    let our_number = h.claim_number().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    for (e, phone) in [("u@x.test", "+15035550201"), ("b@x.test", "+15035550202")] {
        h.ok("POST", "/api/profiles", Some(json!({"email": e, "phone_number": phone, "email_consent": "subscribed", "sms_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    }
    send_campaign(&mut h, &list["id"]).await;
    // A double opt-in sign-up waiting to be confirmed.
    let dlist = h.ok("POST", "/api/lists", Some(json!({"name": "Double", "double_opt_in": true}))).await;
    let dform = h.ok("POST", "/api/forms", Some(json!({"name": "D", "list_id": dlist["id"], "config": {"headline": "Join"}}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", dform["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": dform["id"], "email": "doi@x.test"}).to_string()).await;
    let link = find_link(&h.state.email.sent().into_iter().find(|m| m.to == "doi@x.test").unwrap().html, "https://pl.test/c/").trim_start_matches("https://pl.test").to_string();
    h.drain().await;

    // Consent history can't be written for a while.
    sqlx::query("CREATE FUNCTION fail_history() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'history is down'; END $$").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_history BEFORE INSERT ON consent_events FOR EACH ROW EXECUTE FUNCTION fail_history()").execute(&h.state.db).await.unwrap();

    // An unsubscribe link, a STOP and a hard bounce are all saved, and acknowledged.
    let (s, _, page) = h.raw("POST", &unsub_path(&h, "u@x.test"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("You're unsubscribed"));
    let text = json!({"data": {"event_type": "message.received", "payload": {"text": "STOP", "from": {"phone_number": "+15035550201"}, "to": [{"phone_number": our_number}]}}}).to_string();
    let (s, _, _) = h.raw("POST", "/webhooks/telnyx/hooksecret", &[("content-type", "application/json")], &text).await;
    assert_eq!(s, StatusCode::OK);
    ses(&h, "b@x.test", "Permanent").await;
    let state: Vec<(String, String, String, Option<String>)> = sqlx::query_as("SELECT email, email_consent, sms_consent, email_suppressed_reason FROM profiles WHERE email IN ('u@x.test', 'b@x.test') ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(state, vec![
        ("b@x.test".into(), "subscribed".into(), "subscribed".into(), Some("hard_bounce".into())),
        ("u@x.test".into(), "unsubscribed".into(), "unsubscribed".into(), None),
    ]);
    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'people_consent_history'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(waiting, 3, "each one's history row waits to be written");

    // Changes that make someone more reachable fail as a whole instead, and can be retried.
    let u = pid(&h, "u@x.test").await;
    let (s, _) = h.req("PATCH", &format!("/api/profiles/{u}"), Some(json!({"sms_consent": "subscribed"}))).await;
    assert!(s.is_server_error(), "{s}");
    let sms: String = sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE email = 'u@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sms, "unsubscribed");
    // The confirmation link isn't used up by a confirmation that couldn't be saved.
    let (_, _, page) = h.raw("POST", &link, &[], "").await;
    assert!(page.contains("Something went wrong"), "{page}");
    let doi: (String, bool) = sqlx::query_as("SELECT p.email_consent, o.confirmed_at IS NOT NULL FROM profiles p JOIN optin_confirmations o ON o.profile_id = p.id WHERE p.email = 'doi@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(doi, ("pending".to_string(), false));

    // History is back: the waiting rows are written, once, and say they were late.
    sqlx::query("DROP TRIGGER fail_history ON consent_events").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    h.fast_forward().await;
    let late: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT method, channel || ':' || field, new_value, recorded_at >= occurred_at FROM consent_events WHERE detail->>'recorded_late' = 'true' ORDER BY method",
    )
    .fetch_all(&h.state.db)
    .await
    .unwrap();
    assert_eq!(late, vec![
        ("hard_bounce".into(), "email:suppression".into(), Some("hard_bounce".into()), true),
        ("sms_keyword".into(), "sms:consent".into(), Some("unsubscribed".into()), true),
        ("unsubscribe_link".into(), "email:consent".into(), Some("unsubscribed".into()), true),
    ]);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = 'people_consent_history'").fetch_one(&h.state.db).await.unwrap(), 0);
    let (_, _, page) = h.raw("POST", &link, &[], "").await;
    assert!(page.contains("You're subscribed"), "{page}");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'doi@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "subscribed");

    // If the unsubscribe itself can't be saved, the person is told so and can try again.
    sqlx::query("CREATE FUNCTION fail_unsub() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.email_consent = 'unsubscribed' AND OLD.email_consent <> 'unsubscribed' THEN RAISE EXCEPTION 'down'; END IF; RETURN NEW; END $$").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_unsub BEFORE UPDATE ON profiles FOR EACH ROW EXECUTE FUNCTION fail_unsub()").execute(&h.state.db).await.unwrap();
    let path = unsub_path(&h, "b@x.test");
    let (s, _, page) = h.raw("POST", &path, &[], "").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert!(page.contains("try again") && !page.contains("You're unsubscribed"), "{page}");
    let stamped: bool = sqlx::query_scalar("SELECT unsubscribed_at IS NOT NULL FROM messages WHERE to_address = 'b@x.test' AND channel = 'email'").fetch_one(&h.state.db).await.unwrap();
    assert!(!stamped);
    sqlx::query("DROP TRIGGER fail_unsub ON profiles").execute(&h.state.db).await.unwrap();
    let (s, _, page) = h.raw("POST", &path, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("You're unsubscribed"));
    h.cleanup().await;
}

#[tokio::test]
async fn a_double_opt_in_profile_is_created_waiting_to_confirm() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let dlist = h.ok("POST", "/api/lists", Some(json!({"name": "Double", "double_opt_in": true}))).await;
    let dform = h.ok("POST", "/api/forms", Some(json!({"name": "D", "list_id": dlist["id"], "config": {"headline": "Join"}}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", dform["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    // Any later write that sets a profile to pending fails: so the sign-up only works if the profile
    // is created pending.
    sqlx::query("CREATE FUNCTION no_late_pending() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.email_consent = 'pending' AND OLD.email_consent <> 'pending' THEN RAISE EXCEPTION 'late'; END IF; RETURN NEW; END $$").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE TRIGGER no_late_pending BEFORE UPDATE ON profiles FOR EACH ROW EXECUTE FUNCTION no_late_pending()").execute(&h.state.db).await.unwrap();
    let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": dform["id"], "email": "new@x.test"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let c: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'new@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, "pending");
    h.cleanup().await;
}

#[tokio::test]
async fn concurrent_consent_changes_keep_an_exact_history() {
    use dumbmail::features::people::{consent, Channel, ConsentSource};
    let mut h = harness!();
    h.signup().await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "race@x.test", "phone_number": "+15035550300"}))).await;
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let id: Uuid = p["id"].as_str().unwrap().parse().unwrap();
    let mut tasks = Vec::new();
    for i in 0..24 {
        let db = h.state.db.clone();
        tasks.push(tokio::spawn(async move {
            let v = if i % 2 == 0 { "subscribed" } else { "unsubscribed" };
            if i % 3 == 0 {
                // Through the shared upsert, matched by email.
                let input = dumbmail::audience::ProfileInput { email: Some("race@x.test".into()), email_consent: Some(v.into()), ..Default::default() };
                dumbmail::audience::upsert_profile(&db, acct, &input).await.map(|_| ())
            } else {
                consent::set_consent(&db, acct, id, Channel::Email, v, None, &ConsentSource::new("api", "api")).await.map(|_| ())
            }
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as("SELECT old_value, new_value FROM consent_events WHERE profile_id = $1 AND channel = 'email' AND field = 'consent' ORDER BY id")
        .bind(id)
        .fetch_all(&h.state.db)
        .await
        .unwrap();
    assert!(rows.len() > 2, "{rows:?}");
    let mut prev = "never_subscribed".to_string();
    for (old, new) in &rows {
        assert_eq!(old.as_deref(), Some(prev.as_str()), "each change starts where the last ended: {rows:?}");
        assert_ne!(old, new);
        prev = new.clone().unwrap();
    }
    let now: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1").bind(id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(now, prev, "the last row is the current state");
    h.cleanup().await;
}

#[tokio::test]
async fn retained_blocks_survive_every_reimport_until_a_verified_opt_in() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "gone@x.test", "email_consent": "unsubscribed"}))).await;
    h.ok("DELETE", &format!("/api/profiles/{}", p["id"].as_str().unwrap()), None).await;

    // Imported as subscribed, twice; then the API, then a single opt-in form, all say subscribed.
    for _ in 0..2 {
        h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\ngone@x.test\n", "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
        let c: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'gone@x.test'").fetch_one(&h.state.db).await.unwrap();
        assert_eq!(c, "unsubscribed", "a re-import can't undo the unsubscribe");
    }
    h.ok("POST", "/api/profiles", Some(json!({"email": "gone@x.test", "email_consent": "subscribed"}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list["id"]}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", form["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": form["id"], "email": "gone@x.test"}).to_string()).await;
    let c: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'gone@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, "unsubscribed", "a form alone doesn't prove the address is theirs");
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}}))).await;
    assert_eq!(est["count"], 0);

    // Their own confirmation, clicked from that inbox, is what brings them back; later imports keep it.
    let dlist = h.ok("POST", "/api/lists", Some(json!({"name": "Double", "double_opt_in": true}))).await;
    let dform = h.ok("POST", "/api/forms", Some(json!({"name": "D", "list_id": dlist["id"]}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", dform["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": dform["id"], "email": "gone@x.test"}).to_string()).await;
    let link = find_link(&h.state.email.sent().into_iter().rev().find(|m| m.to == "gone@x.test").unwrap().html, "https://pl.test/c/");
    h.raw("POST", link.trim_start_matches("https://pl.test"), &[], "").await;
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\ngone@x.test\n", "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let c: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'gone@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, "subscribed");

    // The stronger block wins and keeps its reason: a manually suppressed profile moved onto a deleted
    // hard-bounced address is suppressed for the bounce, which can't be lifted.
    let bounced = h.ok("POST", "/api/profiles", Some(json!({"email": "bounced@x.test"}))).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE id = $1::uuid").bind(bounced["id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();
    h.ok("DELETE", &format!("/api/profiles/{}", bounced["id"].as_str().unwrap()), None).await;
    let m = h.ok("POST", "/api/profiles", Some(json!({"email": "manual@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": ["manual@x.test"], "confirm": true}))).await;
    h.ok("PATCH", &format!("/api/profiles/{}", m["id"].as_str().unwrap()), Some(json!({"email": "bounced@x.test"}))).await;
    let r: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE id = $1::uuid").bind(m["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(r.as_deref(), Some("hard_bounce"));
    let (s, _) = h.req("POST", "/api/people/suppressions/unsuppress", Some(json!({"profile_id": m["id"], "channel": "email", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // A lifted manual suppression stays lifted (its retained record goes with it).
    let x = h.ok("POST", "/api/profiles", Some(json!({"email": "manual2@x.test"}))).await;
    h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": ["manual2@x.test"], "confirm": true}))).await;
    h.ok("DELETE", &format!("/api/profiles/{}", x["id"].as_str().unwrap()), None).await;
    let again = h.ok("POST", "/api/profiles", Some(json!({"email": "manual2@x.test"}))).await;
    h.ok("POST", "/api/people/suppressions/unsuppress", Some(json!({"profile_id": again["id"], "channel": "email", "confirm": true}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "manual2@x.test", "first_name": "M"}))).await;
    let r: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE email = 'manual2@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(r, None);
    h.cleanup().await;
}

#[tokio::test]
async fn merging_profiles_with_queued_messages_sends_each_campaign_once() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let keep = h.ok("POST", "/api/profiles", Some(json!({"email": "keep@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let other = h.ok("POST", "/api/profiles", Some(json!({"email": "old@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    // An earlier email to the merged-away address, and a confirmation waiting at that address.
    send_campaign(&mut h, &list["id"]).await;
    let old_unsub = unsub_path(&h, "old@x.test");
    sqlx::query("UPDATE profiles SET email_consent = 'never_subscribed'").execute(&h.state.db).await.unwrap();
    let dlist = h.ok("POST", "/api/lists", Some(json!({"name": "Double", "double_opt_in": true}))).await;
    let dform = h.ok("POST", "/api/forms", Some(json!({"name": "D", "list_id": dlist["id"]}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", dform["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": dform["id"], "email": "old@x.test"}).to_string()).await;
    let link = find_link(&h.state.email.sent().into_iter().rev().find(|m| m.to == "old@x.test").unwrap().html, "https://pl.test/c/");
    h.drain().await;

    // A second campaign is queued for both, but not sent yet.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"subject": "Second", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert!(dumbmail::jobs::run_one(&h.state).await.unwrap());
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND status = 'queued'").bind(c["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(queued, 2);

    // While a message to one of them is being sent, the merge waits.
    sqlx::query("UPDATE messages SET status = 'sending' WHERE campaign_id = $1::uuid AND to_address = 'old@x.test'").bind(c["id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", "/api/people/merge", Some(json!({"keep_id": keep["id"], "merge_id": other["id"], "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    sqlx::query("UPDATE messages SET status = 'queued' WHERE campaign_id = $1::uuid AND to_address = 'old@x.test'").bind(c["id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();

    h.ok("POST", "/api/people/merge", Some(json!({"keep_id": keep["id"], "merge_id": other["id"], "confirm": true}))).await;
    h.drain().await;
    let second: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject == "Second").map(|m| m.to).collect();
    assert_eq!(second, vec!["keep@x.test".to_string()], "one email, to the kept address");
    let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE subject = 'Second' AND status = 'sent'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sent, 1, "and one billed send");
    let why: Vec<Option<String>> = sqlx::query_scalar("SELECT error FROM messages WHERE subject IS NULL AND status = 'skipped'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(why, vec![Some("merged into another profile".to_string())], "the duplicate was cancelled by the merge, not left for a worker");

    // The confirmation sent to old@ can't subscribe keep@.
    let (_, _, page) = h.raw("POST", link.trim_start_matches("https://pl.test"), &[], "").await;
    assert!(page.contains("expired"), "{page}");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1::uuid").bind(keep["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed");
    // The unsubscribe link in the old email unsubscribes the kept profile (the safe direction).
    let (s, _, _) = h.raw("POST", &old_unsub, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE id = $1::uuid").bind(keep["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "unsubscribed");
    h.cleanup().await;
}

#[tokio::test]
async fn a_queued_message_isnt_sent_to_an_address_the_profile_no_longer_has() {
    let mut h = harness!();
    h.signup().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "before@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    h.drain().await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"subject": "Queued", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    assert!(dumbmail::jobs::run_one(&h.state).await.unwrap());
    h.ok("PATCH", &format!("/api/profiles/{}", p["id"].as_str().unwrap()), Some(json!({"email": "after@x.test"}))).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().all(|m| m.subject != "Queued"), "nothing sent");
    let why: Option<String> = sqlx::query_scalar("SELECT error FROM messages WHERE subject IS NULL OR subject = 'Queued'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(why.as_deref(), Some("address changed"));
    h.cleanup().await;
}

#[tokio::test]
async fn a_manually_suppressed_number_gets_no_texts_not_even_tests() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    h.claim_number().await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550400", "sms_consent": "subscribed"}))).await;
    h.ok("POST", "/api/people/suppressions", Some(json!({"addresses": ["+15035550400"], "confirm": true}))).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texts"}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"body": "Hello"}}))).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{}/test", c["id"].as_str().unwrap()), Some(json!({"to": ["+15035550400"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("suppressed"), "{v}");
    assert!(h.state.sms.sent().is_empty(), "no text went out");
    h.cleanup().await;
}

#[tokio::test]
async fn gpc_and_reserved_property_keys() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup"}))).await;
    h.ok("PATCH", &format!("/api/forms/{}", form["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    // From the snippet's body flag, and from the browser's Sec-GPC header on a plain form post.
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": form["id"], "email": "g1@x.test", "gpc": true}).to_string()).await;
    let body = format!("form_id={}&email=g2%40x.test", form["id"].as_str().unwrap());
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/x-www-form-urlencoded"), ("sec-gpc", "1")], &body).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": form["id"], "email": "g3@x.test"}).to_string()).await;
    let flags: Vec<(String, bool)> = sqlx::query_as("SELECT email, ads_opt_out FROM profiles ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(flags, vec![("g1@x.test".into(), true), ("g2@x.test".into(), true), ("g3@x.test".into(), false)]);
    let how: String = sqlx::query_scalar("SELECT method FROM consent_events WHERE channel = 'ads' LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(how, "gpc");

    // Custom properties can't shadow consent, privacy or identity fields.
    for props in [json!({"email_consent": "subscribed"}), json!({"marketing_hold": false}), json!({"$source": "x"})] {
        let (s, v) = h.req("POST", "/api/profiles", Some(json!({"email": "k@x.test", "properties": props}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }
    let (s, _) = h.req("POST", "/api/profiles", Some(json!({"email": "k@x.test", "ads_opt_out": false}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let imp = h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email,ads_opt_out,city\nk@x.test,false,Portland\n", "permission_confirmed": true}))).await;
    assert_eq!(imp["created"], 1, "{imp}");
    let props: Value = sqlx::query_scalar("SELECT properties FROM profiles WHERE email = 'k@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(props, json!({"city": "Portland"}), "the reserved column is left out");
    h.cleanup().await;
}


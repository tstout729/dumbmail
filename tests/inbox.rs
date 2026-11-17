//! Two-way texting: inbound storage on both providers, reply rules, keywords, automatic replies,
//! the reply flow step, account isolation and the MCP tools.
#[macro_use]
mod common;
use common::*;

use axum::http::StatusCode;
use serde_json::{json, Value};

const HOOK: &str = "hooksecret";

/// A Telnyx `message.received` webhook as documented (data.payload: id, from, to[], text, media[]).
async fn telnyx(h: &Harness, id: &str, from: &str, to: &str, text: &str, media: Value) {
    let body = json!({"data": {"event_type": "message.received", "id": format!("evt-{id}"), "record_type": "event", "payload": {
        "id": id, "direction": "inbound", "type": if media.as_array().is_some_and(|m| !m.is_empty()) { "MMS" } else { "SMS" },
        "text": text, "from": {"phone_number": from, "carrier": "T-Mobile", "line_type": "Wireless"},
        "to": [{"phone_number": to, "status": "webhook_delivered"}], "media": media, "record_type": "message"
    }}, "meta": {"attempt": 1}});
    let (s, _, out) = h.raw("POST", &format!("/webhooks/telnyx/{HOOK}"), &[("content-type", "application/json")], &body.to_string()).await;
    assert_eq!(s, StatusCode::OK, "{out}");
}

/// A Twilio incoming-message webhook: form-encoded MessageSid, From, To, Body, NumMedia, MediaUrlN.
async fn twilio(h: &Harness, sid: &str, from: &str, to: &str, text: &str, media: &[(&str, &str)]) {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("MessageSid", sid).append_pair("SmsSid", sid).append_pair("AccountSid", "AC123").append_pair("From", from).append_pair("To", to).append_pair("Body", text);
    form.append_pair("NumMedia", &media.len().to_string()).append_pair("NumSegments", "1");
    for (i, (url, ct)) in media.iter().enumerate() {
        form.append_pair(&format!("MediaUrl{i}"), url).append_pair(&format!("MediaContentType{i}"), ct);
    }
    let (s, headers, out) = h.raw("POST", &format!("/webhooks/twilio/{HOOK}"), &[("content-type", "application/x-www-form-urlencoded")], &form.finish()).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(headers.get("content-type").unwrap(), "text/xml");
}

async fn setup(h: &mut Harness) -> String {
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await
}

async fn conversation_for(h: &mut Harness, phone: &str) -> Value {
    let list = h.ok("GET", "/api/inbox/conversations?status=all", None).await;
    list["data"].as_array().unwrap().iter().find(|c| c["profile"]["phone_number"] == phone).cloned().unwrap_or_else(|| panic!("no conversation for {phone}: {list}"))
}

async fn events(h: &Harness, metric: &str, phone: &str) -> Vec<Value> {
    sqlx::query_scalar("SELECT e.properties FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = $1 AND p.phone_number = $2 ORDER BY e.created_at")
        .bind(metric)
        .bind(phone)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

fn texts_to(h: &Harness, phone: &str) -> Vec<String> {
    h.state.sms.sent().into_iter().filter(|s| s.to == phone).map(|s| s.body).collect()
}

async fn consent(h: &Harness, phone: &str) -> String {
    sqlx::query_scalar("SELECT sms_consent FROM profiles WHERE phone_number = $1").bind(phone).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn inbound_texts_are_stored_on_both_providers_and_deduped() {
    let mut h = harness!();
    let ours = setup(&mut h).await;

    // A number we've never seen: a profile is created, SMS consent untouched.
    telnyx(&h, "tx-1", "+15035550111", &ours, "Do you ship to Canada?", json!([])).await;
    // The provider retries the same webhook: stored once.
    telnyx(&h, "tx-1", "+15035550111", &ours, "Do you ship to Canada?", json!([])).await;
    let c = conversation_for(&mut h, "+15035550111").await;
    assert_eq!(c["unread_count"], 1, "{c}");
    assert_eq!(c["last_preview"], "Do you ship to Canada?");
    assert_eq!(c["profile"]["sms_consent"], "never_subscribed");
    assert_eq!(c["can_reply"], true);
    let ev = events(&h, "Received SMS Reply", "+15035550111").await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["text"], "Do you ship to Canada?");

    // Twilio MMS: the media links are kept. Copying refuses private addresses (the SSRF guard), so
    // these stay as links; nothing breaks.
    twilio(&h, "SM100", "+15035550122", &ours, "", &[("http://127.0.0.1:9/cat.jpg", "image/jpeg"), ("http://169.254.169.254/latest", "image/png")]).await;
    twilio(&h, "SM100", "+15035550122", &ours, "", &[("http://127.0.0.1:9/cat.jpg", "image/jpeg")]).await;
    h.drain().await;
    let c = conversation_for(&mut h, "+15035550122").await;
    assert_eq!(c["last_preview"], "Sent 2 attachments");
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{}", c["id"].as_str().unwrap()), None).await;
    let msgs = thread["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{thread}");
    assert_eq!(msgs[0]["direction"], "inbound");
    assert_eq!(msgs[0]["media"][0]["url"], "http://127.0.0.1:9/cat.jpg");
    assert_eq!(msgs[0]["media"][0]["stored"], false);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_media").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0);

    // Unread counts, read, close and reopen-on-new-text.
    assert_eq!(h.ok("GET", "/api/inbox/unread", None).await["conversations"], 2);
    let id = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/inbox/conversations/{id}/read"), Some(json!({}))).await;
    h.ok("POST", &format!("/api/inbox/conversations/{id}/status"), Some(json!({"status": "closed"}))).await;
    assert_eq!(h.ok("GET", "/api/inbox/conversations", None).await["total"], 1);
    assert_eq!(h.ok("GET", "/api/inbox/conversations?status=closed", None).await["total"], 1);
    twilio(&h, "SM101", "+15035550122", &ours, "hello again", &[]).await;
    assert_eq!(h.ok("GET", "/api/inbox/conversations", None).await["total"], 2);
    // Search by words in their texts and by phone digits.
    assert_eq!(h.ok("GET", "/api/inbox/conversations?q=canada", None).await["total"], 1);
    assert_eq!(h.ok("GET", "/api/inbox/conversations?q=555-0122", None).await["total"], 1);
    assert_eq!(h.ok("GET", "/api/inbox/conversations?q=nothing%25here", None).await["total"], 0);
    h.cleanup().await;
}

#[tokio::test]
async fn replies_follow_the_rules_and_go_through_normal_sending() {
    let mut h = harness!();
    let ours = setup(&mut h).await;
    let them = "+15035550133";
    telnyx(&h, "r-1", them, &ours, "Is the blue one back?", json!([])).await;
    let c = conversation_for(&mut h, them).await;
    let id = c["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/profiles/{}", c["profile"]["id"].as_str().unwrap()), Some(json!({"first_name": "Rae"}))).await;

    // Quiet hours never hold a reply: put the account somewhere it's the middle of the night.
    let night = (0..24).map(|o| format!("Etc/GMT{}{}", if o >= 12 { "-" } else { "+" }, if o >= 12 { o - 12 } else { o }))
        .find(|tz| {
            let t: chrono_tz::Tz = tz.parse().unwrap();
            let hour = chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&t));
            !(8..21).contains(&hour)
        })
        .unwrap();
    sqlx::query("UPDATE accounts SET timezone = $1").bind(&night).execute(&h.state.db).await.unwrap();

    // Dry run first: nothing sent, the text as it would go out.
    let dry = h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Hi {{ first_name|default('there') }}, yes it is!"}))).await;
    assert_eq!(dry["dry_run"], true);
    assert!(dry["body"].as_str().unwrap().contains("Hi Rae, yes it is!"), "{dry}");
    assert!(texts_to(&h, them).is_empty());

    let before: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let sent = h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Hi {{ first_name|default('there') }}, yes it is!", "confirm": true}))).await;
    assert_eq!(sent["sent"], true, "{sent}");
    let out = texts_to(&h, them);
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("Hi Rae, yes it is!") && out[0].contains("Reply STOP"), "{out:?}");
    // Billed through the normal path, and recorded as a received text for segments and reports.
    let after: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts").fetch_one(&h.state.db).await.unwrap();
    assert!(after < before, "reply should be billed");
    assert_eq!(events(&h, "Received SMS", them).await.len(), 1);

    let thread = h.ok("GET", &format!("/api/inbox/conversations/{id}"), None).await;
    let msgs = thread["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["direction"], "outbound");
    assert_eq!(msgs[0]["kind"], "reply");
    assert_eq!(msgs[0]["sent_by"], "owner@acme.test");
    assert!(msgs[0]["body"].as_str().unwrap().contains("Hi Rae"));
    assert_eq!(thread["conversation"]["unread_count"], 0);
    assert_eq!(thread["reply"]["allowed"], true);
    assert!(thread["reply"]["window_ends_at"].is_string());

    // Their last text was over a day ago and they aren't subscribed: the window has closed.
    sqlx::query("UPDATE sms_conversations SET last_inbound_at = now() - interval '25 hours'").execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Still there?", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("24 hours"));
    // Subscribed people can be texted as usual.
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE phone_number = $1").bind(them).execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Still there?", "confirm": true}))).await;

    // STOP (via Twilio this time): shown in the thread, and nothing more can be sent to them.
    twilio(&h, "SM-stop", them, &ours, "Stop", &[]).await;
    assert_eq!(consent(&h, them).await, "unsubscribed");
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{id}"), None).await;
    assert_eq!(thread["messages"][0]["kind"], "stop");
    assert_eq!(thread["reply"]["allowed"], false);
    let (s, v) = h.req("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Wait!", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"]["message"].as_str().unwrap().contains("STOP"));
    // A retried STOP webhook is a no-op, and START brings them back as before.
    twilio(&h, "SM-stop", them, &ours, "Stop", &[]).await;
    assert_eq!(events(&h, "Unsubscribed from SMS", them).await.len(), 1);
    twilio(&h, "SM-start", them, &ours, "START", &[]).await;
    assert_eq!(consent(&h, them).await, "subscribed");
    h.cleanup().await;
}

#[tokio::test]
async fn keyword_sign_ups_record_consent_and_send_the_disclosures() {
    let mut h = harness!();
    let ours = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texters"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();

    // Sign-up keywords need the disclosure details first; reserved words are refused.
    let (s, v) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    for reserved in ["STOP", "help", "Unstop", "Y", "join now"] {
        let (s, _) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": reserved, "reply": "hi"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{reserved}");
    }
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"program_name": "Acme VIP", "message_frequency": "Up to 4 msgs/month"}))).await;
    let join = h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "Join", "aliases": ["vip"], "subscribe": true, "list_id": list_id, "reply": "Welcome! Use SAVE10."}))).await;
    assert_eq!(join["keyword"], "join");
    let (s, _) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": "VIP", "reply": "dup"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "PROMO", "match_type": "starts_with", "reply": "Today's deal: 20% off."}))).await;

    // Text JOIN (any case) from a new number: subscribed, consent recorded, on the list, confirmed.
    let fan = "+15035550144";
    telnyx(&h, "k-1", fan, &ours, "  join ", json!([])).await;
    h.drain().await;
    assert_eq!(consent(&h, fan).await, "subscribed");
    let sub = events(&h, "Subscribed to SMS", fan).await;
    assert_eq!(sub.len(), 1);
    assert_eq!(sub[0]["method"], "keyword");
    assert_eq!(sub[0]["keyword"], "JOIN");
    assert_eq!(sub[0]["text"], "  join ");
    assert_eq!(h.ok("GET", &format!("/api/lists/{list_id}"), None).await["member_count"], 1);
    let out = texts_to(&h, fan);
    assert_eq!(out.len(), 1, "{out:?}");
    for part in ["Acme VIP:", "Welcome! Use SAVE10.", "Up to 4 msgs/month.", "Msg & data rates may apply.", "HELP", "STOP"] {
        assert!(out[0].contains(part), "{part} missing from {}", out[0]);
    }
    // The reply event carries the keyword, so a flow can start on it.
    assert_eq!(events(&h, "Received SMS Reply", fan).await[0]["keyword"], "JOIN");

    // A starts-with keyword answers without touching consent.
    let curious = "+15035550155";
    telnyx(&h, "k-2", curious, &ours, "promo codes?", json!([])).await;
    h.drain().await;
    assert_eq!(consent(&h, curious).await, "never_subscribed");
    assert!(texts_to(&h, curious)[0].contains("Today's deal"));

    // Double opt-in: pending until they reply Y; nothing marketing before that.
    h.ok("PATCH", &format!("/api/inbox/keywords/{}", join["id"].as_str().unwrap()), Some(json!({"double_opt_in": true}))).await;
    let careful = "+15035550166";
    telnyx(&h, "k-3", careful, &ours, "VIP", json!([])).await;
    h.drain().await;
    assert_eq!(consent(&h, careful).await, "pending");
    assert!(events(&h, "Subscribed to SMS", careful).await.is_empty());
    let ask = texts_to(&h, careful);
    assert!(ask[0].contains("Reply Y to confirm") && ask[0].contains("Msg & data rates may apply"), "{ask:?}");
    telnyx(&h, "k-4", careful, &ours, "Y", json!([])).await;
    h.drain().await;
    assert_eq!(consent(&h, careful).await, "subscribed");
    assert_eq!(events(&h, "Subscribed to SMS", careful).await[0]["method"], "double_opt_in");
    assert!(texts_to(&h, careful)[1].contains("Welcome! Use SAVE10."));
    assert_eq!(h.ok("GET", &format!("/api/lists/{list_id}"), None).await["member_count"], 2);

    // Someone who texted STOP can sign up again by texting the keyword themselves.
    telnyx(&h, "k-5", fan, &ours, "STOP", json!([])).await;
    assert_eq!(consent(&h, fan).await, "unsubscribed");
    h.ok("PATCH", &format!("/api/inbox/keywords/{}", join["id"].as_str().unwrap()), Some(json!({"double_opt_in": false}))).await;
    telnyx(&h, "k-6", fan, &ours, "JOIN", json!([])).await;
    h.drain().await;
    assert_eq!(consent(&h, fan).await, "subscribed");

    // Delete; the word becomes an ordinary message.
    h.ok("DELETE", &format!("/api/inbox/keywords/{}", join["id"].as_str().unwrap()), None).await;
    assert_eq!(h.ok("GET", "/api/inbox/keywords", None).await["total"], 1);
    h.cleanup().await;
}

#[tokio::test]
async fn automatic_replies_never_answer_stop_or_help_and_cannot_loop() {
    let mut h = harness!();
    let ours = setup(&mut h).await;
    let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "can't turn on an empty reply");
    let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"default_reply": "Hi {{ first_name"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "template errors are caught when saving");
    // Open around the clock, so the default reply (not the away reply) answers.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true, "default_reply": "Thanks! We'll text you back soon.", "open_hour": 0, "close_hour": 24, "open_days": [1, 2, 3, 4, 5, 6, 7]}))).await;

    // Another bot that answers every text: our default reply goes once, not back and forth.
    let bot = "+15035550177";
    for i in 0..4 {
        telnyx(&h, &format!("bot-{i}"), bot, &ours, "Thanks for your message!", json!([])).await;
        h.drain().await;
    }
    assert_eq!(texts_to(&h, bot), vec!["Acme Goods: Thanks! We'll text you back soon. Reply STOP to opt out.".to_string()]);

    // STOP, HELP and INFO never get an automatic reply, and nobody who texted STOP gets one later.
    let quiet = "+15035550188";
    for (i, word) in ["HELP", "info", "STOP", "hello?"].iter().enumerate() {
        telnyx(&h, &format!("q-{i}"), quiet, &ours, word, json!([])).await;
        h.drain().await;
    }
    assert!(texts_to(&h, quiet).is_empty());

    // Away reply outside opening hours.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"away_reply_enabled": true, "away_reply": "We're closed; back at 9.", "open_days": []}))).await;
    let late = "+15035550199";
    telnyx(&h, "late-1", late, &ours, "anyone there", json!([])).await;
    h.drain().await;
    assert!(texts_to(&h, late)[0].contains("We're closed"));
    h.cleanup().await;
}

#[tokio::test]
async fn flows_can_wait_for_a_reply() {
    let mut h = harness!();
    let ours = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Asked"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let (s, v) = h.req("POST", "/api/flows", Some(json!({"name": "Bad", "trigger": {"type": "list", "list_id": list_id},
        "definition": {"entry": "w", "nodes": [{"id": "w", "type": "wait_for_reply", "amount": 0, "unit": "days", "yes": null, "no": null}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Ask", "trigger": {"type": "list", "list_id": list_id}, "definition": {"entry": "w", "nodes": [
        {"id": "w", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": "y", "no": "n"},
        {"id": "y", "type": "update_profile", "properties": {"replied": true}, "next": null},
        {"id": "n", "type": "update_profile", "properties": {"replied": false}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;

    let a = h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550201", "list_ids": [list_id]}))).await;
    let b = h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550202", "list_ids": [list_id]}))).await;
    h.drain().await;
    let replied = |h: &Harness, id: Value| {
        let db = h.state.db.clone();
        async move {
            sqlx::query_scalar::<_, Option<bool>>("SELECT (properties->>'replied')::boolean FROM profiles WHERE id = $1::uuid")
                .bind(id.as_str().unwrap().to_string())
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(replied(&h, a["id"].clone()).await, None, "still waiting");
    // A replies: the run wakes now and takes the yes path. A STOP from B isn't a reply.
    telnyx(&h, "fw-1", "+15035550201", &ours, "sure!", json!([])).await;
    telnyx(&h, "fw-2", "+15035550202", &ours, "STOP", json!([])).await;
    h.drain().await;
    assert_eq!(replied(&h, a["id"].clone()).await, Some(true));
    assert_eq!(replied(&h, b["id"].clone()).await, None);
    h.fast_forward().await;
    assert_eq!(replied(&h, b["id"].clone()).await, Some(false));
    h.cleanup().await;
}

#[tokio::test]
async fn accounts_only_see_their_own_inbox() {
    let mut h = harness!();
    let ours = setup(&mut h).await;
    telnyx(&h, "iso-1", "+15035550211", &ours, "private question", json!([])).await;
    let c = conversation_for(&mut h, "+15035550211").await;
    let id = c["id"].as_str().unwrap().to_string();
    h.ok("POST", "/api/inbox/saved-replies", Some(json!({"name": "Hours", "body": "We're open 9-5, {{ first_name|default('friend') }}."}))).await;
    let owner_cookie = h.cookie.clone();

    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@else.test", "password": "correct horse", "company": "Else"}))).await;
    assert_eq!(h.ok("GET", "/api/inbox/conversations?status=all", None).await["total"], 0);
    assert_eq!(h.ok("GET", "/api/inbox/saved-replies", None).await["total"], 0);
    for (m, path, body) in [
        ("GET", format!("/api/inbox/conversations/{id}"), None),
        ("POST", format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "hi", "confirm": true}))),
        ("POST", format!("/api/inbox/conversations/{id}/status"), Some(json!({"status": "closed"}))),
        ("POST", format!("/api/inbox/conversations/{id}/read"), Some(json!({}))),
    ] {
        let (s, _) = h.req(m, &path, body).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {path}");
    }
    // Notifying someone outside the account is refused.
    let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"notify_user_ids": [uuid::Uuid::new_v4()]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cookie = None;
    let (s, _) = h.req("GET", "/api/inbox/conversations", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.cookie = owner_cookie;
    assert_eq!(h.ok("GET", "/api/inbox/saved-replies", None).await["total"], 1);
    h.cleanup().await;
}

#[tokio::test]
async fn digests_and_mcp_tools() {
    let mut h = harness!();
    let ours = setup(&mut h).await;
    let settings = h.ok("GET", "/api/inbox/settings", None).await;
    let me = settings["members"][0]["id"].clone();
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"notify_user_ids": [me]}))).await;
    for i in 0..3 {
        telnyx(&h, &format!("d-{i}"), "+15035550221", &ours, &format!("question {i}"), json!([])).await;
    }
    telnyx(&h, "d-9", "+15035550222", &ours, "hi", json!([])).await;
    // One digest for all four texts, not one email each; nothing again until new texts arrive.
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    h.drain().await;
    dumbmail::jobs::scheduler_tick(&h.state, 6).await.unwrap();
    h.drain().await;
    let mail = h.state.email.sent();
    let digests: Vec<_> = mail.iter().filter(|m| m.subject.contains("new texts")).collect();
    assert_eq!(digests.len(), 1, "{:?}", mail.iter().map(|m| &m.subject).collect::<Vec<_>>());
    assert_eq!(digests[0].to, "owner@acme.test");
    assert!(digests[0].subject.starts_with("4 new texts"), "{}", digests[0].subject);
    assert!(digests[0].text.contains("question 2"));

    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Agent"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let rpc = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let (_, _, list) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string()).await;
    for t in ["list_conversations", "get_conversation", "send_sms_reply", "close_conversation", "list_sms_keywords", "upsert_sms_keyword", "delete_sms_keyword", "list_saved_replies", "upsert_saved_reply"] {
        assert!(list.contains(&format!("\"{t}\"")), "{t} missing");
    }
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("list_conversations", json!({"unread": true}))).await;
    let out: Value = serde_json::from_str(&out).unwrap();
    let convs = &out["result"]["structuredContent"]["data"];
    assert_eq!(convs.as_array().unwrap().len(), 2, "{out}");
    let cid = convs[0]["id"].as_str().unwrap().to_string();
    // Without confirm: a dry run, nothing sent.
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("send_sms_reply", json!({"conversation_id": cid, "body": "On it!"}))).await;
    let out: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["result"]["structuredContent"]["dry_run"], true, "{out}");
    assert!(h.state.sms.sent().is_empty());
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("send_sms_reply", json!({"conversation_id": cid, "body": "On it!", "confirm": true}))).await;
    let out: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["result"]["structuredContent"]["sent"], true, "{out}");
    assert_eq!(h.state.sms.sent().len(), 1);
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("upsert_sms_keyword", json!({"keyword": "HOURS", "reply": "Open 9-5"}))).await;
    let out: Value = serde_json::from_str(&out).unwrap();
    let kid = out["result"]["structuredContent"]["id"].as_str().unwrap().to_string();
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("delete_sms_keyword", json!({"id": kid}))).await;
    assert!(out.contains("dry_run"));
    assert_eq!(h.ok("GET", "/api/inbox/keywords", None).await["total"], 1);
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("close_conversation", json!({"conversation_id": cid}))).await;
    assert!(out.contains("closed"));
    let (_, _, docs) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/sms-inbox"}}).to_string()).await;
    assert!(docs.contains("Text JOIN to subscribe"));
    h.cleanup().await;
}

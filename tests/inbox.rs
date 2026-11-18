//! Two-way texting: inbound storage on both providers (signed as each provider signs), opt-outs
//! (every wording, both providers, against everything already queued, across failures and
//! retries), unverified webhooks, reply windows, keyword consent, HELP, automatic-text limits,
//! the reply flow step, account isolation and the MCP tools.
#[macro_use]
mod common;
use common::*;

use axum::http::StatusCode;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use uuid::Uuid;

const HOOK: &str = "hooksecret";
const TWILIO_TOKEN: &str = "twilio-test-auth-token";
const TWILIO_URL: &str = "https://pl.test/webhooks/twilio/hooksecret";

fn telnyx_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A harness whose platform Telnyx and Twilio keys are configured, so webhooks are verified.
macro_rules! signed {
    () => {
        match common::Harness::with(|c| {
            c.telnyx_public_key = Some(b64(&telnyx_key().verifying_key().to_bytes()));
            c.twilio_account_sid = Some("AC123".into());
            c.twilio_auth_token = Some(TWILIO_TOKEN.into());
        })
        .await
        {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

/// A Telnyx `message.received` webhook as documented (data.payload: id, from, to[], text, media[], received_at).
fn telnyx_body(id: &str, from: &str, to: &str, text: &str, extra: Value) -> String {
    let mut payload = json!({
        "id": id, "direction": "inbound", "type": "SMS", "text": text, "received_at": chrono::Utc::now().to_rfc3339(),
        "from": {"phone_number": from, "carrier": "T-Mobile", "line_type": "Wireless"},
        "to": [{"phone_number": to, "status": "webhook_delivered"}], "media": [], "record_type": "message"
    });
    if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    json!({"data": {"event_type": "message.received", "id": format!("evt-{id}"), "record_type": "event", "payload": payload}, "meta": {"attempt": 1}}).to_string()
}

/// Post a Telnyx webhook, signed (`{timestamp}|{body}`, Ed25519) unless `sign` is false.
async fn telnyx_post(h: &Harness, body: &str, sign: bool) -> (StatusCode, String) {
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = b64(&telnyx_key().sign(format!("{ts}|{body}").as_bytes()).to_bytes());
    let mut headers = vec![("content-type", "application/json")];
    if sign {
        headers.push(("telnyx-timestamp", ts.as_str()));
        headers.push(("telnyx-signature-ed25519", sig.as_str()));
    }
    let (s, _, out) = h.raw("POST", &format!("/webhooks/telnyx/{HOOK}"), &headers, body).await;
    (s, out)
}

async fn telnyx(h: &Harness, id: &str, from: &str, to: &str, text: &str) {
    let (s, out) = telnyx_post(h, &telnyx_body(id, from, to, text, json!({})), true).await;
    assert_eq!(s, StatusCode::OK, "{out}");
}

fn twilio_params(sid: &str, from: &str, to: &str, text: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = [("MessageSid", sid), ("SmsSid", sid), ("AccountSid", "AC123"), ("From", from), ("To", to), ("Body", text), ("NumMedia", "0"), ("NumSegments", "1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    p.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    p
}

/// Twilio's signature: HMAC-SHA1 (auth token) over the URL then each parameter, sorted, as name+value.
fn twilio_signature(token: &str, params: &[(String, String)]) -> String {
    twilio_signature_for(TWILIO_URL, token, params)
}

fn twilio_signature_for(url: &str, token: &str, params: &[(String, String)]) -> String {
    let mut sorted = params.to_vec();
    sorted.sort();
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(token.as_bytes()).unwrap();
    mac.update(url.as_bytes());
    for (k, v) in &sorted {
        mac.update(k.as_bytes());
        mac.update(v.as_bytes());
    }
    b64(&mac.finalize().into_bytes())
}

async fn twilio_post(h: &Harness, params: &[(String, String)], token: Option<&str>) -> (StatusCode, String) {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        form.append_pair(k, v);
    }
    let sig = token.map(|t| twilio_signature(t, params));
    let mut headers = vec![("content-type", "application/x-www-form-urlencoded")];
    if let Some(s) = &sig {
        headers.push(("x-twilio-signature", s.as_str()));
    }
    let (s, hd, out) = h.raw("POST", &format!("/webhooks/twilio/{HOOK}"), &headers, &form.finish()).await;
    if s == StatusCode::OK {
        assert_eq!(hd.get("content-type").unwrap(), "text/xml");
    }
    (s, out)
}

async fn twilio(h: &Harness, sid: &str, from: &str, to: &str, text: &str) {
    twilio_with(h, sid, from, to, text, &[]).await;
}

async fn twilio_with(h: &Harness, sid: &str, from: &str, to: &str, text: &str, extra: &[(&str, &str)]) {
    let (s, out) = twilio_post(h, &twilio_params(sid, from, to, text, extra), Some(TWILIO_TOKEN)).await;
    assert_eq!(s, StatusCode::OK, "{out}");
}

async fn setup(h: &mut Harness) -> String {
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await
}

/// Inbox settings a sign-up keyword needs, open around the clock.
async fn program(h: &mut Harness) {
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"program_name": "Acme VIP", "message_frequency": "Up to 4 msgs/month", "support_contact": "help@acme.test",
        "open_hour": 0, "close_hour": 24, "open_days": [1, 2, 3, 4, 5, 6, 7]}))).await;
}

async fn conversation_for(h: &mut Harness, phone: &str) -> Value {
    let list = h.ok("GET", "/api/inbox/conversations?status=all&limit=100", None).await;
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

async fn opted_out(h: &Harness, phone: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_opt_outs WHERE phone_number = $1)").bind(phone).fetch_one(&h.state.db).await.unwrap()
}

async fn inbound_kind(h: &Harness, provider_id: &str) -> (String, Option<String>, bool) {
    sqlx::query_as("SELECT kind, keyword, verified FROM sms_inbound WHERE provider_message_id = $1").bind(provider_id).fetch_one(&h.state.db).await.unwrap()
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// A time zone where it's the middle of the day now, so marketing texts aren't held for quiet hours.
fn daytime_tz() -> String {
    (0..24)
        .map(|o| format!("Etc/GMT{}{}", if o >= 12 { "-" } else { "+" }, if o >= 12 { o - 12 } else { o }))
        .find(|tz| {
            let t: chrono_tz::Tz = tz.parse().unwrap();
            (11..17).contains(&chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&t)))
        })
        .unwrap()
}

/// Distinct test phone numbers.
fn person(n: u32) -> String {
    format!("+1503555{:04}", 1000 + n)
}

#[tokio::test]
async fn inbound_texts_are_stored_on_both_providers_and_deduped() {
    let mut h = signed!();
    let ours = setup(&mut h).await;

    // A number we've never seen: a profile is created, SMS consent untouched.
    telnyx(&h, "tx-1", "+15035550111", &ours, "Do you ship to Canada?").await;
    // The provider retries the same webhook: stored once.
    telnyx(&h, "tx-1", "+15035550111", &ours, "Do you ship to Canada?").await;
    let c = conversation_for(&mut h, "+15035550111").await;
    assert_eq!(c["unread_count"], 1, "{c}");
    assert_eq!(c["last_preview"], "Do you ship to Canada?");
    assert_eq!(c["profile"]["sms_consent"], "never_subscribed");
    assert_eq!(c["can_reply"], true);
    assert_eq!(inbound_kind(&h, "tx-1").await, ("message".into(), None, true));
    let ev = events(&h, "Received SMS Reply", "+15035550111").await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["text"], "Do you ship to Canada?");

    // Twilio MMS: the media links are kept. Copying refuses private addresses (the SSRF guard), so
    // these stay as links; nothing breaks.
    let mut p = twilio_params("SM100", "+15035550122", &ours, "", &[]);
    p.retain(|(k, _)| k != "NumMedia");
    p.extend([("NumMedia", "2"), ("MediaUrl0", "http://127.0.0.1:9/cat.jpg"), ("MediaContentType0", "image/jpeg"), ("MediaUrl1", "http://169.254.169.254/latest"), ("MediaContentType1", "image/png")].map(|(k, v)| (k.to_string(), v.to_string())));
    assert_eq!(twilio_post(&h, &p, Some(TWILIO_TOKEN)).await.0, StatusCode::OK);
    assert_eq!(twilio_post(&h, &p, Some(TWILIO_TOKEN)).await.0, StatusCode::OK);
    h.drain().await;
    let c = conversation_for(&mut h, "+15035550122").await;
    assert_eq!(c["last_preview"], "Sent 2 attachments");
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{}", c["id"].as_str().unwrap()), None).await;
    let msgs = thread["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{thread}");
    assert_eq!(msgs[0]["direction"], "inbound");
    assert_eq!(msgs[0]["verified"], true);
    assert_eq!(msgs[0]["media"][0]["url"], "http://127.0.0.1:9/cat.jpg");
    assert_eq!(msgs[0]["media"][0]["stored"], false);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_media").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0);

    // A Twilio media link needs the account's credentials, so the dashboard isn't sent to it.
    let mut p = twilio_params("SM102", "+15035550123", &ours, "pic", &[]);
    p.retain(|(k, _)| k != "NumMedia");
    p.extend([("NumMedia", "1"), ("MediaUrl0", "https://api.twilio.com/2010-04-01/Accounts/AC123/Messages/SM102/Media/ME1"), ("MediaContentType0", "image/jpeg")].map(|(k, v)| (k.to_string(), v.to_string())));
    assert_eq!(twilio_post(&h, &p, Some(TWILIO_TOKEN)).await.0, StatusCode::OK);
    let c = conversation_for(&mut h, "+15035550123").await;
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(thread["messages"][0]["media"][0]["url"], Value::Null, "{thread}");

    // Unread counts, read, close and reopen-on-new-text.
    assert_eq!(h.ok("GET", "/api/inbox/unread", None).await["conversations"], 3);
    let c = conversation_for(&mut h, "+15035550122").await;
    let id = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/inbox/conversations/{id}/read"), Some(json!({}))).await;
    h.ok("POST", &format!("/api/inbox/conversations/{id}/status"), Some(json!({"status": "closed"}))).await;
    assert_eq!(h.ok("GET", "/api/inbox/conversations", None).await["total"], 2);
    assert_eq!(h.ok("GET", "/api/inbox/conversations?status=closed", None).await["total"], 1);
    twilio(&h, "SM101", "+15035550122", &ours, "hello again").await;
    assert_eq!(h.ok("GET", "/api/inbox/conversations", None).await["total"], 3);
    // Search by words in their texts and by phone digits; paging.
    assert_eq!(h.ok("GET", "/api/inbox/conversations?q=canada", None).await["total"], 1);
    assert_eq!(h.ok("GET", "/api/inbox/conversations?q=555-0122", None).await["total"], 1);
    assert_eq!(h.ok("GET", "/api/inbox/conversations?q=nothing%25here", None).await["total"], 0);
    let page = h.ok("GET", "/api/inbox/conversations?limit=2&offset=2", None).await;
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn replies_follow_the_window_of_the_number_that_texted() {
    let mut h = signed!();
    let first = setup(&mut h).await;
    let them = "+15035550133";
    telnyx(&h, "r-1", them, &first, "Is the blue one back?").await;
    // The account claims a second number (now its default); replies still go from the one they texted.
    let second = h.claim_number().await;
    assert_ne!(first, second);
    let c = conversation_for(&mut h, them).await;
    let id = c["id"].as_str().unwrap().to_string();
    let pid = c["profile"]["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/profiles/{pid}"), Some(json!({"first_name": "Rae"}))).await;

    // Quiet hours never hold a reply: put the account somewhere it's the middle of the night.
    let night = (0..24)
        .map(|o| format!("Etc/GMT{}{}", if o >= 12 { "-" } else { "+" }, if o >= 12 { o - 12 } else { o }))
        .find(|tz| {
            let t: chrono_tz::Tz = tz.parse().unwrap();
            let hour = chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&t));
            !(8..21).contains(&hour)
        })
        .unwrap();
    sqlx::query("UPDATE accounts SET timezone = $1").bind(&night).execute(&h.state.db).await.unwrap();

    // Dry run first: nothing sent, the text as it would go out, between the two numbers.
    let dry = h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Hi {{ first_name|default('there') }}, yes it is!"}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["to"], them);
    assert_eq!(dry["from"], first.as_str());
    assert!(dry["body"].as_str().unwrap().contains("Hi Rae, yes it is!"), "{dry}");
    assert!(texts_to(&h, them).is_empty());

    let before: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let sent = h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Hi {{ first_name|default('there') }}, yes it is!", "confirm": true}))).await;
    assert_eq!(sent["sent"], true, "{sent}");
    let out: Vec<_> = h.state.sms.sent().into_iter().filter(|s| s.to == them).collect();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].from, first, "replies go out from the number they texted");
    assert!(out[0].body.contains("Hi Rae, yes it is!") && out[0].body.contains("Reply STOP"), "{:?}", out[0]);
    // Billed through the normal path, and recorded as a received text for segments and reports.
    let after: i64 = sqlx::query_scalar("SELECT balance_micros FROM accounts").fetch_one(&h.state.db).await.unwrap();
    assert!(after < before, "reply should be billed");
    assert_eq!(events(&h, "Received SMS", them).await.len(), 1);
    let kind: String = sqlx::query_scalar("SELECT kind FROM sms_sent_texts").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(kind, "reply", "marked so smart sending can leave it out");

    let thread = h.ok("GET", &format!("/api/inbox/conversations/{id}"), None).await;
    let msgs = thread["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["direction"], "outbound");
    assert_eq!(msgs[0]["kind"], "reply");
    assert_eq!(msgs[0]["sent_by"], "owner@acme.test");
    assert_eq!(thread["conversation"]["unread_count"], 0);
    assert_eq!(thread["reply"]["allowed"], true);
    assert!(thread["reply"]["window_ends_at"].is_string());

    // Editing the profile's phone doesn't move the window or the reply to the new number.
    h.ok("PATCH", &format!("/api/profiles/{pid}"), Some(json!({"phone_number": "+15035550134"}))).await;
    let dry = h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Still there?"}))).await;
    assert_eq!(dry["to"], them);

    // The window is 24 hours from when the provider received their text.
    sqlx::query("UPDATE sms_inbound SET provider_received_at = now() - interval '25 hours'").execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Still there?", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("24 hours"));
    // Subscribed on a different number than the one that texted doesn't count either.
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE id = $1::uuid").bind(&pid).execute(&h.state.db).await.unwrap();
    let (s, _) = h.req("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Still there?", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Subscribed on the number that texted: texted as usual.
    h.ok("PATCH", &format!("/api/profiles/{pid}"), Some(json!({"phone_number": them}))).await;
    h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Still there?", "confirm": true}))).await;

    // A webhook delivered late opens its window from when the provider got it, not when we did.
    let late = "+15035550135";
    let body = telnyx_body("r-late", late, &first, "hello?", json!({"received_at": (chrono::Utc::now() - chrono::Duration::hours(30)).to_rfc3339()}));
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    let c = conversation_for(&mut h, late).await;
    assert_eq!(c["can_reply"], false, "{c}");

    // STOP (via Twilio this time): shown in the thread, and nothing more can be sent to them.
    twilio(&h, "SM-stop", them, &first, "Stop").await;
    assert_eq!(consent(&h, them).await, "unsubscribed");
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{id}"), None).await;
    assert_eq!(thread["messages"][0]["kind"], "stop");
    assert_eq!(thread["reply"]["allowed"], false);
    let (s, v) = h.req("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Wait!", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"]["message"].as_str().unwrap().contains("STOP"));
    // A retried STOP webhook is a no-op, and START (on Twilio) brings them back as before the inbox did.
    twilio(&h, "SM-stop", them, &first, "Stop").await;
    assert_eq!(events(&h, "Unsubscribed from SMS", them).await.len(), 1);
    twilio(&h, "SM-start", them, &first, "START").await;
    assert_eq!(consent(&h, them).await, "subscribed");
    assert!(!opted_out(&h, them).await);
    h.cleanup().await;
}

#[tokio::test]
async fn every_opt_out_wording_works_on_both_providers() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    program(&mut h).await;
    // Everything that could answer a text is on, so a missed opt-out would show up as a text.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true, "default_reply": "Thanks! We'll text you back soon."}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true, "reply": "Welcome!"}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "PLEASE", "match_type": "starts_with", "reply": "Sure thing."}))).await;

    let wordings = [
        "STOP", "stop", " Stop. ", "STOP!!!", "STOPALL", "stop all", "UNSUBSCRIBE", "Cancel", "END", "quit", "REVOKE", "OPTOUT", "opt-out", "Opt out",
        "Please STOP texting me", "please unsubscribe me", "how do I opt out?", "Please opt-out this number", "remove me from this list", "DO NOT TEXT me", "don't text me again",
        "Don\u{2019}t text this number", "dont text me", "take me off your list",
    ];
    let mut n = 0;
    for (i, w) in wordings.iter().enumerate() {
        n += 1;
        let a = person(n);
        telnyx(&h, &format!("ow-t-{i}"), &a, &ours, w).await;
        n += 1;
        let b = person(n);
        twilio(&h, &format!("ow-w-{i}"), &b, &ours, w).await;
    }
    // The provider's own opt-out signal wins whatever the words.
    n += 1;
    let tsig = person(n);
    let body = telnyx_body("ow-t-signal", &tsig, &ours, "Arret", json!({"autoresponse_type": "STOP"}));
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    n += 1;
    let wsig = person(n);
    twilio_with(&h, "ow-w-signal", &wsig, &ours, "Arr\u{ea}t", &[("OptOutType", "STOP")]).await;
    h.drain().await;
    for i in 1..=n {
        let p = person(i);
        assert!(opted_out(&h, &p).await, "{p} not opted out");
        assert_eq!(consent(&h, &p).await, "unsubscribed", "{p}");
        assert!(texts_to(&h, &p).is_empty(), "{p} was texted: {:?}", texts_to(&h, &p));
        assert!(events(&h, "Received SMS Reply", &p).await.is_empty(), "{p}: an opt-out isn't a reply");
        assert_eq!(events(&h, "Unsubscribed from SMS", &p).await.len(), 1, "{p}");
    }
    for (i, _) in wordings.iter().enumerate() {
        assert_eq!(inbound_kind(&h, &format!("ow-t-{i}")).await.0, "stop");
        assert_eq!(inbound_kind(&h, &format!("ow-w-{i}")).await.0, "stop");
    }
    // Ordinary sentences with those words in them aren't opt-outs.
    let ok = "+15035559001";
    telnyx(&h, "ow-ok", ok, &ours, "Can I cancel my order?").await;
    h.drain().await;
    assert!(!opted_out(&h, ok).await);
    assert_eq!(texts_to(&h, ok).len(), 1);
    // In the thread, an opt-out is labelled as one.
    let c = conversation_for(&mut h, &person(1)).await;
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(thread["messages"][0]["kind"], "stop");
    assert_eq!(c["opted_out"], true);
    h.cleanup().await;
}

#[tokio::test]
async fn after_stop_only_the_providers_opt_in_words_lift_it() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    program(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texters"}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true, "list_id": list["id"], "reply": "Welcome!"}))).await;

    let fan = "+15035550144";
    telnyx(&h, "s-1", fan, &ours, "JOIN").await;
    h.drain().await;
    assert_eq!(consent(&h, fan).await, "subscribed");
    telnyx(&h, "s-2", fan, &ours, "STOP").await;
    assert!(opted_out(&h, fan).await);
    // A marketing keyword from an opted-out number does nothing and sends nothing.
    telnyx(&h, "s-3", fan, &ours, "JOIN").await;
    h.drain().await;
    assert_eq!(consent(&h, fan).await, "unsubscribed");
    assert_eq!(inbound_kind(&h, "s-3").await, ("blocked".into(), Some("join".into()), true));
    assert_eq!(texts_to(&h, fan).len(), 1, "only the first welcome");
    let c = conversation_for(&mut h, fan).await;
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(thread["messages"][0]["kind"], "blocked");
    assert_eq!(thread["messages"][0]["keyword"], "JOIN");
    // YES isn't a Telnyx opt-in word, and Twilio doesn't take YES on toll-free numbers (ours is one).
    telnyx(&h, "s-4", fan, &ours, "yes").await;
    twilio(&h, "s-5", fan, &ours, "YES").await;
    telnyx(&h, "s-6", fan, &ours, "SUBSCRIBE").await;
    assert!(opted_out(&h, fan).await);
    for id in ["s-4", "s-5", "s-6"] {
        assert_eq!(inbound_kind(&h, id).await.0, "blocked", "{id}");
    }
    // START lifts it and, as before the inbox, counts as subscribing again.
    telnyx(&h, "s-7", fan, &ours, "Start").await;
    assert!(!opted_out(&h, fan).await);
    assert_eq!(consent(&h, fan).await, "subscribed");
    // UNSTOP on Twilio, and Twilio's own START signal, do too.
    let other = "+15035550145";
    twilio(&h, "s-8", other, &ours, "stop").await;
    twilio(&h, "s-9", other, &ours, "unstop").await;
    assert!(!opted_out(&h, other).await);
    let third = "+15035550146";
    twilio(&h, "s-10", third, &ours, "quit").await;
    twilio_with(&h, "s-11", third, &ours, "Resume", &[("OptOutType", "START")]).await;
    assert!(!opted_out(&h, third).await);
    h.cleanup().await;
}

#[tokio::test]
async fn stop_wins_over_texts_already_queued() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    program(&mut h).await;
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true, "default_reply": "Thanks! We'll text you back soon."}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true, "reply": "Welcome!"}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "VIP", "subscribe": true, "double_opt_in": true}))).await;

    // An automatic reply waiting in the queue.
    let a = "+15035550301";
    telnyx(&h, "q-1", a, &ours, "hi there").await;
    telnyx(&h, "q-2", a, &ours, "STOP").await;
    h.drain().await;
    assert!(texts_to(&h, a).is_empty());
    let status: String = sqlx::query_scalar("SELECT status FROM sms_auto_sends WHERE to_number = $1").bind(a).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "cancelled");

    // A keyword confirmation waiting in the queue: no text, and not subscribed.
    let b = "+15035550302";
    telnyx(&h, "q-3", b, &ours, "JOIN").await;
    assert_eq!(consent(&h, b).await, "never_subscribed", "not subscribed before the confirmation goes");
    telnyx(&h, "q-4", b, &ours, "stop please").await;
    h.drain().await;
    assert!(texts_to(&h, b).is_empty());
    assert_eq!(consent(&h, b).await, "unsubscribed");
    let consents: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_consents").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consents, 0);

    // A pending double opt-in: the prompt went out, then STOP, then Y. Nothing more.
    let c = "+15035550303";
    telnyx(&h, "q-5", c, &ours, "VIP").await;
    h.drain().await;
    assert_eq!(texts_to(&h, c).len(), 1);
    telnyx(&h, "q-6", c, &ours, "STOP").await;
    telnyx(&h, "q-7", c, &ours, "Y").await;
    h.drain().await;
    assert_eq!(texts_to(&h, c).len(), 1);
    assert_eq!(consent(&h, c).await, "unsubscribed");
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_pending_optins").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(pending, 0);

    // A flow's text step waiting behind a delay.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Flow"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Later", "trigger": {"type": "list", "list_id": list_id}, "definition": {"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "hours", "next": "s"},
        {"id": "s", "type": "sms", "content": {"body": "Big sale today!"}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let d = "+15035550304";
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": d, "list_ids": [list_id]}))).await;
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE phone_number = $1").bind(d).execute(&h.state.db).await.unwrap();
    h.drain().await;
    twilio(&h, "q-8", d, &ours, "Unsubscribe").await;
    sqlx::query("UPDATE accounts SET timezone = $1").bind(daytime_tz()).execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    assert!(texts_to(&h, d).is_empty(), "{:?}", texts_to(&h, d));
    // The run reached the text step (and skipped it), rather than still waiting.
    let done: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs WHERE status = 'completed'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(done, 1);

    // A reply (and a campaign text) already in the send path with consent loaded before the STOP:
    // the last check before the provider refuses it.
    // A number where it's daytime now: at night a marketing text is held for quiet hours before
    // the claim is reached, and this test is about the claim.
    let e = &daytime_number(305);
    telnyx(&h, "q-9", e, &ours, "question?").await;
    let acct = account_id(&h).await;
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = $1").bind(e).fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE id = $1").bind(pid).execute(&h.state.db).await.unwrap();
    let stale = dumbmail::features::inbox::replies::load_profile(&h.state.db, acct, pid).await.unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    telnyx(&h, "q-10", e, &ours, "do not text me").await;
    let content = json!({"body": "Answer to your question"});
    let c = dumbmail::sending::Conversational { kind: "reply", sent_by: None, to: e.to_string(), from: ours.clone() };
    let out = dumbmail::sending::send_conversational(&h.state, spec(&account, &stale, &content), &c).await.unwrap();
    assert!(matches!(out, dumbmail::sending::Outcome::Skipped("opted out") | dumbmail::sending::Outcome::Skipped("texted STOP")), "{out:?}");
    // The stale profile still says subscribed; the claim reads the opt-out and refuses.
    let out = dumbmail::sending::send(&h.state, spec(&account, &stale, &content)).await.unwrap();
    assert!(matches!(out, dumbmail::sending::Outcome::Skipped("opted out")), "{out:?}");
    assert!(texts_to(&h, e).is_empty());
    h.cleanup().await;
}

fn spec<'a>(account: &'a dumbmail::models::Account, profile: &'a dumbmail::models::Profile, content: &'a Value) -> dumbmail::sending::SendSpec<'a> {
    dumbmail::sending::SendSpec {
        account,
        profile,
        channel: "sms",
        content,
        campaign_id: None,
        flow_id: None,
        flow_node_id: None,
        event: None,
        is_test: false,
        message_id: None,
        to_override: None,
    }
}

#[tokio::test]
async fn a_failed_write_is_applied_when_the_provider_redelivers() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let a = "+15035550401";
    telnyx(&h, "f-0", a, &ours, "hello").await;
    // Writing the opt-out fails: the text isn't stored either, so the provider's retry isn't a duplicate.
    sqlx::query("CREATE FUNCTION fail_write() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'disk full'; END $$ LANGUAGE plpgsql").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_opt_out BEFORE INSERT ON sms_opt_outs FOR EACH ROW EXECUTE FUNCTION fail_write()").execute(&h.state.db).await.unwrap();
    let body = telnyx_body("f-1", a, &ours, "STOP", json!({}));
    let (s, _) = telnyx_post(&h, &body, true).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_inbound WHERE provider_message_id = 'f-1'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0);
    assert_eq!(consent(&h, a).await, "never_subscribed");
    let b = "+15035550402";
    let params = twilio_params("f-2", b, &ours, "stop", &[]);
    assert_eq!(twilio_post(&h, &params, Some(TWILIO_TOKEN)).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    sqlx::query("DROP TRIGGER fail_opt_out ON sms_opt_outs").execute(&h.state.db).await.unwrap();
    // The provider redelivers the same messages: now they apply.
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    assert_eq!(twilio_post(&h, &params, Some(TWILIO_TOKEN)).await.0, StatusCode::OK);
    for p in [a, b] {
        assert!(opted_out(&h, p).await, "{p}");
        assert_eq!(consent(&h, p).await, "unsubscribed");
        assert_eq!(events(&h, "Unsubscribed from SMS", p).await.len(), 1);
    }

    // A failure after the text was stored (recording its event): the retry finishes the job once.
    sqlx::query("CREATE TRIGGER fail_events BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION fail_write()").execute(&h.state.db).await.unwrap();
    let c = "+15035550403";
    let body = telnyx_body("f-3", c, &ours, "where's my order?", json!({}));
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let processed: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT processed_at FROM sms_inbound WHERE provider_message_id = 'f-3'").fetch_one(&h.state.db).await.unwrap();
    assert!(processed.is_none());
    sqlx::query("DROP TRIGGER fail_events ON events").execute(&h.state.db).await.unwrap();
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    assert_eq!(events(&h, "Received SMS Reply", c).await.len(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn forged_webhooks_are_rejected_where_a_key_is_set() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let body = telnyx_body("v-1", "+15035550501", &ours, "JOIN", json!({}));
    // Unsigned, wrongly signed, or too old: rejected, nothing stored.
    assert_eq!(telnyx_post(&h, &body, false).await.0, StatusCode::FORBIDDEN);
    let other = SigningKey::from_bytes(&[9u8; 32]);
    let ts = chrono::Utc::now().timestamp().to_string();
    let bad = b64(&other.sign(format!("{ts}|{body}").as_bytes()).to_bytes());
    let (s, _, _) = h.raw("POST", &format!("/webhooks/telnyx/{HOOK}"), &[("content-type", "application/json"), ("telnyx-timestamp", &ts), ("telnyx-signature-ed25519", &bad)], &body).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let old = (chrono::Utc::now().timestamp() - 600).to_string();
    let sig = b64(&telnyx_key().sign(format!("{old}|{body}").as_bytes()).to_bytes());
    let (s, _, _) = h.raw("POST", &format!("/webhooks/telnyx/{HOOK}"), &[("content-type", "application/json"), ("telnyx-timestamp", &old), ("telnyx-signature-ed25519", &sig)], &body).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let params = twilio_params("v-2", "+15035550502", &ours, "JOIN", &[]);
    assert_eq!(twilio_post(&h, &params, None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(twilio_post(&h, &params, Some("not-the-token")).await.0, StatusCode::FORBIDDEN);
    let mut changed = params.clone();
    let sig = twilio_signature(TWILIO_TOKEN, &params);
    changed[5].1 = "STOP".into();
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in &changed {
        form.append_pair(k, v);
    }
    let (s, _, _) = h.raw("POST", &format!("/webhooks/twilio/{HOOK}"), &[("content-type", "application/x-www-form-urlencoded"), ("x-twilio-signature", &sig)], &form.finish()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a changed body fails");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_inbound").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0);
    let settings = h.ok("GET", "/api/inbox/settings", None).await;
    assert_eq!(settings["verification"]["configured"], false, "this test server's provider is the log one: {settings}");
    h.cleanup().await;
}

#[tokio::test]
async fn unverified_texts_can_opt_out_but_nothing_else() {
    // No keys configured (an existing install): texts are kept, opt-outs honored, nothing else.
    let mut h = harness!();
    let ours = setup(&mut h).await;
    program(&mut h).await;
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true, "default_reply": "Thanks!"}))).await;
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true, "reply": "Welcome!"}))).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Asked"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Ask", "trigger": {"type": "list", "list_id": list_id}, "definition": {"entry": "w", "nodes": [
        {"id": "w", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": "y", "no": null},
        {"id": "y", "type": "update_profile", "properties": {"replied": true}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let a = "+15035550601";
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": a, "list_ids": [list_id]}))).await;
    h.drain().await;

    for (id, text) in [("u-1", "JOIN"), ("u-2", "hello?"), ("u-3", "HELP")] {
        assert_eq!(telnyx_post(&h, &telnyx_body(id, a, &ours, text, json!({})), false).await.0, StatusCode::OK);
    }
    h.drain().await;
    assert_eq!(inbound_kind(&h, "u-1").await, ("keyword".into(), Some("join".into()), false));
    assert!(texts_to(&h, a).is_empty(), "no keyword answer, automatic reply or HELP answer");
    assert_eq!(consent(&h, a).await, "never_subscribed");
    assert!(events(&h, "Received SMS Reply", a).await.is_empty(), "no reply event, so no flow starts");
    let replied: Option<bool> = sqlx::query_scalar("SELECT (properties->>'replied')::boolean FROM profiles WHERE phone_number = $1").bind(a).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(replied, None, "the waiting flow wasn't woken");
    let c = conversation_for(&mut h, a).await;
    assert_eq!(c["can_reply"], false);
    let thread = h.ok("GET", &format!("/api/inbox/conversations/{}", c["id"].as_str().unwrap()), None).await;
    assert!(thread["reply"]["reason"].as_str().unwrap().contains("verified"), "{thread}");
    assert_eq!(thread["messages"][0]["verified"], false);
    let settings = h.ok("GET", "/api/inbox/settings", None).await;
    assert_eq!(settings["verification"]["configured"], false);

    // An unverified STOP is honored; an unverified START can't undo it.
    let params = twilio_params("u-4", a, &ours, "STOP", &[]);
    assert_eq!(twilio_post(&h, &params, None).await.0, StatusCode::OK);
    assert!(opted_out(&h, a).await);
    assert_eq!(telnyx_post(&h, &telnyx_body("u-5", a, &ours, "START", json!({})), false).await.0, StatusCode::OK);
    assert!(opted_out(&h, a).await);
    assert_eq!(consent(&h, a).await, "unsubscribed");

    // No message id: refused, except that an opt-out still applies.
    let b = "+15035550602";
    let no_id = json!({"data": {"event_type": "message.received", "payload": {"text": "hi", "from": {"phone_number": b}, "to": [{"phone_number": ours}]}}}).to_string();
    assert_eq!(telnyx_post(&h, &no_id, false).await.0, StatusCode::BAD_REQUEST);
    let no_id_stop = no_id.replace("\"hi\"", "\"Stop texting me\"");
    assert_eq!(telnyx_post(&h, &no_id_stop, false).await.0, StatusCode::OK);
    assert!(opted_out(&h, b).await);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_inbound WHERE from_number = $1").bind(b).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0);

    // A number two accounts claim: nothing stored in either, not even an opt-out, and not even
    // through one of their own webhook addresses.
    let acme_token: String = sqlx::query_scalar("SELECT webhook_token FROM accounts WHERE name = 'Acme Goods'").fetch_one(&h.state.db).await.unwrap();
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@else.test", "password": "correct horse", "company": "Else"}))).await;
    sqlx::query("UPDATE accounts SET sms_from = $1 WHERE name = 'Else'").bind(&ours).execute(&h.state.db).await.unwrap();
    let c2 = "+15035550603";
    assert_eq!(telnyx_post(&h, &telnyx_body("u-6", c2, &ours, "private question", json!({})), false).await.0, StatusCode::OK);
    let (s, _, _) = h.raw("POST", &format!("/webhooks/telnyx/{acme_token}"), &[("content-type", "application/json")], &telnyx_body("u-6b", c2, &ours, "private question", json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_inbound WHERE from_number = $1").bind(c2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 0);
    assert_eq!(telnyx_post(&h, &telnyx_body("u-7", c2, &ours, "STOP", json!({})), false).await.0, StatusCode::OK);
    assert_eq!(telnyx_post(&h, &no_id_stop.replace(b, c2), false).await.0, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_opt_outs WHERE phone_number = $1").bind(c2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0, "never written into several accounts");
    // The operator hears about the dropped opt-out once that day, by number and account ids only.
    h.drain().await;
    let alerts: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "ops@pl.test" && m.subject.contains("Opt-out not recorded")).collect();
    assert_eq!(alerts.len(), 1, "two opt-outs, one alert");
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE sms_from = $1").bind(&ours).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(ids.len(), 2);
    assert!(alerts[0].subject.contains(&ours) && ids.iter().all(|id| alerts[0].text.contains(&id.to_string())), "{}", alerts[0].text);
    assert!(!alerts[0].text.contains(c2) && !alerts[0].text.contains("Acme") && !alerts[0].text.contains("Else"), "{}", alerts[0].text);
    h.cleanup().await;
}

#[tokio::test]
async fn keyword_sign_ups_need_a_sent_confirmation_and_keep_evidence() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texters"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();

    // Keywords need the HELP contact; sign-ups need the disclosure details and an exact match.
    let (s, v) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": "HOURS", "reply": "9-5"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"support_contact": "help@acme.test"}))).await;
    let (s, v) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    program(&mut h).await;
    let (s, v) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": "JOIN", "subscribe": true, "match_type": "starts_with"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    for reserved in ["STOP", "help", "Unstop", "Y", "join now", "revoke", "OPTOUT"] {
        let (s, _) = h.req("POST", "/api/inbox/keywords", Some(json!({"keyword": reserved, "reply": "hi"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{reserved}");
    }
    let join = h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "Join", "aliases": ["vip"], "subscribe": true, "list_id": list_id, "reply": "Welcome! Use SAVE10."}))).await;
    let join_id = join["id"].as_str().unwrap().to_string();
    h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "PROMO", "match_type": "starts_with", "reply": "Today's deal: 20% off."}))).await;
    // The disclosure and HELP settings can't be cleared while keywords use them.
    for patch in [json!({"program_name": ""}), json!({"message_frequency": ""}), json!({"support_contact": ""})] {
        let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(patch.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{patch}");
    }

    // JOIN: subscribed only once the confirmation, with the full disclosure, is accepted.
    let fan = "+15035550701";
    telnyx(&h, "k-1", fan, &ours, "  join ").await;
    assert_eq!(consent(&h, fan).await, "never_subscribed");
    h.drain().await;
    assert_eq!(consent(&h, fan).await, "subscribed");
    let out = texts_to(&h, fan);
    assert_eq!(out.len(), 1, "{out:?}");
    for part in ["Acme VIP:", "Welcome! Use SAVE10.", "Up to 4 msgs/month.", "Msg & data rates may apply.", "HELP", "STOP"] {
        assert!(out[0].contains(part), "{part} missing from {}", out[0]);
    }
    let (method, disclosure, keyword, version, phone, our, signup_pmid, conf_pmid): (String, String, String, i32, String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT method, disclosure, keyword, keyword_version, phone_number, our_number, signup_provider_message_id, confirmation_provider_message_id FROM sms_consents",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    assert_eq!((method.as_str(), keyword.as_str(), version), ("keyword", "join", 1));
    assert_eq!(disclosure, out[0], "the exact text sent");
    assert_eq!((phone.as_str(), our.as_str()), (fan, ours.as_str()));
    assert_eq!(signup_pmid.as_deref(), Some("k-1"));
    assert!(conf_pmid.unwrap().starts_with("log-"));
    let sub = events(&h, "Subscribed to SMS", fan).await;
    assert_eq!(sub.len(), 1);
    assert_eq!(sub[0]["method"], "keyword");
    assert_eq!(sub[0]["keyword"], "JOIN");
    assert_eq!(h.ok("GET", &format!("/api/lists/{list_id}"), None).await["member_count"], 1);
    assert_eq!(events(&h, "Received SMS Reply", fan).await[0]["keyword"], "JOIN");
    // An ordinary text starting with a sign-up keyword doesn't sign anyone up.
    let chatty = "+15035550702";
    telnyx(&h, "k-2", chatty, &ours, "join me for lunch?").await;
    h.drain().await;
    assert_eq!(consent(&h, chatty).await, "never_subscribed");
    // A starts-with keyword that doesn't subscribe answers without touching consent.
    let curious = "+15035550703";
    telnyx(&h, "k-3", curious, &ours, "promo codes?").await;
    h.drain().await;
    assert_eq!(consent(&h, curious).await, "never_subscribed");
    assert!(texts_to(&h, curious)[0].contains("Today's deal"));

    // If the confirmation can't go (here: the hourly budget is used up), they aren't subscribed.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_account": 0}))).await;
    let capped = "+15035550704";
    telnyx(&h, "k-4", capped, &ours, "JOIN").await;
    h.drain().await;
    assert!(texts_to(&h, capped).is_empty());
    assert_eq!(consent(&h, capped).await, "never_subscribed");
    let s = h.ok("GET", "/api/inbox/settings", None).await;
    assert_eq!(s["auto_paused"], true, "{s}");
    assert_eq!(s["auto_budget_hit_scope"], "account");
    let (st, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_account": 5000}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "above the hard maximum");
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_account": 200}))).await;

    // Double opt-in: a Y counts only after the prompt went out, within 7 days, with the keyword on.
    h.ok("PATCH", &format!("/api/inbox/keywords/{join_id}"), Some(json!({"double_opt_in": true}))).await;
    let careful = "+15035550705";
    telnyx(&h, "k-5", careful, &ours, "VIP").await;
    telnyx(&h, "k-6", careful, &ours, "Y").await; // before the prompt was sent: just a text
    h.drain().await;
    assert_eq!(consent(&h, careful).await, "never_subscribed");
    let ask = texts_to(&h, careful);
    assert!(ask[0].contains("Reply Y to confirm") && ask[0].contains("Msg & data rates may apply"), "{ask:?}");
    telnyx(&h, "k-7", careful, &ours, "yes").await;
    assert_eq!(consent(&h, careful).await, "never_subscribed", "not until the confirmation is accepted");
    h.drain().await;
    assert_eq!(consent(&h, careful).await, "subscribed");
    assert!(texts_to(&h, careful)[1].contains("Welcome! Use SAVE10."));
    let (method, confirm_pmid, prompt_pmid, version): (String, Option<String>, Option<String>, i32) =
        sqlx::query_as("SELECT method, confirm_provider_message_id, prompt_provider_message_id, keyword_version FROM sms_consents WHERE phone_number = $1").bind(careful).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(method, "double_opt_in");
    assert_eq!(confirm_pmid.as_deref(), Some("k-7"));
    assert!(prompt_pmid.unwrap().starts_with("log-"));
    assert_eq!(version, 2, "the keyword was edited once");
    assert_eq!(h.ok("GET", &format!("/api/lists/{list_id}"), None).await["member_count"], 2);
    // Expired prompt.
    let slow = "+15035550706";
    telnyx(&h, "k-8", slow, &ours, "VIP").await;
    h.drain().await;
    sqlx::query("UPDATE sms_pending_optins SET prompt_sent_at = now() - interval '8 days'").execute(&h.state.db).await.unwrap();
    telnyx(&h, "k-9", slow, &ours, "Y").await;
    h.drain().await;
    assert_eq!(consent(&h, slow).await, "never_subscribed");
    // Keyword turned off after the prompt.
    let late = "+15035550707";
    telnyx(&h, "k-10", late, &ours, "VIP").await;
    h.drain().await;
    h.ok("PATCH", &format!("/api/inbox/keywords/{join_id}"), Some(json!({"enabled": false}))).await;
    telnyx(&h, "k-11", late, &ours, "Y").await;
    h.drain().await;
    assert_eq!(consent(&h, late).await, "never_subscribed");

    // Delete; the word becomes an ordinary message.
    h.ok("DELETE", &format!("/api/inbox/keywords/{join_id}"), None).await;
    assert_eq!(h.ok("GET", "/api/inbox/keywords", None).await["total"], 1);
    h.cleanup().await;
}

#[tokio::test]
async fn help_gets_one_fixed_answer() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    program(&mut h).await;
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true, "default_reply": "Thanks!"}))).await;
    h.ok("POST", "/api/flows", Some(json!({"name": "On reply", "trigger": {"type": "metric", "metric": "Received SMS Reply"}, "definition": {"entry": "u", "nodes": [
        {"id": "u", "type": "update_profile", "properties": {"flowed": true}, "next": null}
    ]}}))).await;
    let flow_id: Uuid = sqlx::query_scalar("SELECT id FROM flows").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/flows/{flow_id}/status"), Some(json!({"status": "live"}))).await;

    // Telnyx: DumbMail answers, once an hour per number, and nothing else happens.
    let a = "+15035550801";
    telnyx(&h, "h-1", a, &ours, "HELP").await;
    telnyx(&h, "h-2", a, &ours, "help").await;
    telnyx(&h, "h-3", a, &ours, "Info").await;
    h.drain().await;
    let out = texts_to(&h, a);
    assert_eq!(out.len(), 1, "{out:?}");
    for part in ["Acme VIP", "help@acme.test", "Msg & data rates may apply", "Reply STOP to cancel"] {
        assert!(out[0].contains(part), "{part} missing from {}", out[0]);
    }
    assert!(events(&h, "Received SMS Reply", a).await.is_empty(), "HELP doesn't start flows");
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(runs, 0);
    let kind: String = sqlx::query_scalar("SELECT kind FROM sms_sent_texts t JOIN messages m ON m.id = t.message_id WHERE m.to_address = $1").bind(a).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(kind, "help");
    // A HELP doesn't open the reply window: only a subscriber could be answered.
    let c = conversation_for(&mut h, a).await;
    assert_eq!(c["can_reply"], false, "{c}");
    // HELP answers have their own allowance: used-up automatic-text budgets don't stop them. And
    // two HELPs landing at once from one number get one answer (the write itself refuses the second).
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_account": 0, "auto_budget_number": 0}))).await;
    let twice = "+15035550806";
    let (b1, b2) = (telnyx_body("h-9", twice, &ours, "HELP", json!({})), telnyx_body("h-10", twice, &ours, "help", json!({})));
    let (one, two) = tokio::join!(telnyx_post(&h, &b1, true), telnyx_post(&h, &b2, true));
    assert_eq!((one.0, two.0), (StatusCode::OK, StatusCode::OK));
    h.drain().await;
    assert_eq!(texts_to(&h, twice).len(), 1);
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_account": 200, "auto_budget_number": 100}))).await;
    // Telnyx already answered it (its autoresponse said HELP): not answered twice.
    let b = "+15035550802";
    assert_eq!(telnyx_post(&h, &telnyx_body("h-4", b, &ours, "HELP", json!({"autoresponse_type": "HELP"})), true).await.0, StatusCode::OK);
    // Twilio answers HELP itself on long codes and toll-free numbers.
    let c = "+15035550803";
    twilio(&h, "h-5", c, &ours, "HELP").await;
    h.drain().await;
    assert!(texts_to(&h, b).is_empty() && texts_to(&h, c).is_empty());
    // The account can choose DumbMail's answer instead.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"help_replies": "dumbmail"}))).await;
    let d = "+15035550804";
    twilio(&h, "h-6", d, &ours, "help").await;
    h.drain().await;
    assert_eq!(texts_to(&h, d).len(), 1);
    // HELP from an opted-out number is still answered (it's administrative).
    let e = "+15035550805";
    telnyx(&h, "h-7", e, &ours, "STOP").await;
    telnyx(&h, "h-8", e, &ours, "HELP").await;
    h.drain().await;
    let out = texts_to(&h, e);
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("help@acme.test"));
    assert!(opted_out(&h, e).await);
    h.cleanup().await;
}

#[tokio::test]
async fn automatic_texts_are_capped_and_sent_at_most_once() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    program(&mut h).await;
    let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"default_reply": "Hi {{ first_name"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "template errors are caught when saving");
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"default_reply_enabled": true, "default_reply": "Thanks! We'll text you back soon."}))).await;

    // Another bot that answers every text: our default reply goes once, not back and forth.
    let bot = "+15035550901";
    for i in 0..4 {
        telnyx(&h, &format!("bot-{i}"), bot, &ours, "Thanks for your message!").await;
        h.drain().await;
    }
    assert_eq!(texts_to(&h, bot), vec!["Acme Goods: Thanks! We'll text you back soon. Reply STOP to opt out.".to_string()]);

    // Per receiving number budget: the third sender this hour gets nothing, and the dashboard says so.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_number": 3}))).await;
    for i in 0..3 {
        telnyx(&h, &format!("b-{i}"), &person(900 + i), &ours, "hello").await;
    }
    h.drain().await;
    let answered = (0..3).filter(|i| !texts_to(&h, &person(900 + i)).is_empty()).count();
    assert_eq!(answered, 2, "the bot's reply used one of the three");
    let s = h.ok("GET", "/api/inbox/settings", None).await;
    assert_eq!(s["auto_paused"], true);
    assert_eq!(s["auto_budget_hit_scope"], ours.as_str());

    // A job that runs again can't send the same text twice.
    let (id, n): (Uuid, usize) = (sqlx::query_scalar("SELECT id FROM sms_auto_sends WHERE status = 'sent' LIMIT 1").fetch_one(&h.state.db).await.unwrap(), h.state.sms.sent().len());
    dumbmail::jobs::enqueue(&h.state.db, "inbox_auto_send", json!({"id": id}), None).await.unwrap();
    h.drain().await;
    assert_eq!(h.state.sms.sent().len(), n);
    // One that stopped part way (claimed, provider never answered) isn't sent again either.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"auto_budget_number": 100}))).await;
    let x = "+15035550990";
    telnyx(&h, "b-x", x, &ours, "anyone?").await;
    sqlx::query("UPDATE sms_auto_sends SET status = 'sending' WHERE to_number = $1").bind(x).execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert!(texts_to(&h, x).is_empty());
    let st: String = sqlx::query_scalar("SELECT status FROM sms_auto_sends WHERE to_number = $1").bind(x).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(st, "failed");

    // A delayed job checks the reply window again when it runs.
    let y = "+15035550991";
    telnyx(&h, "b-y", y, &ours, "late?").await;
    sqlx::query("UPDATE sms_inbound SET provider_received_at = now() - interval '25 hours' WHERE from_number = $1").bind(y).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE sms_conversations SET last_default_reply_at = NULL").execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert!(texts_to(&h, y).is_empty());

    // Away reply outside opening hours.
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"away_reply_enabled": true, "away_reply": "We're closed; back at 9.", "open_days": []}))).await;
    let late = "+15035550992";
    telnyx(&h, "late-1", late, &ours, "anyone there").await;
    h.drain().await;
    assert!(texts_to(&h, late)[0].contains("We're closed"));
    h.cleanup().await;
}

#[tokio::test]
async fn flows_can_wait_for_a_reply() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Asked"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    for bad in [json!({"amount": 0, "unit": "days"}), json!({"amount": 31, "unit": "days"})] {
        let mut node = json!({"id": "w", "type": "wait_for_reply", "yes": null, "no": null});
        node.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        let (s, v) = h.req("POST", "/api/flows", Some(json!({"name": "Bad", "trigger": {"type": "list", "list_id": list_id}, "definition": {"entry": "w", "nodes": [node]}}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Ask", "trigger": {"type": "list", "list_id": list_id}, "definition": {"entry": "w", "nodes": [
        {"id": "w", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": "y", "no": "n"},
        {"id": "y", "type": "update_profile", "properties": {"replied": true}, "next": null},
        {"id": "n", "type": "update_profile", "properties": {"replied": false}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;

    let a = h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035551201", "list_ids": [list_id]}))).await;
    let b = h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035551202", "list_ids": [list_id]}))).await;
    let c = h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035551203", "list_ids": [list_id]}))).await;
    let d = h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035551204", "list_ids": [list_id]}))).await;
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
    telnyx(&h, "fw-1", "+15035551201", &ours, "sure!").await;
    telnyx(&h, "fw-2", "+15035551202", &ours, "STOP").await;
    h.drain().await;
    assert_eq!(replied(&h, a["id"].clone()).await, Some(true));
    assert_eq!(replied(&h, b["id"].clone()).await, None);
    // D's text from before the wait began, delivered late: it doesn't end the wait or bring the
    // timeout forward, and a wake without a reply inside the wait just keeps waiting.
    let body = telnyx_body("fw-4", "+15035551204", &ours, "old news", json!({"received_at": (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()}));
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    h.drain().await;
    assert_eq!(replied(&h, d["id"].clone()).await, None);
    let due = || {
        let db = h.state.db.clone();
        let id = d["id"].as_str().unwrap().to_string();
        async move {
            sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
                "SELECT max(j.run_at) FROM jobs j JOIN flow_runs r ON r.id::text = j.payload->>'run_id' WHERE j.kind = 'flow_step' AND r.profile_id = $1::uuid AND j.locked_at IS NULL",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert!(due().await > chrono::Utc::now() + chrono::Duration::days(1), "still due at the deadline");
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'flow_step' AND payload->>'run_id' IN (SELECT id::text FROM flow_runs WHERE profile_id = $1::uuid)")
        .bind(d["id"].as_str().unwrap())
        .execute(&h.state.db)
        .await
        .unwrap();
    h.drain().await;
    assert_eq!(replied(&h, d["id"].clone()).await, None, "woken early, it keeps waiting");
    assert!(due().await > chrono::Utc::now() + chrono::Duration::days(1));
    // B's and C's deadlines passed before the worker got to them (the clock is moved by hand: a
    // job made due early keeps waiting until its deadline); a reply after the deadline doesn't count.
    for p in [&b, &c] {
        sqlx::query("UPDATE flow_runs SET context = jsonb_set(context, '{_reply_until}', to_jsonb(now() - interval '1 minute')) WHERE profile_id = $1::uuid")
            .bind(p["id"].as_str().unwrap())
            .execute(&h.state.db)
            .await
            .unwrap();
    }
    telnyx(&h, "fw-3", "+15035551203", &ours, "sorry, late!").await;
    h.fast_forward().await;
    assert_eq!(replied(&h, b["id"].clone()).await, Some(false));
    assert_eq!(replied(&h, c["id"].clone()).await, Some(false));
    h.cleanup().await;
}

#[tokio::test]
async fn inbox_replies_dont_count_for_smart_sending() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    // A number where it's daytime now, so the flow text isn't held for quiet hours.
    let them = &daytime_number(1301);
    telnyx(&h, "ss-1", them, &ours, "is my order shipped?").await;
    let c = conversation_for(&mut h, them).await;
    h.ok("POST", &format!("/api/inbox/conversations/{}/reply", c["id"].as_str().unwrap()), Some(json!({"body": "Yes, today!", "confirm": true}))).await;
    sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE phone_number = $1").bind(them).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE accounts SET timezone = $1").bind(daytime_tz()).execute(&h.state.db).await.unwrap();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Promo"}))).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Promo", "trigger": {"type": "list", "list_id": list["id"]}, "definition": {"entry": "s", "settings": {"smart_sending_hours": 24}, "nodes": [
        {"id": "s", "type": "sms", "content": {"body": "New arrivals are in"}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": [c["profile"]["id"]]}))).await;
    h.fast_forward().await;
    let out = texts_to(&h, them);
    assert_eq!(out.len(), 2, "the earlier inbox reply didn't hold the flow text: {out:?}");
    assert!(out[1].contains("New arrivals"));
    h.cleanup().await;
}

#[tokio::test]
async fn browsers_cant_fake_texting_events() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    for metric in ["Received SMS Reply", "Subscribed to SMS", "Unsubscribed from SMS", "Received SMS", "received sms reply"] {
        let (s, _, body) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
            &json!({"metric": metric, "profile": {"phone_number": "+15035551401"}, "properties": {"text": "JOIN"}}).to_string()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{metric}: {body}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn accounts_only_see_their_own_inbox() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    telnyx(&h, "iso-1", "+15035550211", &ours, "private question").await;
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
    // Notifying someone outside the account is refused; a Telnyx key without your own Telnyx too.
    let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"notify_user_ids": [uuid::Uuid::new_v4()]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.req("PATCH", "/api/inbox/settings", Some(json!({"telnyx_public_key": b64(&[1u8; 32])}))).await;
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
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let settings = h.ok("GET", "/api/inbox/settings", None).await;
    let me = settings["members"][0]["id"].clone();
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"notify_user_ids": [me], "support_contact": "help@acme.test"}))).await;
    for i in 0..3 {
        telnyx(&h, &format!("d-{i}"), "+15035550221", &ours, &format!("question {i}")).await;
    }
    telnyx(&h, "d-9", "+15035550222", &ours, "hi").await;
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
    let kid = out["result"]["structuredContent"]["id"].as_str().unwrap_or_else(|| panic!("{out}")).to_string();
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("delete_sms_keyword", json!({"id": kid}))).await;
    assert!(out.contains("dry_run"));
    assert_eq!(h.ok("GET", "/api/inbox/keywords", None).await["total"], 1);
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &rpc("close_conversation", json!({"conversation_id": cid}))).await;
    assert!(out.contains("closed"));
    let (_, _, docs) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/sms-inbox"}}).to_string()).await;
    assert!(docs.contains("Text JOIN to subscribe"));
    h.cleanup().await;
}

/// A number whose own time zone (as its country and area code say) is in the middle of the day
/// now, so marketing texts to it aren't held for quiet hours. `n` keeps numbers distinct.
fn daytime_number(n: u32) -> String {
    let candidates = [
        format!("+1808555{:04}", n), format!("+1503555{:04}", n), format!("+1303555{:04}", n), format!("+1312555{:04}", n), format!("+1212555{:04}", n),
        format!("+4420794{:05}", n), format!("+9198123{:05}", n), format!("+97798123{:04}", n), format!("+8190123{:05}", n),
    ];
    candidates
        .into_iter()
        .find(|c| {
            dumbmail::features::sendtime::zone::phone_zone(c)
                .is_some_and(|tz| (11..17).contains(&chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&tz))))
        })
        .expect("some zone is in the middle of the day")
}

async fn message_status_to(h: &Harness, phone: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT status, error FROM messages WHERE channel = 'sms' AND to_address = $1 ORDER BY created_at").bind(phone).fetch_all(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn a_stop_that_lands_while_a_text_waits_for_its_turn_stops_it() {
    use dumbmail::sending::LimiterPause;
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let acct = account_id(&h).await;

    // A campaign text, held right after its rate-limiter wait while the STOP commits.
    let camp = daytime_number(1);
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Campaign"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": camp, "sms_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"body": "Sale today. Reply STOP to opt out."}}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.drain().await;
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let pause = LimiterPause::hold(acct);
    let st = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&st).await.unwrap() });
    pause.arrived().await;
    telnyx(&h, "rl-1", &camp, &ours, "STOP").await;
    pause.release(acct);
    worker.await.unwrap();
    assert!(texts_to(&h, &camp).is_empty(), "{:?}", texts_to(&h, &camp));
    assert_eq!(message_status_to(&h, &camp).await, vec![("skipped".to_string(), Some("opted out".to_string()))]);

    // A flow text.
    let flow_person = daytime_number(2);
    let fl = h.ok("POST", "/api/lists", Some(json!({"name": "Flow"}))).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Welcome", "trigger": {"type": "list", "list_id": fl["id"]}, "definition": {"entry": "s", "nodes": [
        {"id": "s", "type": "sms", "content": {"body": "Welcome! Reply STOP to opt out."}, "next": null}
    ]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": flow_person, "sms_consent": "subscribed", "list_ids": [fl["id"]]}))).await;
    let pause = LimiterPause::hold(acct);
    let st = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&st).await.unwrap() });
    pause.arrived().await;
    twilio(&h, "rl-2", &flow_person, &ours, "Stop texting me").await;
    pause.release(acct);
    worker.await.unwrap();
    assert!(texts_to(&h, &flow_person).is_empty());
    assert_eq!(message_status_to(&h, &flow_person).await, vec![("skipped".to_string(), Some("opted out".to_string()))], "the STOP's mark isn't overwritten");

    // A reply typed in the inbox.
    let asker = "+15035552001";
    telnyx(&h, "rl-3", asker, &ours, "Do you have it in blue?").await;
    h.drain().await;
    let conv: Uuid = conversation_for(&mut h, asker).await["id"].as_str().unwrap().parse().unwrap();
    let pause = LimiterPause::hold(acct);
    let st = h.state.clone();
    let reply = tokio::spawn(async move { dumbmail::features::inbox::replies::send_reply(&st, acct, conv, "Yes, we do!", None, true).await });
    pause.arrived().await;
    telnyx(&h, "rl-4", asker, &ours, "unsubscribe").await;
    pause.release(acct);
    let out = reply.await.unwrap();
    assert!(matches!(&out, Err(e) if e.public_message().contains("opted out")), "{:?}", out.map_err(|e| e.public_message()));
    assert!(texts_to(&h, asker).is_empty());

    // A test send typed in another format than the provider uses for the number.
    let tester = "+15035552002";
    let draft = dumbmail::campaigns::load(&h.state.db, acct, cid.parse().unwrap()).await.unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let pause = LimiterPause::hold(acct);
    let st = h.state.clone();
    let test_send = tokio::spawn(async move { dumbmail::campaigns::send_test(&st, &account, &draft, &["(503) 555-2002".to_string()], None, true).await });
    pause.arrived().await;
    telnyx(&h, "rl-5", tester, &ours, "STOP").await;
    pause.release(acct);
    assert_eq!(test_send.await.unwrap().map_err(|e| e.public_message()).unwrap(), 0);
    assert!(texts_to(&h, tester).is_empty() && texts_to(&h, "(503) 555-2002").is_empty());
    assert_eq!(message_status_to(&h, tester).await, vec![("skipped".to_string(), Some("opted out".to_string()))]);

    // Without a STOP, a held text goes out once it's released.
    let fine = "+15035552003";
    telnyx(&h, "rl-6", fine, &ours, "hello?").await;
    h.drain().await;
    let conv: Uuid = conversation_for(&mut h, fine).await["id"].as_str().unwrap().parse().unwrap();
    let pause = LimiterPause::hold(acct);
    let st = h.state.clone();
    let reply = tokio::spawn(async move { dumbmail::features::inbox::replies::send_reply(&st, acct, conv, "Hi!", None, true).await });
    pause.arrived().await;
    pause.release(acct);
    assert!(reply.await.unwrap().is_ok());
    assert_eq!(texts_to(&h, fine).len(), 1);
    assert_eq!(message_status_to(&h, fine).await[0].0, "sent");
    h.cleanup().await;
}

/// Post a Twilio webhook to an account's own address, signed with `token` for that exact URL.
async fn twilio_post_to(h: &Harness, path: &str, params: &[(String, String)], token: &str) -> StatusCode {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        form.append_pair(k, v);
    }
    let sig = twilio_signature_for(&format!("https://pl.test{path}"), token, params);
    h.raw("POST", path, &[("content-type", "application/x-www-form-urlencoded"), ("x-twilio-signature", &sig)], &form.finish()).await.0
}

#[tokio::test]
async fn a_verified_text_belongs_to_exactly_one_account() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let stored = |h: &Harness, from: &'static str| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT a.name FROM sms_inbound i JOIN accounts a ON a.id = i.account_id WHERE i.from_number = $1").bind(from).fetch_all(&db).await.unwrap() }
    };
    let opt_outs = |h: &Harness, phone: &'static str| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT a.name FROM sms_opt_outs o JOIN accounts a ON a.id = o.account_id WHERE o.phone_number = $1").bind(phone).fetch_all(&db).await.unwrap() }
    };

    // A second account that connected its own Twilio, with its own number and webhook address.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "own@twilio.test", "password": "correct horse", "company": "Own Twilio"}))).await;
    let byo_number = "+15035559100";
    let creds = dumbmail::crypto::encrypt(&h.state.cfg.secret_key, &json!({"provider": "twilio", "account_sid": "ACown", "auth_token": "own-token"}).to_string()).unwrap();
    sqlx::query("UPDATE accounts SET sms_mode = 'byo', sms_byo_provider = 'twilio', sms_byo_credentials = $1, sms_from = $2 WHERE name = 'Own Twilio'")
        .bind(&creds)
        .bind(byo_number)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO phone_numbers (id, account_id, phone_number, type, source, provider, status) SELECT $1, id, $2, 'local', 'connected', 'twilio', 'active' FROM accounts WHERE name = 'Own Twilio'")
        .bind(Uuid::now_v7())
        .bind(byo_number)
        .execute(&h.state.db)
        .await
        .unwrap();
    let token: String = sqlx::query_scalar("SELECT webhook_token FROM accounts WHERE name = 'Own Twilio'").fetch_one(&h.state.db).await.unwrap();
    let own_path = format!("/webhooks/twilio/{token}");

    // Its Telnyx address doesn't fall back to "no key" because it uses Twilio: refused.
    let (s, _, _) = h.raw("POST", &format!("/webhooks/telnyx/{token}"), &[("content-type", "application/json")], &telnyx_body("x-1", "+15035559201", byo_number, "JOIN", json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Signed with its own token, for its own number: stored there, verified.
    assert_eq!(twilio_post_to(&h, &own_path, &twilio_params("x-2", "+15035559202", byo_number, "hello", &[]), "own-token").await, StatusCode::OK);
    assert_eq!(stored(&h, "+15035559202").await, vec!["Own Twilio".to_string()]);
    assert!(inbound_kind(&h, "x-2").await.2);
    // Signed with its own token but for the other account's number: nothing stored anywhere, not even a STOP.
    assert_eq!(twilio_post_to(&h, &own_path, &twilio_params("x-3", "+15035559203", &ours, "question", &[]), "own-token").await, StatusCode::OK);
    assert_eq!(twilio_post_to(&h, &own_path, &twilio_params("x-4", "+15035559204", &ours, "STOP", &[]), "own-token").await, StatusCode::OK);
    assert!(stored(&h, "+15035559203").await.is_empty() && stored(&h, "+15035559204").await.is_empty());
    assert!(opt_outs(&h, "+15035559204").await.is_empty());
    // The platform's address (and key) for the connected account's number: the platform doesn't
    // carry that number, so nothing is stored; an opt-out is still recorded there, and only there.
    twilio(&h, "x-5", "+15035559205", byo_number, "hi").await;
    twilio(&h, "x-6", "+15035559206", byo_number, "Stop").await;
    assert!(stored(&h, "+15035559205").await.is_empty() && stored(&h, "+15035559206").await.is_empty());
    assert_eq!(opt_outs(&h, "+15035559206").await, vec!["Own Twilio".to_string()]);
    // The platform's address for the platform account's number works as always.
    twilio(&h, "x-7", "+15035559207", &ours, "hi").await;
    assert_eq!(stored(&h, "+15035559207").await, vec!["Acme Goods".to_string()]);
    h.cleanup().await;
}

#[tokio::test]
async fn start_lifts_only_the_opt_out_made_on_its_own_number() {
    let mut h = signed!();
    let first = setup(&mut h).await;
    let second = h.claim_number().await;
    let fan = "+15035552101";
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": fan}))).await;

    // STOP to the first number; START to the second doesn't undo it.
    telnyx(&h, "p-1", fan, &first, "STOP").await;
    telnyx(&h, "p-2", fan, &second, "START").await;
    assert!(opted_out(&h, fan).await);
    assert_eq!(inbound_kind(&h, "p-2").await.0, "blocked");
    assert_eq!(consent(&h, fan).await, "unsubscribed");
    // START to the number that got the STOP does, and (as before the inbox) subscribes them.
    twilio(&h, "p-3", fan, &first, "Start!").await;
    assert!(!opted_out(&h, fan).await);
    assert_eq!(consent(&h, fan).await, "subscribed");

    // Opted out on both: each START lifts its own; they're blocked until both are lifted.
    telnyx(&h, "p-4", fan, &first, "stop").await;
    telnyx(&h, "p-5", fan, &second, "stop").await;
    telnyx(&h, "p-6", fan, &first, "UNSTOP").await;
    assert!(opted_out(&h, fan).await, "the second number's opt-out still stands");
    assert_eq!(consent(&h, fan).await, "unsubscribed");
    telnyx(&h, "p-7", fan, &second, "start").await;
    assert!(!opted_out(&h, fan).await);
    assert_eq!(consent(&h, fan).await, "subscribed");
    // Only the whole word: "start now" or "s-t-a-r-t" isn't START.
    telnyx(&h, "p-8", fan, &first, "STOP").await;
    for (id, text) in [("p-9", "start now"), ("p-10", "s-t-a-r-t")] {
        telnyx(&h, id, fan, &first, text).await;
        assert!(opted_out(&h, fan).await, "{text}");
    }

    // Twilio's YES opts back in on a long code, not on toll-free; Telnyx's never.
    let long_code = "+15035559300";
    sqlx::query("INSERT INTO phone_numbers (id, account_id, phone_number, type, source, provider, status) SELECT $1, id, $2, 'local', 'purchased', 'telnyx', 'active' FROM accounts")
        .bind(Uuid::now_v7())
        .bind(long_code)
        .execute(&h.state.db)
        .await
        .unwrap();
    let a = "+15035552102";
    twilio(&h, "p-11", a, long_code, "STOP").await;
    telnyx(&h, "p-12", a, long_code, "YES").await;
    assert!(opted_out(&h, a).await, "Telnyx documents only START and UNSTOP");
    twilio(&h, "p-13", a, long_code, "yes").await;
    assert!(!opted_out(&h, a).await, "Twilio long code: YES opts back in");
    let b = "+15035552103";
    twilio(&h, "p-14", b, &first, "STOP").await;
    twilio(&h, "p-15", b, &first, "YES").await;
    assert!(opted_out(&h, b).await, "Twilio toll-free: YES doesn't");
    h.cleanup().await;
}

#[tokio::test]
async fn yes_is_consent_only_as_the_answer_to_a_prompt() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    program(&mut h).await;

    // A known profile texting YES or SUBSCRIBE with no prompt: an ordinary text. START still subscribes.
    let known = "+15035552201";
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": known}))).await;
    telnyx(&h, "y-1", known, &ours, "YES").await;
    twilio(&h, "y-2", known, &ours, "Subscribe").await;
    h.drain().await;
    assert_eq!(consent(&h, known).await, "never_subscribed");
    assert_eq!(inbound_kind(&h, "y-1").await.0, "message");
    assert_eq!(inbound_kind(&h, "y-2").await.0, "message");
    telnyx(&h, "y-3", known, &ours, "START").await;
    assert_eq!(consent(&h, known).await, "subscribed");

    let vip = h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "VIP", "subscribe": true, "double_opt_in": true, "reply": "Welcome to VIP!"}))).await;
    let vip_id = vip["id"].as_str().unwrap().to_string();
    // A sign-up keyword is matched only as the whole text.
    let fuzzy = "+15035552202";
    telnyx(&h, "y-4", fuzzy, &ours, "V-I-P").await;
    h.drain().await;
    assert!(texts_to(&h, fuzzy).is_empty());
    assert_eq!(inbound_kind(&h, "y-4").await.0, "message");

    // YES sent (by the provider's clock) before the prompt went out doesn't count.
    let early = "+15035552203";
    telnyx(&h, "y-5", early, &ours, "VIP!").await;
    h.drain().await;
    assert_eq!(texts_to(&h, early).len(), 1, "the prompt");
    let sent_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT prompt_sent_at FROM sms_pending_optins WHERE phone_number = $1").bind(early).fetch_one(&h.state.db).await.unwrap();
    let body = telnyx_body("y-6", early, &ours, "YES", json!({"received_at": (sent_at - chrono::Duration::seconds(30)).to_rfc3339()}));
    assert_eq!(telnyx_post(&h, &body, true).await.0, StatusCode::OK);
    h.drain().await;
    assert_eq!(consent(&h, early).await, "never_subscribed");
    assert_eq!(inbound_kind(&h, "y-6").await.0, "message");
    // "yes please" isn't YES either; "Yes!" is.
    telnyx(&h, "y-7", early, &ours, "yes please").await;
    h.drain().await;
    assert_eq!(consent(&h, early).await, "never_subscribed");
    telnyx(&h, "y-8", early, &ours, "Yes!").await;
    h.drain().await;
    assert_eq!(consent(&h, early).await, "subscribed");

    // An expired prompt, or the keyword turned off: YES does nothing.
    let slow = "+15035552204";
    telnyx(&h, "y-9", slow, &ours, "VIP").await;
    h.drain().await;
    sqlx::query("UPDATE sms_pending_optins SET prompt_sent_at = now() - interval '8 days' WHERE phone_number = $1").bind(slow).execute(&h.state.db).await.unwrap();
    telnyx(&h, "y-10", slow, &ours, "YES").await;
    h.drain().await;
    assert_eq!(consent(&h, slow).await, "never_subscribed");
    let off = "+15035552205";
    telnyx(&h, "y-11", off, &ours, "VIP").await;
    h.drain().await;
    h.ok("PATCH", &format!("/api/inbox/keywords/{vip_id}"), Some(json!({"enabled": false}))).await;
    telnyx(&h, "y-12", off, &ours, "YES").await;
    h.drain().await;
    assert_eq!(consent(&h, off).await, "never_subscribed");
    h.ok("PATCH", &format!("/api/inbox/keywords/{vip_id}"), Some(json!({"enabled": true}))).await;

    // Staff change the profile's number after the YES: nobody is subscribed.
    let moved = "+15035552206";
    telnyx(&h, "y-13", moved, &ours, "VIP").await;
    h.drain().await;
    telnyx(&h, "y-14", moved, &ours, "YES").await;
    let pid: String = sqlx::query_scalar("SELECT id::text FROM profiles WHERE phone_number = $1").bind(moved).fetch_one(&h.state.db).await.unwrap();
    h.ok("PATCH", &format!("/api/profiles/{pid}"), Some(json!({"phone_number": "+15035552299"}))).await;
    h.drain().await;
    let (c, n): (String, i64) = sqlx::query_as("SELECT sms_consent, (SELECT count(*) FROM sms_consents WHERE profile_id = p.id) FROM profiles p WHERE id = $1::uuid").bind(&pid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((c.as_str(), n), ("never_subscribed", 0));
    h.cleanup().await;
}

#[tokio::test]
async fn paging_never_skips_or_repeats_items_that_share_a_time() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    for i in 0..3 {
        telnyx(&h, &format!("pg-{i}"), &person(2300 + i), &ours, "hi").await;
    }
    // Three conversations with the same last activity, to the microsecond.
    sqlx::query("UPDATE sms_conversations SET last_message_at = '2026-11-17 18:00:00+00'").execute(&h.state.db).await.unwrap();
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/api/inbox/conversations?limit=1&cursor={}", url::form_urlencoded::byte_serialize(c.as_bytes()).collect::<String>()),
            None => "/api/inbox/conversations?limit=1".to_string(),
        };
        let page = h.ok("GET", &path, None).await;
        seen.extend(page["data"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()));
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!((seen.len(), unique.len()), (3, 3), "{seen:?}");
    let (s, _) = h.req("GET", "/api/inbox/conversations?cursor=nonsense", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A thread whose texts share a time.
    let them = person(2300);
    for i in 0..3 {
        telnyx(&h, &format!("pt-{i}"), &them, &ours, &format!("text {i}")).await;
    }
    sqlx::query("UPDATE sms_inbound SET received_at = '2026-11-17 18:00:00+00' WHERE from_number = $1").bind(&them).execute(&h.state.db).await.unwrap();
    let id = conversation_for(&mut h, &them).await["id"].as_str().unwrap().to_string();
    let mut bodies = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/api/inbox/conversations/{id}?limit=1&cursor={}", url::form_urlencoded::byte_serialize(c.as_bytes()).collect::<String>()),
            None => format!("/api/inbox/conversations/{id}?limit=1"),
        };
        let page = h.ok("GET", &path, None).await;
        bodies.extend(page["messages"].as_array().unwrap().iter().map(|m| m["body"].as_str().unwrap().to_string()));
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    bodies.sort();
    assert_eq!(bodies, vec!["hi", "text 0", "text 1", "text 2"]);
    h.cleanup().await;
}

#[tokio::test]
async fn an_unverified_text_never_picks_which_number_a_reply_goes_from() {
    // No signing keys: texts are kept but unverified.
    let mut h = harness!();
    let first = setup(&mut h).await;
    let default = h.claim_number().await;
    let them = "+15035552401";
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": them, "sms_consent": "subscribed"}))).await;
    assert_eq!(telnyx_post(&h, &telnyx_body("uv-1", them, &first, "is this the shop?", json!({})), false).await.0, StatusCode::OK);
    let c = conversation_for(&mut h, them).await;
    let id = c["id"].as_str().unwrap();
    // They're subscribed, so a reply may go, but from the account's sending number, not the one
    // the unverified text named.
    let dry = h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Yes!"}))).await;
    assert_eq!((dry["to"].as_str(), dry["from"].as_str()), (Some(them), Some(default.as_str())), "{dry}");
    h.ok("POST", &format!("/api/inbox/conversations/{id}/reply"), Some(json!({"body": "Yes!", "confirm": true}))).await;
    let out: Vec<_> = h.state.sms.sent().into_iter().filter(|s| s.to == them).collect();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].from, default);
    h.cleanup().await;
}

/// A number whose own time zone is in the middle of the night now (texts to it are held).
fn nighttime_number(n: u32) -> String {
    let candidates = [
        format!("+1808555{:04}", n), format!("+1503555{:04}", n), format!("+1303555{:04}", n), format!("+1312555{:04}", n), format!("+1212555{:04}", n),
        format!("+4420794{:05}", n), format!("+9198123{:05}", n), format!("+97798123{:04}", n), format!("+8190123{:05}", n),
    ];
    candidates
        .into_iter()
        .find(|c| {
            dumbmail::features::sendtime::zone::phone_zone(c).is_some_and(|tz| {
                let hour = chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&tz));
                !(7..22).contains(&hour)
            })
        })
        .expect("some zone is in the middle of the night")
}

#[tokio::test]
async fn an_inbox_reply_doesnt_hold_the_next_campaign_text_but_quiet_hours_and_stop_still_do() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texters"}))).await;
    let day = daytime_number(1);
    let night = nighttime_number(2);
    let stopped = daytime_number(3);
    for (i, p) in [&day, &night, &stopped].into_iter().enumerate() {
        telnyx(&h, &format!("sm-{i}"), p, &ours, "is my order shipped?").await;
        let c = conversation_for(&mut h, p).await;
        h.ok("POST", &format!("/api/inbox/conversations/{}/reply", c["id"].as_str().unwrap()), Some(json!({"body": "Yes, today!", "confirm": true}))).await;
        sqlx::query("UPDATE profiles SET sms_consent = 'subscribed' WHERE phone_number = $1").bind(p).execute(&h.state.db).await.unwrap();
        h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": [c["profile"]["id"]]}))).await;
    }
    telnyx(&h, "sm-stop", &stopped, &ours, "STOP").await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "New in", "channel": "sms", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"body": "New arrivals are in. Reply STOP to opt out."}, "send_options": {"smart_sending": true, "smart_sending_hours": 24}}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let preview = h.ok("POST", "/api/sendtime/preview", Some(json!({"campaign_id": cid}))).await;
    assert_eq!(preview["smart_sending_skips"], 0, "inbox replies aren't marketing: {preview}");
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert!(texts_to(&h, &day).iter().any(|t| t.contains("New arrivals")), "the reply didn't hold it: {:?}", texts_to(&h, &day));
    assert_eq!(texts_to(&h, &night).len(), 1, "quiet hours in their own zone still hold it: {:?}", texts_to(&h, &night));
    assert_eq!(texts_to(&h, &stopped).len(), 1, "only the reply before the STOP");
    let to_stopped: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND to_address = $2 AND status NOT IN ('skipped')").bind(&cid).bind(&stopped).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(to_stopped, 0, "the STOP left them out");
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND to_address = $2 AND status = 'queued'").bind(&cid).bind(&night).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(held, 1, "held for their morning, not dropped");
    h.cleanup().await;
}

#[tokio::test]
async fn roles_decide_who_reads_replies_and_changes_settings() {
    let mut h = signed!();
    let ours = setup(&mut h).await;
    let them = "+15035550931";
    telnyx(&h, "perm-1", them, &ours, "Do you ship to Canada?").await;
    let cid = h.ok("GET", "/api/inbox/conversations", None).await["data"][0]["id"].as_str().unwrap().to_string();
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE name = 'Acme Goods'").fetch_one(&h.state.db).await.unwrap();
    let reply = format!("/api/inbox/conversations/{cid}/reply");
    let send = json!({"body": "Yes, we do.", "confirm": true});
    async fn role(h: &Harness, account: Uuid, role: &str) {
        sqlx::query("UPDATE memberships SET role = $2 WHERE account_id = $1").bind(account).bind(role).execute(&h.state.db).await.unwrap();
    }

    // Content: no texts (people's data), no replies, no settings changes; settings can be read.
    role(&h, acct, "content").await;
    for (m, p, b) in [
        ("GET", "/api/inbox/conversations".to_string(), None),
        ("GET", format!("/api/inbox/conversations/{cid}"), None),
        ("GET", "/api/inbox/unread".to_string(), None),
        ("POST", reply.clone(), Some(send.clone())),
        ("POST", format!("/api/inbox/conversations/{cid}/status"), Some(json!({"status": "closed"}))),
        ("POST", "/api/inbox/saved-replies".to_string(), Some(json!({"name": "Hours", "body": "9-5"}))),
        ("POST", "/api/inbox/keywords".to_string(), Some(json!({"keyword": "HOURS", "reply": "9-5"}))),
        ("PATCH", "/api/inbox/settings".to_string(), Some(json!({"default_reply_enabled": false}))),
    ] {
        let (s, v) = h.req(m, &p, b).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "content {m} {p}: {v}");
    }
    h.ok("GET", "/api/inbox/settings", None).await;
    h.ok("GET", "/api/inbox/keywords", None).await;

    // Analyst: reads texts and marks them read, but can't reply.
    role(&h, acct, "analyst").await;
    h.ok("GET", &format!("/api/inbox/conversations/{cid}"), None).await;
    h.ok("POST", &format!("/api/inbox/conversations/{cid}/read"), Some(json!({}))).await;
    assert_eq!(h.req("POST", &reply, Some(send.clone())).await.0, StatusCode::FORBIDDEN);
    assert!(h.state.sms.sent().is_empty());

    // Manager: replies and changes inbox settings, but not the provider's signing key.
    role(&h, acct, "manager").await;
    h.ok("PATCH", "/api/inbox/settings", Some(json!({"support_contact": "help@acme.test", "default_reply_enabled": false}))).await;
    let (s, v) = h.req("PATCH", "/api/inbox/settings", Some(json!({"telnyx_public_key": null}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let k = h.ok("POST", "/api/inbox/keywords", Some(json!({"keyword": "HOURS", "reply": "Open 9-5"}))).await;
    h.ok("DELETE", &format!("/api/inbox/keywords/{}", k["id"].as_str().unwrap()), None).await;
    assert_eq!(h.ok("POST", &reply, Some(send.clone())).await["sent"], true);
    // A dry run isn't a send, so it isn't in the audit log.
    h.ok("POST", &reply, Some(json!({"body": "Preview only"}))).await;
    role(&h, acct, "owner").await;

    // The audit log names what happened, never the text.
    let log: Vec<(String, Option<String>, Value)> = sqlx::query_as("SELECT action, target, details FROM audit_log WHERE account_id = $1 AND action LIKE 'sms_%' ORDER BY created_at")
        .bind(acct)
        .fetch_all(&h.state.db)
        .await
        .unwrap();
    let actions: Vec<&str> = log.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(actions, ["sms_inbox.settings_change", "sms_keyword.create", "sms_keyword.delete", "sms_reply.send"], "{log:?}");
    assert_eq!(log[0].2["fields"], json!(["default_reply_enabled", "support_contact"]));
    assert_eq!(log[1].2["word"], "hours");
    assert_eq!(log[3].1.as_deref(), Some(cid.as_str()));
    assert!(log[3].2["message_id"].is_string(), "{:?}", log[3]);
    let all = format!("{log:?}");
    assert!(!all.contains("Yes, we do") && !all.contains("Open 9-5") && !all.contains("help@acme.test"), "{all}");

    // A read-only API key reads the settings but not who's on the team, and can't reply.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let (s, _, body) = h.raw("GET", "/api/inbox/settings", &[("authorization", &auth)], "").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["members"], json!([]), "{v}");
    assert!(!body.contains("owner@acme.test"));
    let (s, _, _) = h.raw("POST", &reply, &[("authorization", &auth), ("content-type", "application/json")], &send.to_string()).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(h.state.sms.sent().len(), 1);
    h.cleanup().await;
}

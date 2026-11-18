//! Customer hub: sign-in by emailed code and signed link, what a shopper sees (only their own
//! data, only server-recorded orders, never sensitive or access fields), preferences written like
//! the preference page, sessions ending on email change, erasure and merge, settings permissions.

#[macro_use]
mod common;
use common::*;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://pl.test";

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

async fn account_key(h: &Harness, name: &str) -> (Uuid, String) {
    sqlx::query_as("SELECT id, public_key FROM accounts WHERE name = $1").bind(name).fetch_one(&h.state.db).await.unwrap()
}

/// Sign up, give the account a name, turn the hub on and add two shoppers.
async fn setup(h: &mut Harness) -> (Uuid, String) {
    h.signup().await;
    h.ok("PATCH", "/api/hub/settings", Some(json!({"enabled": true, "help": [{"question": "Where is my order?", "answer": "Open it below."}], "contact_url": "help@acme.test"}))).await;
    for (email, first) in [("alice@example.com", "Alice"), ("bob@example.com", "Bob")] {
        h.ok("POST", "/api/profiles", Some(json!({"email": email, "first_name": first, "email_consent": "subscribed"}))).await;
    }
    account_key(h, "Acme Goods").await
}

async fn profile_id(h: &Harness, account: Uuid, email: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND email = $2").bind(account).bind(email).fetch_one(&h.state.db).await.unwrap()
}

/// A request to the hub as a shopper's browser makes it (no team session).
async fn shop(h: &Harness, method: &str, path: &str, cookie: Option<&str>, form: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    if method == "POST" {
        b = b.header(header::ORIGIN, ORIGIN).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    }
    let res = h.router.clone().oneshot(b.body(Body::from(form.unwrap_or("").to_string())).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers.get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find(|v| v.starts_with("dm_hub=") && !v.starts_with("dm_hub=;")).map(|v| v.split(';').next().unwrap().to_string())
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The code in the newest sign-in email to `to`.
fn code_sent_to(h: &Harness, to: &str) -> Option<String> {
    let mail = h.state.email.sent().into_iter().rev().find(|e| e.to == to && e.subject.starts_with("Your sign-in code"))?;
    let i = mail.text.find(": ")? + 2;
    Some(mail.text[i..i + 6].to_string())
}

async fn ask_code(h: &Harness, key: &str, email: &str) -> (StatusCode, String) {
    let (s, _, body) = shop(h, "POST", &format!("/hub/{key}/code"), None, Some(&format!("email={}", enc(email)))).await;
    h.drain().await;
    (s, body)
}

async fn try_code(h: &Harness, key: &str, email: &str, code: &str) -> (StatusCode, Option<String>) {
    let (s, headers, _) = shop(h, "POST", &format!("/hub/{key}/verify"), None, Some(&format!("email={}&code={code}", enc(email)))).await;
    (s, session_cookie(&headers))
}

async fn sign_in(h: &Harness, key: &str, email: &str) -> String {
    h.state.email.clear_sent();
    let (s, _) = ask_code(h, key, email).await;
    assert_eq!(s, StatusCode::OK);
    let code = code_sent_to(h, email).expect("a code email");
    let (s, cookie) = try_code(h, key, email, &code).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    cookie.expect("a session cookie")
}

async fn hub_page(h: &Harness, key: &str, cookie: Option<&str>) -> String {
    let (s, _, body) = shop(h, "GET", &format!("/hub/{key}"), cookie, None).await;
    assert!(s.is_success(), "{s}: {body}");
    body
}

fn signed_in(page: &str) -> bool {
    page.contains("Signed in as")
}

fn form_token(page: &str) -> String {
    let i = page.find(r#"name="t" value=""#).expect("a form token") + r#"name="t" value=""#.len();
    page[i..i + 32].to_string()
}

async fn order(h: &mut Harness, email: &str, metric: &str, props: Value, unique: &str) {
    h.ok("POST", "/api/events", Some(json!({"metric": metric, "profile": {"email": email}, "properties": props, "unique_id": unique}))).await;
}

async fn place(h: &mut Harness, email: &str, id: &str, total: f64) {
    let props = json!({"order_id": id, "order_number": id, "total": total, "currency": "USD", "items": [{"name": format!("Thing for {id}"), "quantity": 1, "price": total}]});
    order(h, email, "Placed Order", props, &format!("order:{id}")).await;
}

#[tokio::test]
async fn a_code_is_required_and_expires() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    place(&mut h, "alice@example.com", "A100", 25.0).await;

    // No session: the sign-in page, nothing about anyone.
    let page = hub_page(&h, &key, None).await;
    assert!(page.contains("Email me a code") && !page.contains("A100"));
    // The website snippet's identity token signs nobody in.
    let pl = dumbmail::public::profile_token(&h.state, account, profile_id(&h, account, "alice@example.com").await);
    let (_, _, page) = shop(&h, "GET", &format!("/hub/{key}?_pl={pl}"), None, None).await;
    assert!(!signed_in(&page));

    // A known and an unknown address get the same answer; only the known one gets an email.
    h.state.email.clear_sent();
    let (s1, known) = ask_code(&h, &key, "alice@example.com").await;
    let (s2, unknown) = ask_code(&h, &key, "nobody@example.com").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(known.replace("alice@example.com", "X"), unknown.replace("nobody@example.com", "X"));
    assert!(known.contains("If we have"));
    assert!(code_sent_to(&h, "nobody@example.com").is_none());
    let code = code_sent_to(&h, "alice@example.com").expect("code email");
    let sent = h.state.email.sent();
    let mail = sent.iter().find(|e| e.to == "alice@example.com").unwrap();
    assert!(!mail.subject.contains(&code), "the code is never in the subject (it's stored)");
    assert_eq!(count(&h, "SELECT count(*) FROM hub_codes").await, 2, "a row for both, so limits count both");
    assert_eq!(count(&h, "SELECT count(*) FROM hub_codes WHERE code_hash IS NULL").await, 1);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM hub_codes WHERE code_hash LIKE '%{code}%'")).await, 0, "only a hash is stored");
    // Not billed, and kept as a sent email.
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE subject LIKE 'Your sign-in code%' AND campaign_id IS NULL AND flow_id IS NULL").await, 1);

    // Wrong code, then an expired right one.
    let wrong = if code == "000000" { "111111" } else { "000000" };
    assert_eq!(try_code(&h, &key, "alice@example.com", wrong).await, (StatusCode::BAD_REQUEST, None));
    sqlx::query("UPDATE hub_codes SET expires_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    assert_eq!(try_code(&h, &key, "alice@example.com", &code).await, (StatusCode::BAD_REQUEST, None));
    // A code for an unknown address never works.
    assert_eq!(try_code(&h, &key, "nobody@example.com", "123456").await.1, None);

    // A fresh code signs in, once.
    let cookie = sign_in(&h, &key, "alice@example.com").await;
    let page = hub_page(&h, &key, Some(&cookie)).await;
    assert!(signed_in(&page) && page.contains("A100") && page.contains("Hi, Alice"), "{page}");
    let code = code_sent_to(&h, "alice@example.com").unwrap();
    assert_eq!(try_code(&h, &key, "alice@example.com", &code).await.1, None, "a used code can't be used again");

    // A signed link from an email signs in (and the plain hub address in a test send doesn't).
    let p: dumbmail::models::Profile = sqlx::query_as(&format!("SELECT {} FROM profiles p WHERE p.email = 'bob@example.com'", dumbmail::models::PROFILE_COLS)).fetch_one(&h.state.db).await.unwrap();
    let a = dumbmail::campaigns::load_account(&h.state.db, account).await.unwrap();
    let mut ctx = json!({});
    dumbmail::features::hub::session::fill(&h.state, &a, &p, false, &mut ctx);
    let url = ctx["hub_url"].as_str().unwrap().strip_prefix(ORIGIN).unwrap().to_string();
    let (s, headers, _) = shop(&h, "GET", &url, None, None).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], format!("/hub/{key}"), "the token is dropped from the address");
    let bob = session_cookie(&headers).expect("signed in by the link");
    assert!(hub_page(&h, &key, Some(&bob)).await.contains("Signed in as bob@example.com"));
    let mut test_ctx = json!({});
    dumbmail::features::hub::session::fill(&h.state, &a, &p, true, &mut test_ctx);
    assert_eq!(test_ctx["hub_url"], json!(format!("{ORIGIN}/hub/{key}")), "a test send gets no sign-in link");
    // A tampered link signs nobody in.
    let (s, headers, page) = shop(&h, "GET", &format!("{}x", url), None, None).await;
    assert!(s.is_success() && session_cookie(&headers).is_none() && page.contains("That link has expired"));
    // The hub answers only on its own host.
    let req = Request::builder().uri(format!("/hub/{key}")).header(header::HOST, "elsewhere.test").body(Body::empty()).unwrap();
    assert_eq!(h.router.clone().oneshot(req).await.unwrap().status(), StatusCode::NOT_FOUND);
    // Sign-in posts from another site are refused.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/hub/{key}/code"))
        .header(header::ORIGIN, "https://evil.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("email=alice%40example.com"))
        .unwrap();
    assert_eq!(h.router.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    h.cleanup().await;
}

#[tokio::test]
async fn the_sixth_try_is_refused_even_with_the_right_code_and_requests_are_limited() {
    let mut h = harness!();
    let (_, key) = setup(&mut h).await;
    h.state.email.clear_sent();
    ask_code(&h, &key, "alice@example.com").await;
    let code = code_sent_to(&h, "alice@example.com").unwrap();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        assert_eq!(try_code(&h, &key, "alice@example.com", wrong).await, (StatusCode::BAD_REQUEST, None));
    }
    assert_eq!(try_code(&h, &key, "alice@example.com", &code).await, (StatusCode::BAD_REQUEST, None), "the sixth try fails even with the right code");
    // Five requests an hour for one address (known or not); the sixth is refused.
    for _ in 0..4 {
        assert_eq!(ask_code(&h, &key, "alice@example.com").await.0, StatusCode::OK);
    }
    assert_eq!(ask_code(&h, &key, "alice@example.com").await.0, StatusCode::TOO_MANY_REQUESTS);
    for _ in 0..5 {
        assert_eq!(ask_code(&h, &key, "ghost@example.com").await.0, StatusCode::OK);
    }
    assert_eq!(ask_code(&h, &key, "ghost@example.com").await.0, StatusCode::TOO_MANY_REQUESTS, "unknown addresses are limited the same way");
    // Per visitor: 20 an hour across addresses.
    let from_ip = |email: String| {
        let key = key.clone();
        let router = h.router.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri(format!("/hub/{key}/code"))
                .header(header::ORIGIN, ORIGIN)
                .header("x-real-ip", "203.0.113.9")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("email={}", enc(&email))))
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };
    for i in 0..20 {
        assert_eq!(from_ip(format!("p{i}@example.com")).await, StatusCode::OK);
    }
    assert_eq!(from_ip("p99@example.com".into()).await, StatusCode::TOO_MANY_REQUESTS);
    // Ten at once for one new address: exactly five get in (the limit is checked where the row is written).
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let router = h.router.clone();
            let key = key.clone();
            tokio::spawn(async move {
                let req = Request::builder()
                    .method("POST")
                    .uri(format!("/hub/{key}/code"))
                    .header(header::ORIGIN, ORIGIN)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("email=race%40example.com"))
                    .unwrap();
                router.oneshot(req).await.unwrap().status()
            })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 5);
    let race: i64 = sqlx::query_scalar("SELECT count(*) FROM hub_codes WHERE email_hash = $1")
        .bind(dumbmail::features::hub::session::email_hash(&h.state, account_key(&h, "Acme Goods").await.0, "race@example.com"))
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(race, 5, "never more than five for one address");
    h.cleanup().await;
}

#[tokio::test]
async fn a_session_sees_only_its_own_person_and_account() {
    let mut h = harness!();
    let (acct1, key1) = setup(&mut h).await;
    let owner1 = h.cookie.clone();
    place(&mut h, "alice@example.com", "A100", 25.0).await;
    place(&mut h, "bob@example.com", "B200", 40.0).await;
    // A second store with its own Alice.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "owner@second.test", "password": "correct horse", "company": "Second Shop"}))).await;
    h.ok("PATCH", "/api/account", Some(json!({"from_email": "hello@second.test", "website": "second.test", "address": "2 Main St"}))).await;
    h.ok("PATCH", "/api/hub/settings", Some(json!({"enabled": true}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "alice@example.com", "first_name": "Other Alice"}))).await;
    place(&mut h, "alice@example.com", "X900", 99.0).await;
    let (_, key2) = account_key(&h, "Second Shop").await;
    h.cookie = owner1;

    let alice = sign_in(&h, &key1, "alice@example.com").await;
    let page = hub_page(&h, &key1, Some(&alice)).await;
    assert!(page.contains("A100") && !page.contains("B200") && !page.contains("X900"), "{page}");
    // Bob's order by its id: not hers.
    let (s, _, body) = shop(&h, "GET", &format!("/hub/{key1}/orders/B200"), Some(&alice), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!body.contains("Thing for B200"));
    let (s, _, body) = shop(&h, "GET", &format!("/hub/{key1}/orders/A100"), Some(&alice), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Thing for A100") && body.contains("$25.00"));
    // Her session means nothing at the other store, even for the same address.
    let other = hub_page(&h, &key2, Some(&alice)).await;
    assert!(!signed_in(&other) && !other.contains("X900"));
    let (s, _, _) = shop(&h, "GET", &format!("/hub/{key2}/orders/X900"), Some(&alice), None).await;
    assert_eq!(s, StatusCode::SEE_OTHER, "sent to sign in");
    // Opening the hub and an order are recorded once a day, for her only.
    let opened = count(&h, &format!("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Opened Hub' AND e.account_id = '{acct1}'")).await;
    assert_eq!(opened, 1);
    hub_page(&h, &key1, Some(&alice)).await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Opened Hub'").await, 1, "once a day per session");
    let viewed = count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Viewed Order in Hub' AND e.properties->>'order_id' = 'A100'").await;
    assert_eq!(viewed, 1);
    let _ = acct1;
    h.cleanup().await;
}

#[tokio::test]
async fn browser_orders_never_show_and_hub_events_cant_be_forged() {
    let mut h = harness!();
    let (_, key) = setup(&mut h).await;
    place(&mut h, "alice@example.com", "A100", 60.0).await;
    order(&mut h, "alice@example.com", "Fulfilled Order", json!({"order_id": "A100", "tracking_number": "1Z999", "tracking_url": "https://track.example/1Z999", "carrier": "UPS"}), "ful:A100").await;
    order(&mut h, "alice@example.com", "Delivered", json!({"order_id": "A100", "tracking_number": "1Z999", "courier": "ups"}), "del:A100").await;
    place(&mut h, "alice@example.com", "A101", 30.0).await;
    order(&mut h, "alice@example.com", "Refunded Order", json!({"order_id": "A101", "amount": 30.0}), "ref:A101").await;
    place(&mut h, "alice@example.com", "A102", 30.0).await;
    order(&mut h, "alice@example.com", "Cancelled Order", json!({"order_id": "A102"}), "can:A102").await;
    order(&mut h, "alice@example.com", "Fulfilled Order", json!({"order_id": "A100", "tracking_url": "javascript:alert(1)", "tracking_number": "EVIL"}), "ful2:A100").await;
    // A browser says Alice ordered something (and that an order of hers was delivered).
    let browser = |metric: &str, props: Value| json!({"metric": metric, "profile": {"email": "alice@example.com"}, "properties": props, "unique_id": format!("b:{metric}")}).to_string();
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &browser("Placed Order", json!({"order_id": "FAKE1", "total": 999, "items": [{"name": "Free TV"}]}))).await;
    assert_eq!(s, StatusCode::OK, "the browser event itself is accepted (it's only a browser event)");
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &browser("Delivered", json!({"order_id": "A101"}))).await;
    assert!(s.is_success() || s == StatusCode::BAD_REQUEST);

    let alice = sign_in(&h, &key, "alice@example.com").await;
    let page = hub_page(&h, &key, Some(&alice)).await;
    assert!(!page.contains("FAKE1") && !page.contains("Free TV"), "browser orders never show");
    assert!(page.contains(r#"data-status="delivered""#) && page.contains("Track parcel") && page.contains("https://track.example/1Z999"), "{page}");
    assert!(page.contains(r#"data-status="refunded""#) && page.contains(r#"data-status="cancelled""#));
    assert!(!page.contains("javascript:"), "only web links");

    // Hub events come only from DumbMail.
    let (s, _) = h.req("POST", "/api/events", Some(json!({"metric": "Opened Hub", "profile": {"email": "bob@example.com"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &browser("Viewed Order in Hub", json!({"order_id": "A100"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn sensitive_and_access_fields_never_show() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let fields = json!([
        {"key": "check_in", "label": "Check-in", "type": "date"},
        {"key": "nights", "type": "number"},
        {"key": "door_code", "type": "text", "sensitive": true},
        {"key": "address", "type": "text"},
        {"key": "guest_note", "label": "Wi-Fi password", "type": "text"}
    ]);
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "name": "Reservation", "fields": fields}))).await;
    h.ok(
        "POST",
        "/api/objects",
        Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": "2026-12-01", "nights": 3, "door_code": "4321", "address": "1 Beach Rd", "guest_note": "hunter2"}, "profiles": [{"email": "alice@example.com", "relationship": "guest"}]})),
    )
    .await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R2", "fields": {"nights": 9}, "profiles": [{"email": "bob@example.com"}]}))).await;
    let pick = |kind: &str, f: &[&str]| json!({"object_types": [{"type": "reservation", "kind": kind, "fields": f}]});
    let (s, v) = h.req("PATCH", "/api/hub/settings", Some(pick("other", &["door_code"]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("sensitive"));
    for f in ["address", "guest_note"] {
        let (s, v) = h.req("PATCH", "/api/hub/settings", Some(pick("booking", &[f]))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f}: {v}");
    }
    let v = h.ok("PATCH", "/api/hub/settings", Some(pick("booking", &["check_in", "nights"]))).await;
    let types = v["available_types"].to_string();
    assert!(types.contains(r#""access_like":true"#) && types.contains(r#""sensitive":true"#));

    let alice = sign_in(&h, &key, "alice@example.com").await;
    let page = hub_page(&h, &key, Some(&alice)).await;
    assert!(page.contains("Dec 1, 2026") && page.contains("Nights"), "{page}");
    assert!(!page.contains("4321") && !page.contains("Beach Rd") && !page.contains("hunter2"));
    assert!(!page.contains(">9<"), "Bob's booking isn't hers");
    // Even if the stored settings name the fields (an older save, or a field made sensitive later).
    sqlx::query("UPDATE hub_settings SET config = jsonb_set(config, '{object_types}', $2) WHERE account_id = $1")
        .bind(account)
        .bind(json!([{"type": "reservation", "kind": "booking", "fields": ["door_code", "address", "guest_note", "nights"]}]))
        .execute(&h.state.db)
        .await
        .unwrap();
    let page = hub_page(&h, &key, Some(&alice)).await;
    assert!(page.contains("Nights") && !page.contains("4321") && !page.contains("Beach Rd") && !page.contains("hunter2"), "{page}");
    // The dashboard preview (a made-up person) shows the same fields only.
    let prev = h.ok("GET", "/api/hub/preview", None).await;
    let html = prev["html"].as_str().unwrap();
    assert!(html.contains("Sample reservation") && !html.to_lowercase().contains("door"));
    h.cleanup().await;
}

#[tokio::test]
async fn email_change_erasure_merge_and_sign_out_end_sessions() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "cara@example.com"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "dan@example.com"}))).await;
    let alice = sign_in(&h, &key, "alice@example.com").await;
    let bob = sign_in(&h, &key, "bob@example.com").await;
    let cara = sign_in(&h, &key, "cara@example.com").await;
    let dan = sign_in(&h, &key, "dan@example.com").await;
    for c in [&alice, &bob, &cara, &dan] {
        assert!(signed_in(&hub_page(&h, &key, Some(c)).await));
    }
    // Alice's email changes: her session was for the old address.
    let a = profile_id(&h, account, "alice@example.com").await;
    h.ok("PATCH", &format!("/api/profiles/{a}"), Some(json!({"email": "alice.new@example.com"}))).await;
    assert!(!signed_in(&hub_page(&h, &key, Some(&alice)).await));
    // Bob is erased.
    let b = profile_id(&h, account, "bob@example.com").await;
    h.ok("DELETE", &format!("/api/profiles/{b}"), None).await;
    assert!(!signed_in(&hub_page(&h, &key, Some(&bob)).await));
    assert_eq!(count(&h, &format!("SELECT count(*) FROM hub_sessions WHERE profile_id = '{b}'")).await, 0);
    // Cara is merged (a merge ends both people's sessions).
    let c = profile_id(&h, account, "cara@example.com").await;
    assert_eq!(dumbmail::features::hub::session::end_sessions(&h.state.db, account, &[c]).await.unwrap(), 1);
    assert!(!signed_in(&hub_page(&h, &key, Some(&cara)).await));
    // Dan signs out: that session can't be used again.
    let page = hub_page(&h, &key, Some(&dan)).await;
    let (s, headers, _) = shop(&h, "POST", &format!("/hub/{key}/signout"), Some(&dan), Some("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(headers.get_all(header::SET_COOKIE).iter().any(|v| v.to_str().unwrap().starts_with("dm_hub=;")));
    assert!(!signed_in(&hub_page(&h, &key, Some(&dan)).await));
    let _ = page;
    h.cleanup().await;
}

#[tokio::test]
async fn hub_preferences_write_consent_like_the_preference_page() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "show_on_preferences": true}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let a = profile_id(&h, account, "alice@example.com").await;
    h.ok("POST", &format!("/api/lists/{list_id}/members"), Some(json!({"profile_ids": [a]}))).await;
    sqlx::query("UPDATE profiles SET phone_number = '+15035550100', sms_consent = 'subscribed' WHERE id = $1").bind(a).execute(&h.state.db).await.unwrap();
    let alice = sign_in(&h, &key, "alice@example.com").await;
    let page = hub_page(&h, &key, Some(&alice)).await;
    assert!(page.contains("Newsletter") && page.contains("ending 0100"), "{page}");
    let t = form_token(&page);
    let prefs = format!("/hub/{key}/prefs");

    // Without the session's form token, or from another site: refused.
    let (s, _, _) = shop(&h, "POST", &prefs, Some(&alice), Some("action=email_unsubscribe")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let req = Request::builder()
        .method("POST")
        .uri(&prefs)
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, "https://evil.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("t={t}&action=email_unsubscribe")))
        .unwrap();
    assert_eq!(h.router.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    // Bob's token doesn't work for Alice's session.
    let bob = sign_in(&h, &key, "bob@example.com").await;
    let bob_t = form_token(&hub_page(&h, &key, Some(&bob)).await);
    let (s, _, _) = shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={bob_t}&action=email_unsubscribe"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let consent = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_as::<_, (String, String)>("SELECT email_consent, sms_consent FROM profiles WHERE id = $1").bind(a).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(consent(&h).await.0, "subscribed");

    // Leaving the list: the preference page's own save (Unsubscribed from List, method preferences).
    let (s, _, _) = shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=save_lists"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM list_members WHERE profile_id = '{a}'")).await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed from List' AND e.properties->>'method' = 'preferences'").await, 1);

    // Unsubscribe from all email: the same consent write and event as the unsubscribe page.
    let (s, headers, _) = shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=email_unsubscribe"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().contains("saved=unsubscribed"));
    assert_eq!(consent(&h).await.0, "unsubscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed' AND e.properties->>'method' = 'hub'").await, 1);
    // Again changes nothing and records nothing.
    shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=email_unsubscribe"))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed'").await, 1);
    // Their own opt-in for the address they proved.
    shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=email_subscribe"))).await;
    assert_eq!(consent(&h).await.0, "subscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Subscribed to Email Marketing' AND e.properties->>'method' = 'hub'").await, 1);
    // Texts can be stopped here, never started.
    shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=sms_unsubscribe"))).await;
    assert_eq!(consent(&h).await.1, "unsubscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed from SMS' AND e.properties->>'method' = 'hub'").await, 1);
    let page = hub_page(&h, &key, Some(&alice)).await;
    assert!(!page.contains("sms_subscribe") && page.contains("sign up for texts on our site"));
    // A suppressed address can't be subscribed again from here.
    shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=email_unsubscribe"))).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE id = $1").bind(a).execute(&h.state.db).await.unwrap();
    shop(&h, "POST", &prefs, Some(&alice), Some(&format!("t={t}&action=email_subscribe"))).await;
    assert_eq!(consent(&h).await.0, "unsubscribed");
    h.cleanup().await;
}

#[tokio::test]
async fn a_blocked_or_closed_account_sends_no_code() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    sqlx::query("UPDATE accounts SET sending_paused_at = now(), sending_paused_reason = 'test' WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    h.state.email.clear_sent();
    let (s, body) = ask_code(&h, &key, "alice@example.com").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("If we have"), "the same answer");
    assert!(h.state.email.sent().is_empty(), "no code email while the account can't send");
    assert_eq!(count(&h, "SELECT count(*) FROM hub_codes WHERE code_hash IS NOT NULL").await, 0);
    // A code that was waiting to go out when sending stopped isn't sent either.
    sqlx::query("UPDATE accounts SET sending_paused_at = NULL WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    shop(&h, "POST", &format!("/hub/{key}/code"), None, Some("email=alice%40example.com")).await;
    sqlx::query("UPDATE accounts SET sending_paused_at = now() WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert!(h.state.email.sent().is_empty());
    // A replayed send job never sends twice.
    sqlx::query("UPDATE accounts SET sending_paused_at = NULL WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    shop(&h, "POST", &format!("/hub/{key}/code"), None, Some("email=bob%40example.com")).await;
    h.drain().await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM hub_codes WHERE sent_at IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    dumbmail::features::hub::session::run_send_code(&h.state, &json!({"id": id})).await.unwrap();
    assert_eq!(h.state.email.sent().iter().filter(|e| e.to == "bob@example.com").count(), 1);
    // A closed account has no hub.
    sqlx::query("UPDATE accounts SET closed_at = now() WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    let (s, _, _) = shop(&h, "GET", &format!("/hub/{key}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn back_in_stock_requests_can_be_cancelled_only_by_their_owner() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    h.ok("POST", "/api/catalog/products", Some(json!({"products": [{"id": "SKU1", "title": "Blue Mug", "url": "https://acme.test/mug", "price": 12, "available": false}]}))).await;
    let product: Uuid = sqlx::query_scalar("SELECT id FROM catalog_products WHERE external_id = 'SKU1'").fetch_one(&h.state.db).await.unwrap();
    let mut subs = Vec::new();
    for email in ["alice@example.com", "bob@example.com"] {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO catalog_stock_subscriptions (id, account_id, profile_id, product_id, confirmed_at) VALUES ($1, $2, $3, $4, now())")
            .bind(id)
            .bind(account)
            .bind(profile_id(&h, account, email).await)
            .bind(product)
            .execute(&h.state.db)
            .await
            .unwrap();
        subs.push(id);
    }
    let alice = sign_in(&h, &key, "alice@example.com").await;
    let page = hub_page(&h, &key, Some(&alice)).await;
    assert!(page.contains("Blue Mug") && page.contains("Waiting to come back in stock"), "{page}");
    let t = form_token(&page);
    let (s, _, _) = shop(&h, "POST", &format!("/hub/{key}/stock/{}/cancel", subs[1]), Some(&alice), Some(&format!("t={t}"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "Bob's request isn't hers");
    let (s, _, _) = shop(&h, "POST", &format!("/hub/{key}/stock/{}/cancel", subs[0]), Some(&alice), Some(&format!("t={t}"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(count(&h, "SELECT count(*) FROM catalog_stock_subscriptions").await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn settings_need_permissions_are_checked_and_audited() {
    let mut h = harness!();
    let (_, key) = setup(&mut h).await;
    let v = h.ok("GET", "/api/hub/settings", None).await;
    assert_eq!(v["enabled"], true);
    assert_eq!(v["url"], json!(format!("{ORIGIN}/hub/{key}")));
    assert_eq!(v["embed_site"], Value::Null, "no branded link domain: a new tab");
    assert!(count(&h, "SELECT count(*) FROM audit_log WHERE action = 'hub.enable'").await == 1);
    for bad in [json!({"launcher": "popup"}), json!({"contact_url": "javascript:alert(1)"}), json!({"accent_color": "red;x"}), json!({"object_types": [{"type": "nope"}]}), json!({"surprise": 1})] {
        let (s, _) = h.req("PATCH", "/api/hub/settings", Some(bad.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    let v = h.ok("PATCH", "/api/hub/settings", Some(json!({"launcher": "button", "button_label": "Account", "sections": {"orders": true, "bookings": false, "recommendations": false, "preferences": true, "help": false}}))).await;
    assert_eq!(v["launcher"], "button");
    // The launcher script carries the settings (and nothing when off).
    let (s, _, js) = h.raw("GET", &format!("/client/hub.js?k={key}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(js.contains(r#""launcher":"button""#) && js.contains("/hub/"));
    // An analyst can read the settings but not change them.
    sqlx::query("UPDATE memberships SET role = 'analyst'").execute(&h.state.db).await.unwrap();
    h.ok("GET", "/api/hub/settings", None).await;
    let (s, _) = h.req("PATCH", "/api/hub/settings", Some(json!({"enabled": false}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE memberships SET role = 'owner'").execute(&h.state.db).await.unwrap();
    // MCP.
    let api_key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent", "scopes": ["full"]}))).await["key"].as_str().unwrap().to_string();
    let call = |tool: &str, args: Value| {
        let router = h.router.clone();
        let api_key = api_key.clone();
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}).to_string();
        async move {
            let req = Request::builder().method("POST").uri("/mcp").header(header::AUTHORIZATION, format!("Bearer {api_key}")).header(header::CONTENT_TYPE, "application/json").body(Body::from(body)).unwrap();
            let res = router.oneshot(req).await.unwrap();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    let r = call("update_hub_settings", json!({"enabled": false})).await;
    assert_eq!(r["result"]["isError"], json!(false), "{r}");
    let r = call("get_hub_settings", json!({})).await;
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains(r#""enabled": false"#) || r["result"]["structuredContent"]["enabled"] == json!(false), "{r}");
    assert!(count(&h, "SELECT count(*) FROM audit_log WHERE action = 'hub.disable'").await == 1);
    // Off: the page and the launcher say nothing.
    let (s, _, _) = shop(&h, "GET", &format!("/hub/{key}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, _, js) = h.raw("GET", &format!("/client/hub.js?k={key}"), &[], "").await;
    assert!(js.contains("var C = null;"));
    h.cleanup().await;
}

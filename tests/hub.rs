//! Customer hub: sign-in by emailed code and one-time link, what a shopper sees (only their own
//! data, only server-recorded orders, only for an address the store gave them, never sensitive,
//! unconfirmed free-text or access fields), preferences through the preference page's functions,
//! sessions ending on email change (for good), erasure and merge, framing, the snippet, settings.

#[macro_use]
mod common;
use common::*;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
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

/// Sign up, turn the hub on and add two shoppers (their addresses come from the API).
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

/// A shopper's browser: keeps the hub's cookies and sends posts from the hub's origin.
#[derive(Default, Clone)]
struct Browser {
    cookies: Vec<(String, String)>,
}

impl Browser {
    fn cookie_header(&self) -> String {
        self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    async fn go_with(&mut self, h: &Harness, method: &str, path: &str, form: Option<&str>, extra: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            b = b.header(header::COOKIE, self.cookie_header());
        }
        if method == "POST" {
            b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("origin")) {
                b = b.header(header::ORIGIN, ORIGIN);
            }
        }
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        let res = h.router.clone().oneshot(b.body(Body::from(form.unwrap_or("").to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        for v in headers.get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()) {
            let pair = v.split(';').next().unwrap();
            let (k, val) = pair.split_once('=').unwrap();
            self.cookies.retain(|(n, _)| n != k);
            if !val.is_empty() {
                self.cookies.push((k.to_string(), val.to_string()));
            }
        }
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn go(&mut self, h: &Harness, method: &str, path: &str, form: Option<&str>) -> (StatusCode, HeaderMap, String) {
        self.go_with(h, method, path, form, &[]).await
    }

    async fn page(&mut self, h: &Harness, key: &str) -> String {
        let (s, _, body) = self.go(h, "GET", &format!("/hub/{key}"), None).await;
        assert!(s.is_success(), "{s}: {body}");
        body
    }

    fn has_session(&self) -> bool {
        self.cookies.iter().any(|(k, _)| k == "dm_hub")
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The form token on a page.
fn token(page: &str) -> String {
    let i = page.find(r#"name="t" value=""#).expect("a form token") + r#"name="t" value=""#.len();
    page[i..i + 32].to_string()
}

/// The code in the newest sign-in email to `to`.
fn code_sent_to(h: &Harness, to: &str) -> Option<String> {
    let mail = h.state.email.sent().into_iter().rev().find(|e| e.to == to && e.subject.starts_with("Your sign-in code"))?;
    let i = mail.text.find(": ")? + 2;
    Some(mail.text[i..i + 6].to_string())
}

/// Ask for a code from the sign-in page, as a browser does; runs the send job.
async fn ask_code(h: &Harness, key: &str, b: &mut Browser, email: &str) -> (StatusCode, String) {
    let t = token(&b.page(h, key).await);
    let (s, _, body) = b.go(h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email={}", enc(email)))).await;
    h.drain().await;
    (s, body)
}

async fn try_code(h: &Harness, key: &str, b: &mut Browser, email: &str, code: &str) -> StatusCode {
    let t = token(&b.page(h, key).await);
    b.go(h, "POST", &format!("/hub/{key}/verify"), Some(&format!("t={t}&email={}&code={code}", enc(email)))).await.0
}

async fn sign_in(h: &Harness, key: &str, email: &str) -> Browser {
    let mut b = Browser::default();
    h.state.email.clear_sent();
    let (s, _) = ask_code(h, key, &mut b, email).await;
    assert_eq!(s, StatusCode::OK);
    let code = code_sent_to(h, email).expect("a code email");
    assert_eq!(try_code(h, key, &mut b, email, &code).await, StatusCode::SEE_OTHER);
    assert!(b.has_session());
    b
}

fn signed_in(page: &str) -> bool {
    page.contains("Signed in as")
}

async fn order(h: &mut Harness, who: Value, metric: &str, props: Value, unique: &str) {
    h.ok("POST", "/api/events", Some(json!({"metric": metric, "profile": who, "properties": props, "unique_id": unique}))).await;
}

async fn place(h: &mut Harness, email: &str, id: &str, total: f64) {
    let props = json!({"order_id": id, "order_number": id, "total": total, "currency": "USD", "items": [{"name": format!("Thing for {id}"), "quantity": 1, "price": total}]});
    order(h, json!({"email": email}), "Placed Order", props, &format!("order:{id}")).await;
}

fn wrong_code(code: &str) -> &'static str {
    if code == "000000" { "111111" } else { "000000" }
}

#[tokio::test]
async fn a_code_is_required_and_expires() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    place(&mut h, "alice@example.com", "A100", 25.0).await;

    // No session: the sign-in page, nothing about anyone.
    let mut b = Browser::default();
    let page = b.page(&h, &key).await;
    assert!(page.contains("Email me a code") && !page.contains("A100"));
    // The website snippet's identity token signs nobody in.
    let pl = dumbmail::public::profile_token(&h.state, account, profile_id(&h, account, "alice@example.com").await);
    let (_, _, page) = b.go(&h, "GET", &format!("/hub/{key}?_pl={pl}"), None).await;
    assert!(!signed_in(&page));

    // A known and an unknown address get the same answer; only the known one gets an email.
    h.state.email.clear_sent();
    let (s1, known) = ask_code(&h, &key, &mut b, "alice@example.com").await;
    let (s2, unknown) = ask_code(&h, &key, &mut b, "nobody@example.com").await;
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

    // Wrong code, then an expired right one.
    assert_eq!(try_code(&h, &key, &mut b, "alice@example.com", wrong_code(&code)).await, StatusCode::BAD_REQUEST);
    sqlx::query("UPDATE hub_codes SET expires_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    assert_eq!(try_code(&h, &key, &mut b, "alice@example.com", &code).await, StatusCode::BAD_REQUEST);
    assert_eq!(try_code(&h, &key, &mut b, "nobody@example.com", "123456").await, StatusCode::BAD_REQUEST, "a code for an unknown address never works");
    assert!(!b.has_session());

    // A fresh code signs in, once.
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    let page = alice.page(&h, &key).await;
    assert!(signed_in(&page) && page.contains("A100") && page.contains("Hi, Alice"), "{page}");
    let code = code_sent_to(&h, "alice@example.com").unwrap();
    let mut other = Browser::default();
    assert_eq!(try_code(&h, &key, &mut other, "alice@example.com", &code).await, StatusCode::BAD_REQUEST, "a used code can't be used again");

    // The hub answers only on its own host.
    let req = Request::builder().uri(format!("/hub/{key}")).header(header::HOST, "elsewhere.test").body(Body::empty()).unwrap();
    assert_eq!(h.router.clone().oneshot(req).await.unwrap().status(), StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn the_sixth_try_is_refused_even_with_the_right_code_and_requests_are_limited() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let mut b = Browser::default();
    h.state.email.clear_sent();
    ask_code(&h, &key, &mut b, "alice@example.com").await;
    let code = code_sent_to(&h, "alice@example.com").unwrap();
    for _ in 0..5 {
        assert_eq!(try_code(&h, &key, &mut b, "alice@example.com", wrong_code(&code)).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(try_code(&h, &key, &mut b, "alice@example.com", &code).await, StatusCode::BAD_REQUEST, "the sixth try fails even with the right code");
    // Five requests an hour for one address (known or not); the sixth is refused.
    for _ in 0..4 {
        assert_eq!(ask_code(&h, &key, &mut b, "alice@example.com").await.0, StatusCode::OK);
    }
    assert_eq!(ask_code(&h, &key, &mut b, "alice@example.com").await.0, StatusCode::TOO_MANY_REQUESTS);
    for _ in 0..5 {
        assert_eq!(ask_code(&h, &key, &mut b, "ghost@example.com").await.0, StatusCode::OK);
    }
    assert_eq!(ask_code(&h, &key, &mut b, "ghost@example.com").await.0, StatusCode::TOO_MANY_REQUESTS, "unknown addresses are limited the same way");
    // Per visitor: 20 an hour across addresses.
    let t = token(&b.page(&h, &key).await);
    for i in 0..20 {
        let (s, _, _) = b.go_with(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=p{i}%40example.com")), &[("x-real-ip", "203.0.113.9")]).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _, _) = b.go_with(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=p99%40example.com")), &[("x-real-ip", "203.0.113.9")]).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    // Ten at once for one new address: exactly five get in (the limit is checked where the row is written).
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let mut b = b.clone();
            let router_h = h.router.clone();
            let (key, t) = (key.clone(), t.clone());
            tokio::spawn(async move {
                let req = Request::builder()
                    .method("POST")
                    .uri(format!("/hub/{key}/code"))
                    .header(header::ORIGIN, ORIGIN)
                    .header(header::COOKIE, b.cookie_header())
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("t={t}&email=race%40example.com")))
                    .unwrap();
                b.cookies.clear();
                router_h.oneshot(req).await.unwrap().status()
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
        .bind(dumbmail::features::hub::session::email_hash(&h.state, account, "race@example.com"))
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(race, 5, "never more than five for one address");
    h.cleanup().await;
}

/// Item 8: only the newest code counts, and one statement counts the try and uses the code.
#[tokio::test]
async fn only_the_newest_code_counts_and_a_code_is_used_once() {
    let mut h = harness!();
    let (_, key) = setup(&mut h).await;
    let mut b = Browser::default();
    h.state.email.clear_sent();
    ask_code(&h, &key, &mut b, "alice@example.com").await;
    let first = code_sent_to(&h, "alice@example.com").unwrap();
    h.state.email.clear_sent();
    ask_code(&h, &key, &mut b, "alice@example.com").await;
    let second = code_sent_to(&h, "alice@example.com").unwrap();
    if first != second {
        assert_eq!(try_code(&h, &key, &mut b, "alice@example.com", &first).await, StatusCode::BAD_REQUEST, "an older code doesn't work once a newer one is out");
    }
    // Ten tries at once with the right code: exactly one session.
    let t = token(&b.page(&h, &key).await);
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let (router, key, t, cookie, code) = (h.router.clone(), key.clone(), t.clone(), b.cookie_header(), second.clone());
            tokio::spawn(async move {
                let req = Request::builder()
                    .method("POST")
                    .uri(format!("/hub/{key}/verify"))
                    .header(header::ORIGIN, ORIGIN)
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("t={t}&email=alice%40example.com&code={code}")))
                    .unwrap();
                router.oneshot(req).await.unwrap().status()
            })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::SEE_OTHER {
            ok += 1;
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM hub_sessions").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM hub_codes WHERE used_at IS NOT NULL").await, 1);
    h.cleanup().await;
}

/// Item 1: a verified inbox opens only orders the store tied to that address.
#[tokio::test]
async fn an_address_filled_in_by_a_browser_opens_nothing_personal() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    // The store's server knows Pat by phone only, and records a real order.
    h.ok("POST", "/api/profiles", Some(json!({"phone_number": "+15035550177", "first_name": "Pat"}))).await;
    order(&mut h, json!({"phone_number": "+15035550177"}), "Placed Order", json!({"order_id": "P900", "total": 500, "items": [{"name": "Secret purchase"}]}), "order:P900").await;
    // A browser fills an email onto that profile (the snippet's identify, or a form).
    let body = json!({"metric": "Viewed Product", "profile": {"phone_number": "+15035550177", "email": "mallory@example.com"}, "properties": {}}).to_string();
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK);
    let pat: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = '+15035550177'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(profile_id(&h, account, "mallory@example.com").await, pat, "the browser filled the email in");
    // Proving that inbox shows only preferences, and a plain note.
    let mut m = sign_in(&h, &key, "mallory@example.com").await;
    let page = m.page(&h, &key).await;
    assert!(signed_in(&page) && page.contains("Email and texts") && page.contains("We can't show orders"), "{page}");
    assert!(!page.contains("P900") && !page.contains("Secret purchase"));
    let (s, _, body) = m.go(&h, "GET", &format!("/hub/{key}/orders/P900"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!body.contains("Secret purchase"));
    // Once the store's own systems give that address, the order shows.
    h.ok("PATCH", &format!("/api/profiles/{pat}"), Some(json!({"email": "mallory@example.com"}))).await;
    let mut m = sign_in(&h, &key, "mallory@example.com").await;
    assert!(m.page(&h, &key).await.contains("P900"));
    // Shopify- and integration-given addresses count too; one the dashboard or API set does.
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'alice@example.com' AND email_source = 'api'").await, 1);
    h.cleanup().await;
}

/// Item 2: sign-in links only in email, never click-wrapped, used once, by a button press.
#[tokio::test]
async fn sign_in_links_are_for_email_only_and_work_once_by_a_button() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    h.credit(5.0).await;
    let a = dumbmail::campaigns::load_account(&h.state.db, account).await.unwrap();
    let p: dumbmail::models::Profile = sqlx::query_as(&format!("SELECT {} FROM profiles p WHERE p.email = 'bob@example.com'", dumbmail::models::PROFILE_COLS)).fetch_one(&h.state.db).await.unwrap();
    let plain = format!("{ORIGIN}/hub/{key}");
    for (channel, test) in [("sms", false), ("whatsapp", false), ("push", false), ("email", true)] {
        let mut ctx = json!({});
        dumbmail::features::hub::session::fill(&h.state, &a, &p, channel, test, &mut ctx);
        assert_eq!(ctx["hub_url"], json!(plain), "{channel} (test: {test}) gets no sign-in link");
    }
    // A real campaign email: the link goes straight to the hub (never through click tracking).
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [p.id]}))).await;
    let content = json!({"subject": "Your account", "blocks": [{"type": "text", "html": "<p><a href=\"{{ hub_url }}\">My account</a> <a href=\"https://acme.test/shop\">Shop</a></p>"}]});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Hub", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "bob@example.com" && m.subject == "Your account").expect("campaign email");
    let html = mail.html.replace("&#x2f;", "/").replace("&#x2F;", "/").replace("&#x3d;", "=").replace("&#61;", "=").replace("&amp;", "&");
    let start = html.find(&format!("{plain}?t=")).unwrap_or_else(|| panic!("a sign-in link, unwrapped: {html}"));
    let link: String = html[start..].split('"').next().unwrap().to_string();
    assert!(mail.html.contains("/t/c/"), "other links are still tracked");
    assert_eq!(count(&h, "SELECT count(*) FROM events WHERE properties::text LIKE '%/hub/%'").await, 0);

    // Opening it (as a mail scanner would) signs nobody in and doesn't use it up.
    let path = link.strip_prefix(ORIGIN).unwrap().to_string();
    let mut scanner = Browser::default();
    for _ in 0..2 {
        let (s, _, page) = scanner.go(&h, "GET", &path, None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(page.contains("Continue to your account") && !scanner.has_session(), "{page}");
    }
    // The button signs in, once.
    let mut bob = Browser::default();
    let (_, _, page) = bob.go(&h, "GET", &path, None).await;
    let t = token(&page);
    let tok = path.split("t=").nth(1).unwrap().to_string();
    let (s, _, _) = bob.go(&h, "POST", &format!("/hub/{key}/link"), Some(&format!("t={t}&link={}", enc(&tok)))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(bob.has_session() && signed_in(&bob.page(&h, &key).await));
    let mut again = Browser::default();
    let (_, _, page) = again.go(&h, "GET", &path, None).await;
    assert!(page.contains("expired or was already used") && !page.contains("Continue to your account"), "a used link falls back to the code sign-in");
    let t = token(&again.page(&h, &key).await);
    let (s, _, _) = again.go(&h, "POST", &format!("/hub/{key}/link"), Some(&format!("t={t}&link={}", enc(&tok)))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!again.has_session());
    // An expired link (made 73 hours ago) doesn't work either.
    let old = dumbmail::features::hub::session::link_token(&h.state, account, p.id, "bob@example.com", chrono::Utc::now() - chrono::Duration::hours(73));
    let (_, _, page) = again.go(&h, "GET", &format!("/hub/{key}?t={old}"), None).await;
    assert!(page.contains("expired or was already used"));
    h.cleanup().await;
}

/// Item 3: changing the address ends access for good (A → B → A revives nothing).
#[tokio::test]
async fn an_address_changed_away_and_back_revives_no_session_link_or_code() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let a = profile_id(&h, account, "alice@example.com").await;
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    assert!(signed_in(&alice.page(&h, &key).await));
    let link = dumbmail::features::hub::session::link_token(&h.state, account, a, "alice@example.com", chrono::Utc::now() - chrono::Duration::seconds(5));
    let mut pending = Browser::default();
    h.state.email.clear_sent();
    ask_code(&h, &key, &mut pending, "alice@example.com").await;
    let code = code_sent_to(&h, "alice@example.com").unwrap();
    // A → B → A through the API.
    h.ok("PATCH", &format!("/api/profiles/{a}"), Some(json!({"email": "alice.b@example.com"}))).await;
    h.ok("PATCH", &format!("/api/profiles/{a}"), Some(json!({"email": "alice@example.com"}))).await;
    assert!(!signed_in(&alice.page(&h, &key).await), "the old cookie stays dead");
    assert_eq!(try_code(&h, &key, &mut pending, "alice@example.com", &code).await, StatusCode::BAD_REQUEST, "and the code sent before");
    let (_, _, page) = pending.go(&h, "GET", &format!("/hub/{key}?t={link}"), None).await;
    assert!(page.contains("expired or was already used"), "and the link sent before");
    // An event matched by id that brings a new address does the same (audience::upsert_inner).
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Viewed", "profile": {"id": a, "email": "alice.c@example.com"}}))).await;
    h.ok("PATCH", &format!("/api/profiles/{a}"), Some(json!({"email": "alice@example.com"}))).await;
    assert!(!signed_in(&alice.page(&h, &key).await));
    // The address is still the store's after moving back (a changed source is marked again).
    assert_eq!(count(&h, &format!("SELECT count(*) FROM profiles WHERE id = '{a}' AND email_source = 'api'")).await, 1);
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

    let mut alice = sign_in(&h, &key1, "alice@example.com").await;
    let page = alice.page(&h, &key1).await;
    assert!(page.contains("A100") && !page.contains("B200") && !page.contains("X900"), "{page}");
    let (s, _, body) = alice.go(&h, "GET", &format!("/hub/{key1}/orders/B200"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!body.contains("Thing for B200"));
    let (s, _, body) = alice.go(&h, "GET", &format!("/hub/{key1}/orders/A100"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains("Thing for A100") && body.contains("$25.00"));
    // Her session means nothing at the other store, even for the same address.
    let other = alice.page(&h, &key2).await;
    assert!(!signed_in(&other) && !other.contains("X900"));
    let (s, _, _) = alice.go(&h, "GET", &format!("/hub/{key2}/orders/X900"), None).await;
    assert_eq!(s, StatusCode::SEE_OTHER, "sent to sign in");
    assert_eq!(count(&h, &format!("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Opened Hub' AND e.account_id = '{acct1}'")).await, 1);
    alice.page(&h, &key1).await;
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Opened Hub'").await, 1, "once a day per session");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Viewed Order in Hub' AND e.properties->>'order_id' = 'A100'").await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn browser_orders_never_show_and_parcels_attach_to_the_right_order() {
    let mut h = harness!();
    let (_, key) = setup(&mut h).await;
    let alice_who = json!({"email": "alice@example.com"});
    place(&mut h, "alice@example.com", "A100", 60.0).await;
    order(&mut h, alice_who.clone(), "Fulfilled Order", json!({"order_id": "A100", "tracking_number": "1Z999", "tracking_url": "https://track.example/1Z999", "carrier": "UPS"}), "ful:A100").await;
    order(&mut h, alice_who.clone(), "Delivered", json!({"order_id": "A100", "tracking_number": "1Z999", "courier": "ups"}), "del:A100").await;
    place(&mut h, "alice@example.com", "A101", 30.0).await;
    order(&mut h, alice_who.clone(), "Refunded Order", json!({"order_id": "A101", "amount": 30.0}), "ref:A101").await;
    place(&mut h, "alice@example.com", "A102", 30.0).await;
    order(&mut h, alice_who.clone(), "Cancelled Order", json!({"order_id": "A102"}), "can:A102").await;
    order(&mut h, alice_who.clone(), "Fulfilled Order", json!({"order_id": "A100", "tracking_url": "javascript:alert(1)", "tracking_number": "EVIL"}), "ful2:A100").await;
    // Item 9: a parcel that names another order id never attaches by display number, and a
    // malformed date (multibyte where the day ends) counts as missing.
    order(&mut h, alice_who.clone(), "Shipment In Transit", json!({"order_id": "OTHER", "order_number": "A101", "tracking_number": "WRONG1"}), "ship:other").await;
    order(&mut h, alice_who.clone(), "Shipment In Transit", json!({"order_number": "A102", "tracking_number": "BYNUM", "expected_delivery": "2026-11-0éé"}), "ship:bynum").await;
    // A browser says Alice ordered something.
    let browser = |metric: &str, props: Value| json!({"metric": metric, "profile": {"email": "alice@example.com"}, "properties": props, "unique_id": format!("b:{metric}")}).to_string();
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &browser("Placed Order", json!({"order_id": "FAKE1", "total": 999, "items": [{"name": "Free TV"}]}))).await;
    assert_eq!(s, StatusCode::OK);

    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    let page = alice.page(&h, &key).await;
    assert!(!page.contains("FAKE1") && !page.contains("Free TV"), "browser orders never show");
    assert!(page.contains(r#"data-status="delivered""#) && page.contains("Track parcel") && page.contains("https://track.example/1Z999"), "{page}");
    assert!(page.contains(r#"data-status="refunded""#) && page.contains(r#"data-status="cancelled""#));
    assert!(!page.contains("javascript:"), "only web links");
    assert!(!page.contains("WRONG1"), "a parcel for another order id isn't attached by number");
    assert!(page.contains("BYNUM") && !page.contains("Expected"), "{page}");

    // Hub events come only from DumbMail.
    let (s, _) = h.req("POST", "/api/events", Some(json!({"metric": "Opened Hub", "profile": {"email": "bob@example.com"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &browser("Viewed Order in Hub", json!({"order_id": "A100"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

/// Item 4: only fields the store cleared show.
#[tokio::test]
async fn only_cleared_fields_show_and_a_changed_field_needs_clearing_again() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let fields = json!([
        {"key": "check_in", "label": "Check-in", "type": "date"},
        {"key": "nights", "type": "number"},
        {"key": "plan", "label": "Plan", "type": "text"},
        {"key": "door_code", "type": "text", "sensitive": true},
        {"key": "address", "type": "text"},
        {"key": "guest_note", "label": "Wi-Fi password", "type": "text"}
    ]);
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "name": "Reservation", "fields": fields, "title_field": "plan"}))).await;
    h.ok(
        "POST",
        "/api/objects",
        Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": "2026-12-01", "nights": 3, "plan": "Ocean suite", "door_code": "4321", "address": "1 Beach Rd", "guest_note": "hunter2"}, "profiles": [{"email": "alice@example.com", "relationship": "guest"}]})),
    )
    .await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R2", "fields": {"nights": 9}, "profiles": [{"email": "bob@example.com"}]}))).await;
    let pick = |kind: &str, f: &[&str], cleared: &[&str]| json!({"object_types": [{"type": "reservation", "kind": kind, "fields": f, "cleared": cleared}]});
    let (s, v) = h.req("PATCH", "/api/hub/settings", Some(pick("other", &["door_code"], &["door_code"]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("sensitive"));
    for f in ["address", "guest_note"] {
        let (s, v) = h.req("PATCH", "/api/hub/settings", Some(pick("booking", &[f], &[f]))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f}: {v}");
    }
    // Free text needs the store's confirmation, whatever the kind of record.
    let (s, v) = h.req("PATCH", "/api/hub/settings", Some(pick("other", &["address"], &[]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("free text"));
    // Numbers and dates show when chosen; the title (free text) only when chosen and cleared.
    let v = h.ok("PATCH", "/api/hub/settings", Some(pick("booking", &["check_in", "nights"], &[]))).await;
    assert!(v["available_types"].to_string().contains(r#""free_text":true"#));
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    let page = alice.page(&h, &key).await;
    assert!(page.contains("Dec 1, 2026") && page.contains("Nights"), "{page}");
    assert!(!page.contains("Ocean suite"), "the title field shows only when chosen");
    assert!(!page.contains("4321") && !page.contains("Beach Rd") && !page.contains("hunter2") && !page.contains(">9<"));
    let v = h.ok("PATCH", "/api/hub/settings", Some(pick("booking", &["plan", "nights"], &["plan"]))).await;
    assert_eq!(v["object_types"][0]["cleared"], json!(["plan"]));
    assert!(v["object_types"][0].get("fingerprints").is_none(), "fingerprints stay on the server");
    assert!(alice.page(&h, &key).await.contains("Ocean suite"));
    // The field's definition changes: the confirmation lapses (page and settings).
    let mut fields2 = fields.clone();
    fields2[2]["label"] = json!("Plan and notes");
    h.ok("PATCH", "/api/object-types/reservation", Some(json!({"fields": fields2}))).await;
    assert!(!alice.page(&h, &key).await.contains("Ocean suite"));
    assert_eq!(h.ok("GET", "/api/hub/settings", None).await["object_types"][0]["cleared"], json!([]));
    // Even settings forced into the database show nothing sensitive, uncleared or access-like.
    sqlx::query("UPDATE hub_settings SET config = jsonb_set(config, '{object_types}', $2) WHERE account_id = $1")
        .bind(account)
        .bind(json!([{"type": "reservation", "kind": "booking", "fields": ["door_code", "address", "guest_note", "nights"], "cleared": ["door_code", "address", "guest_note"], "fingerprints": {"door_code": "x", "address": "y", "guest_note": "z"}}]))
        .execute(&h.state.db)
        .await
        .unwrap();
    let page = alice.page(&h, &key).await;
    assert!(page.contains("Nights") && !page.contains("4321") && !page.contains("Beach Rd") && !page.contains("hunter2"), "{page}");
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
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    let mut bob = sign_in(&h, &key, "bob@example.com").await;
    let mut cara = sign_in(&h, &key, "cara@example.com").await;
    let mut dan = sign_in(&h, &key, "dan@example.com").await;
    let mut dan2 = sign_in(&h, &key, "dan@example.com").await;
    for b in [&mut alice, &mut bob, &mut cara, &mut dan, &mut dan2] {
        assert!(signed_in(&b.page(&h, &key).await));
    }
    let a = profile_id(&h, account, "alice@example.com").await;
    h.ok("PATCH", &format!("/api/profiles/{a}"), Some(json!({"email": "alice.new@example.com"}))).await;
    assert!(!signed_in(&alice.page(&h, &key).await));
    let b = profile_id(&h, account, "bob@example.com").await;
    h.ok("DELETE", &format!("/api/profiles/{b}"), None).await;
    assert!(!signed_in(&bob.page(&h, &key).await));
    assert_eq!(count(&h, &format!("SELECT count(*) FROM hub_sessions WHERE profile_id = '{b}'")).await, 0);
    // A merge calls end_sessions for both people.
    let c = profile_id(&h, account, "cara@example.com").await;
    dumbmail::features::hub::end_sessions(&h.state.db, c).await.unwrap();
    assert!(!signed_in(&cara.page(&h, &key).await));
    // Sign out: this browser only. Sign out everywhere: every browser.
    let t = token(&dan.page(&h, &key).await);
    let (s, _, _) = dan.go(&h, "POST", &format!("/hub/{key}/signout"), Some(&format!("t={t}"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(!dan.has_session() && !signed_in(&dan.page(&h, &key).await));
    let mut dan3 = sign_in(&h, &key, "dan@example.com").await;
    assert!(signed_in(&dan2.page(&h, &key).await));
    let t = token(&dan3.page(&h, &key).await);
    let (s, _, _) = dan3.go(&h, "POST", &format!("/hub/{key}/signout"), Some(&format!("t={t}&everywhere=1"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(!signed_in(&dan2.page(&h, &key).await), "everywhere ends the other browser's session too");
    h.cleanup().await;
}

/// Item 10: preferences through the preference page's functions; the page says what happened.
#[tokio::test]
async fn hub_preferences_use_the_preference_pages_functions() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "show_on_preferences": true}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    let a = profile_id(&h, account, "alice@example.com").await;
    h.ok("POST", &format!("/api/lists/{list_id}/members"), Some(json!({"profile_ids": [a]}))).await;
    sqlx::query("UPDATE profiles SET phone_number = '+15035550100', sms_consent = 'subscribed' WHERE id = $1").bind(a).execute(&h.state.db).await.unwrap();
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    let page = alice.page(&h, &key).await;
    assert!(page.contains("Newsletter") && page.contains("ending 0100"), "{page}");
    let t = token(&page);
    let prefs = format!("/hub/{key}/prefs");

    // Without the session's form token, with another session's, or from another site: refused.
    let (s, _, _) = alice.go(&h, "POST", &prefs, Some("action=email_unsubscribe")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = alice.go_with(&h, "POST", &prefs, Some(&format!("t={t}&action=email_unsubscribe")), &[("origin", "https://evil.test")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = alice.go_with(&h, "POST", &prefs, Some(&format!("t={t}&action=email_unsubscribe")), &[("origin", "https://pl.test:8443")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the exact origin, port included");
    let mut bob = sign_in(&h, &key, "bob@example.com").await;
    let bob_t = token(&bob.page(&h, &key).await);
    let (s, _, _) = alice.go(&h, "POST", &prefs, Some(&format!("t={bob_t}&action=email_unsubscribe"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let consent = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_as::<_, (String, String)>("SELECT email_consent, sms_consent FROM profiles WHERE id = $1").bind(a).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(consent(&h).await.0, "subscribed");

    // Leaving the list: the preference page's own save.
    let (s, _, _) = alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=save_lists"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM list_members WHERE profile_id = '{a}'")).await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed from List' AND e.properties->>'method' = 'preferences'").await, 1);

    // Unsubscribe from all email: preferences::opt_out, with source hub.
    let (s, headers, _) = alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=email_unsubscribe"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(headers[header::LOCATION].to_str().unwrap().contains("saved=unsubscribed"));
    assert_eq!(consent(&h).await.0, "unsubscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed' AND e.properties = '{\"method\": \"hub\"}'::jsonb").await, 1);
    // Again changes nothing, records nothing, and says so.
    let (_, headers, _) = alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=email_unsubscribe"))).await;
    assert!(headers[header::LOCATION].to_str().unwrap().contains("saved=nochange"));
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed'").await, 1);
    // Their own opt-in for the address they proved.
    alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=email_subscribe"))).await;
    assert_eq!(consent(&h).await.0, "subscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Subscribed to Email Marketing' AND e.properties->>'method' = 'hub'").await, 1);
    // Texts can be stopped here, never started.
    alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=sms_unsubscribe"))).await;
    assert_eq!(consent(&h).await.1, "unsubscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Unsubscribed from SMS' AND e.properties->>'method' = 'hub'").await, 1);
    let page = alice.page(&h, &key).await;
    assert!(!page.contains("sms_subscribe") && page.contains("sign up for texts on our site"));
    // A suppressed address can't be subscribed again, and the page says nothing was changed.
    alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=email_unsubscribe"))).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE id = $1").bind(a).execute(&h.state.db).await.unwrap();
    let (_, headers, _) = alice.go(&h, "POST", &prefs, Some(&format!("t={t}&action=email_subscribe"))).await;
    let to = headers[header::LOCATION].to_str().unwrap().to_string();
    assert!(to.contains("saved=suppressed"), "{to}");
    let (_, _, page) = alice.go(&h, "GET", &to, None).await;
    assert!(page.contains("Nothing was changed") && !page.contains("You'll get our emails"), "{page}");
    assert_eq!(consent(&h).await.0, "unsubscribed");
    let _ = bob;
    h.cleanup().await;
}

/// Items 6 and 11: the code mail checks again just before it goes, and is recorded as transactional.
#[tokio::test]
async fn code_mail_checks_again_before_it_goes_and_is_transactional() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let mut b = Browser::default();
    h.state.email.clear_sent();
    // Two codes asked for before the jobs run: only the newest goes out.
    let t = token(&b.page(&h, &key).await);
    for _ in 0..2 {
        b.go(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=alice%40example.com"))).await;
    }
    h.drain().await;
    assert_eq!(h.state.email.sent().iter().filter(|e| e.to == "alice@example.com").count(), 1, "an older code waiting to go isn't sent");
    assert_eq!(count(&h, "SELECT count(*) FROM messages WHERE subject LIKE 'Your sign-in code%' AND transactional").await, 1);
    // The hub turned off, the address suppressed, or sending paused while a code waits: nothing goes.
    for (stop, undo) in [
        ("UPDATE hub_settings SET enabled = false", "UPDATE hub_settings SET enabled = true"),
        ("UPDATE profiles SET email_suppressed_reason = 'complaint' WHERE email = 'alice@example.com'", "UPDATE profiles SET email_suppressed_reason = NULL"),
        ("UPDATE accounts SET sending_paused_at = now()", "UPDATE accounts SET sending_paused_at = NULL"),
    ] {
        h.state.email.clear_sent();
        let t = token(&b.page(&h, &key).await);
        sqlx::query("DELETE FROM hub_codes").execute(&h.state.db).await.unwrap();
        b.go(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=alice%40example.com"))).await;
        sqlx::query(stop).execute(&h.state.db).await.unwrap();
        h.drain().await;
        assert!(h.state.email.sent().is_empty(), "{stop}");
        sqlx::query(undo).execute(&h.state.db).await.unwrap();
    }
    // A replayed send job never sends twice.
    h.state.email.clear_sent();
    sqlx::query("DELETE FROM hub_codes").execute(&h.state.db).await.unwrap();
    let t = token(&b.page(&h, &key).await);
    b.go(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=bob%40example.com"))).await;
    h.drain().await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM hub_codes WHERE sent_at IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    dumbmail::features::hub::session::run_send_code(&h.state, &json!({"id": id})).await.unwrap();
    assert_eq!(h.state.email.sent().iter().filter(|e| e.to == "bob@example.com").count(), 1);
    // A paused account writes no code at all; a closed one has no hub.
    sqlx::query("UPDATE accounts SET sending_paused_at = now() WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    h.state.email.clear_sent();
    let (s, body) = ask_code(&h, &key, &mut b, "alice@example.com").await;
    assert!(s == StatusCode::OK && body.contains("If we have") && h.state.email.sent().is_empty());
    sqlx::query("UPDATE accounts SET closed_at = now() WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    let (s, _, _) = b.go(&h, "GET", &format!("/hub/{key}"), None).await;
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
    let mut alice = sign_in(&h, &key, "alice@example.com").await;
    let page = alice.page(&h, &key).await;
    assert!(page.contains("Blue Mug") && page.contains("Waiting to come back in stock"), "{page}");
    let t = token(&page);
    let (s, _, _) = alice.go(&h, "POST", &format!("/hub/{key}/stock/{}/cancel", subs[1]), Some(&format!("t={t}"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "Bob's request isn't hers");
    let (s, _, _) = alice.go(&h, "POST", &format!("/hub/{key}/stock/{}/cancel", subs[0]), Some(&format!("t={t}"))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(count(&h, "SELECT count(*) FROM catalog_stock_subscriptions").await, 1);
    h.cleanup().await;
}

/// Items 5, 7 and 12: framing, the snippet's config and unknown hub paths.
#[tokio::test]
async fn framing_only_by_approved_origins_the_snippet_and_unknown_paths() {
    let mut h = harness!();
    let (account, key) = setup(&mut h).await;
    let csp = |headers: &HeaderMap| headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
    let mut b = Browser::default();
    // No branded link domain: never framed, panel page or not.
    for path in [format!("/hub/{key}"), format!("/hub/{key}?embed=1")] {
        let (_, headers, _) = b.go(&h, "GET", &path, None).await;
        assert!(csp(&headers).contains("frame-ancestors 'none'"), "{path}");
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin");
    }
    // A live branded link domain under the store's site: the panel page may be framed only by the
    // website's exact origin (or the origins the store approves); other pages never.
    sqlx::query("UPDATE accounts SET link_domain = 'links.acme.test', link_domain_status = 'active' WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    let (_, headers, _) = b.go(&h, "GET", &format!("/hub/{key}?embed=1"), None).await;
    assert!(csp(&headers).ends_with("frame-ancestors https://acme.test"), "{}", csp(&headers));
    let (_, headers, _) = b.go(&h, "GET", &format!("/hub/{key}"), None).await;
    assert!(csp(&headers).contains("frame-ancestors 'none'"));
    h.ok("PATCH", "/api/hub/settings", Some(json!({"panel_origins": ["https://www.acme.test", "https://shop.acme.test/"]}))).await;
    let (_, headers, _) = b.go(&h, "GET", &format!("/hub/{key}?embed=1"), None).await;
    assert!(csp(&headers).ends_with("frame-ancestors https://www.acme.test https://shop.acme.test"), "{}", csp(&headers));
    let (s, _) = h.req("PATCH", "/api/hub/settings", Some(json!({"panel_origins": ["https://acme.test/path"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    sqlx::query("UPDATE accounts SET link_domain = NULL, link_domain_status = 'not_started' WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    // Every post carries a token: a sign-in form without one (or from a page this browser never
    // loaded) is refused.
    let (s, _, _) = Browser::default().go(&h, "POST", &format!("/hub/{key}/code"), Some("email=alice%40example.com")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let t = token(&b.page(&h, &key).await);
    let (s, _, _) = Browser::default().go(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=alice%40example.com"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "another browser's token");

    // The snippet's storefront config names the launcher only when it's on.
    let (_, _, forms) = h.raw("GET", &format!("/client/forms?k={key}"), &[], "").await;
    assert!(!forms.contains("\"hub\""), "{forms}");
    h.ok("PATCH", "/api/hub/settings", Some(json!({"launcher": "button"}))).await;
    let (_, _, forms) = h.raw("GET", &format!("/client/forms?k={key}"), &[], "").await;
    assert!(forms.contains(r#""hub":{"launcher":"button"}"#), "{forms}");
    let (_, _, js) = h.raw("GET", &format!("/client/hub.js?k={key}"), &[], "").await;
    assert!(js.contains(r#""launcher":"button""#));
    h.ok("PATCH", "/api/hub/settings", Some(json!({"enabled": false}))).await;
    let (_, _, forms) = h.raw("GET", &format!("/client/forms?k={key}"), &[], "").await;
    assert!(!forms.contains("\"hub\""));

    // Anything else under /hub/ is the hub's own 404, not the dashboard.
    for path in [format!("/hub/{key}/nope"), "/hub/".to_string(), format!("/hub/{key}/orders/1/x")] {
        let (s, _, body) = b.go(&h, "GET", &path, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body, "not found", "{path}");
    }
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
    sqlx::query("UPDATE memberships SET role = 'analyst'").execute(&h.state.db).await.unwrap();
    h.ok("GET", "/api/hub/settings", None).await;
    let (s, _) = h.req("PATCH", "/api/hub/settings", Some(json!({"enabled": false}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE memberships SET role = 'owner'").execute(&h.state.db).await.unwrap();
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
    let (s, _, _) = Browser::default().go(&h, "GET", &format!("/hub/{key}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, _, js) = h.raw("GET", &format!("/client/hub.js?k={key}"), &[], "").await;
    assert!(js.contains("var C = null;"));
    h.cleanup().await;
}

/// Sender screening (hosted): a pending account's sign-in codes go (transactional, to a person who
/// asked); a blocked account sends none, also when the block lands after the code was asked for.
#[tokio::test]
async fn a_blocked_account_sends_no_sign_in_code() {
    let Some(mut h) = Harness::with(|c| c.screen_senders = true).await else { return };
    let (account, key) = setup(&mut h).await;
    let standing: String = sqlx::query_scalar("SELECT sending_review FROM accounts WHERE id = $1").bind(account).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(standing, "pending");
    let mut b = Browser::default();
    h.state.email.clear_sent();
    ask_code(&h, &key, &mut b, "alice@example.com").await;
    assert!(code_sent_to(&h, "alice@example.com").is_some(), "a pending account still sends codes");
    // Blocked before the code is asked for: none is written or sent.
    sqlx::query("UPDATE accounts SET sending_review = 'blocked' WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    h.state.email.clear_sent();
    let (s, body) = ask_code(&h, &key, &mut b, "bob@example.com").await;
    assert!(s == StatusCode::OK && body.contains("If we have"), "the same answer either way");
    assert!(h.state.email.sent().is_empty());
    // Blocked after it was asked for, before the send job ran: the last check stops it.
    sqlx::query("UPDATE accounts SET sending_review = 'approved' WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM hub_codes").execute(&h.state.db).await.unwrap();
    let t = token(&b.page(&h, &key).await);
    b.go(&h, "POST", &format!("/hub/{key}/code"), Some(&format!("t={t}&email=alice%40example.com"))).await;
    sqlx::query("UPDATE accounts SET sending_review = 'blocked' WHERE id = $1").bind(account).execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert!(h.state.email.sent().is_empty());
    h.cleanup().await;
}

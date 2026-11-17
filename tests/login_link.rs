//! "Continue with Google" never hands someone an account another person set up under their
//! email: it links to a password login only once that login's email is confirmed, and otherwise
//! the person proves the password or starts fresh. Also the email confirmation links. Needs
//! TEST_DATABASE_URL (see tests/common).

#[macro_use]
mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

// ---- A browser ----

/// One browser: its cookies (any path), the address it calls from, and whether its writes say
/// they come from the app (as the dashboard's do).
struct B {
    cookies: HashMap<String, String>,
    ip: String,
    origin: Option<String>,
}

impl B {
    fn new(h: &Harness, ip: &str) -> Self {
        B { cookies: HashMap::new(), ip: ip.into(), origin: Some(h.state.cfg.public_url().to_string()) }
    }
    fn has(&self, name: &str) -> bool {
        self.cookies.contains_key(name)
    }
}

async fn send(h: &Harness, b: &mut B, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value, HeaderMap) {
    let mut r = Request::builder().method(method).uri(path).header("x-real-ip", &b.ip).header(header::USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Firefox/130.0");
    if let Some(o) = b.origin.as_ref().filter(|_| method != "GET") {
        r = r.header(header::ORIGIN, o);
    }
    if !b.cookies.is_empty() {
        r = r.header(header::COOKIE, b.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "));
    }
    // The page knows the account its session has open.
    if let Some(tok) = b.cookies.get("pl_session").filter(|_| method != "GET") {
        let a: Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM sessions WHERE token_hash = $1").bind(dumbmail::auth::sha256_hex(tok)).fetch_optional(&h.state.db).await.unwrap();
        if let Some(a) = a {
            r = r.header("x-dumbmail-account", a.to_string());
        }
    }
    let req = match body {
        Some(v) => r.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => r.body(Body::empty()).unwrap(),
    };
    let res = h.router.clone().oneshot(req).await.unwrap();
    for set in res.headers().get_all(header::SET_COOKIE) {
        let set = set.to_str().unwrap();
        let (name, value) = set.split(';').next().unwrap().split_once('=').unwrap();
        if value.is_empty() || set.contains("Max-Age=0") {
            b.cookies.remove(name);
        } else {
            b.cookies.insert(name.to_string(), value.to_string());
        }
    }
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(json!({"_raw": String::from_utf8_lossy(&bytes)})), headers)
}

async fn call(h: &Harness, b: &mut B, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (s, v, _) = send(h, b, method, path, body).await;
    (s, v)
}

async fn ok(h: &Harness, b: &mut B, method: &str, path: &str, body: Option<Value>) -> Value {
    let (s, v) = call(h, b, method, path, body).await;
    assert!(s.is_success(), "{method} {path} -> {s}: {v}");
    v
}

async fn bearer(h: &Harness, key: &str, method: &str, path: &str, body: Option<Value>) -> StatusCode {
    let r = Request::builder().method(method).uri(path).header(header::AUTHORIZATION, format!("Bearer {key}"));
    let req = match body {
        Some(v) => r.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => r.body(Body::empty()).unwrap(),
    };
    h.router.clone().oneshot(req).await.unwrap().status()
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ---- Google, played by a local stand-in ----

/// One stand-in for Google's token endpoint for the whole test binary, on its own thread (tests
/// run on their own runtimes). Each sign-in registers the claims for its own code.
fn mock_google() -> &'static Arc<Mutex<HashMap<String, Value>>> {
    static MOCK: OnceLock<Arc<Mutex<HashMap<String, Value>>>> = OnceLock::new();
    MOCK.get_or_init(|| {
        let codes: Arc<Mutex<HashMap<String, Value>>> = Arc::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let served = codes.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                use axum::routing::post;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
                let app = axum::Router::new().route(
                    "/token",
                    post(move |axum::Form(f): axum::Form<HashMap<String, String>>| {
                        let served = served.clone();
                        async move {
                            use base64::Engine;
                            let Some(c) = f.get("code").and_then(|c| served.lock().unwrap().remove(c)) else {
                                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"})));
                            };
                            let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
                            (StatusCode::OK, axum::Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": format!("{}.{}.sig", b64(&json!({"alg": "RS256"})), b64(&c))})))
                        }
                    }),
                );
                axum::serve(listener, app).await.unwrap();
            });
        });
        dumbmail::api::google::use_token_url_for_tests(&rx.recv().unwrap());
        codes
    })
}

/// Sign in with "Google" as (`sub`, `email`) and return where the callback sends the browser.
async fn google(h: &Harness, b: &mut B, sub: &str, email: &str, next: &str) -> String {
    let codes = mock_google();
    let (st, _, headers) = send(h, b, "GET", &format!("/api/auth/google?next={}", enc(next)), None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let loc = headers[header::LOCATION].to_str().unwrap().to_string();
    let q: HashMap<String, String> = url::Url::parse(&loc).unwrap().query_pairs().into_owned().collect();
    let code = format!("code-{}", Uuid::now_v7().simple());
    let now = chrono::Utc::now().timestamp();
    codes.lock().unwrap().insert(
        code.clone(),
        json!({"iss": "https://accounts.google.com", "aud": "test-client.apps.googleusercontent.com", "sub": sub, "email": email, "email_verified": true, "exp": now + 600, "name": "Vic Tim", "given_name": "Vic"}),
    );
    let (st, _, headers) = send(h, b, "GET", &format!("/api/auth/google/callback?code={code}&state={}", enc(&q["state"])), None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    headers[header::LOCATION].to_str().unwrap().to_string()
}

// ---- Setting things up ----

async fn signup(h: &Harness, b: &mut B, email: &str, password: &str, company: &str) -> Value {
    ok(h, b, "POST", "/api/auth/signup", Some(json!({"email": email, "password": password, "company": company}))).await
}

async fn login(h: &Harness, b: &mut B, email: &str, password: &str) -> (StatusCode, Value) {
    call(h, b, "POST", "/api/auth/login", Some(json!({"email": email, "password": password}))).await
}

/// The newest link in an email to `to` whose text contains `marker` (waits for background sends).
async fn link_token(h: &Harness, to: &str, marker: &str) -> String {
    for _ in 0..100 {
        let sent = h.state.email.sent();
        if let Some(m) = sent.iter().rev().find(|e| e.to == to && e.text.contains(marker)) {
            let i = m.text.find(marker).unwrap() + marker.len();
            return m.text[i..].split_whitespace().next().unwrap().to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    panic!("no email to {to} with {marker}");
}

/// Invite `email` as `role` and accept by making a new login. Returns its browser.
async fn join_new(h: &Harness, owner: &mut B, email: &str, role: &str, ip: &str) -> B {
    ok(h, owner, "POST", "/api/team/invites", Some(json!({"email": email, "role": role}))).await;
    let token = link_token(h, email, "/invite/").await;
    let mut s = B::new(h, ip);
    let me = ok(h, &mut s, "POST", &format!("/api/invites/{token}/accept"), Some(json!({"name": "Member", "password": "member pass 1"}))).await;
    assert_eq!(me["role"], role);
    s
}

async fn new_key(h: &Harness, b: &mut B, name: &str) -> String {
    ok(h, b, "POST", "/api/api-keys", Some(json!({"name": name}))).await["key"].as_str().unwrap().to_string()
}

async fn user_row(h: &Harness, email: &str) -> (Uuid, Option<String>, bool, bool) {
    sqlx::query_as("SELECT id, google_sub, email_confirmed_at IS NOT NULL, password_hash IS NOT NULL FROM users WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap()
}

/// Wait until the current `window`-second rate window has room for a run of attempts, so counts
/// aren't split across two windows.
async fn clear_of_rate_window(window: i64) {
    let into = chrono::Utc::now().timestamp().rem_euclid(window);
    if into > window - 45 {
        tokio::time::sleep(std::time::Duration::from_secs((window - into + 1) as u64)).await;
    }
}

// ---- The attack, and starting fresh ----

#[tokio::test]
async fn a_stranger_who_signed_up_first_never_gets_the_owners_account() {
    let h = harness!();
    // The stranger signs up as the victim's address and sets the account up.
    let mut stranger = B::new(&h, "203.0.113.10");
    let me = signup(&h, &mut stranger, "victim@acme.test", "stranger pass", "Bait").await;
    let old_account: Uuid = me["account"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(me["email_confirmed"], false);
    let mut second = join_new(&h, &mut stranger, "second@evil.test", "admin", "203.0.113.11").await;
    let stranger_key = new_key(&h, &mut stranger, "stranger").await;
    let second_key = new_key(&h, &mut second, "second").await;
    ok(&h, &mut stranger, "POST", "/api/team/invites", Some(json!({"email": "third@evil.test", "role": "manager"}))).await;
    let pending_invite = link_token(&h, "third@evil.test", "/invite/").await;
    let public_key: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(old_account).fetch_one(&h.state.db).await.unwrap();
    // Work waiting to run: a campaign due now and a live flow.
    let campaign = Uuid::now_v7();
    sqlx::query("INSERT INTO campaigns (id, account_id, name, channel, status, send_at) VALUES ($1, $2, 'Due', 'email', 'scheduled', now() + interval '1 hour')")
        .bind(campaign)
        .bind(old_account)
        .execute(&h.state.db)
        .await
        .unwrap();
    let flow = Uuid::now_v7();
    sqlx::query("INSERT INTO flows (id, account_id, name, status, trigger, definition) VALUES ($1, $2, 'Welcome', 'live', '{\"type\": \"metric\", \"metric\": \"Signed Up\"}', '{}')")
        .bind(flow)
        .bind(old_account)
        .execute(&h.state.db)
        .await
        .unwrap();

    // The real owner signs in with Google: not signed in to anything, sent to choose.
    let mut victim = B::new(&h, "198.51.100.20");
    let back = google(&h, &mut victim, "g-victim", "victim@acme.test", "/campaigns").await;
    assert_eq!(back, "/link-google");
    assert!(!victim.has("pl_session"), "no session before they choose");
    assert!(victim.has("pl_google_link"));
    let (_, sub, confirmed, _) = user_row(&h, "victim@acme.test").await;
    assert_eq!((sub, confirmed), (None, false), "nothing linked");
    let page = ok(&h, &mut victim, "GET", "/api/auth/google/link", None).await;
    assert_eq!((page["email"].as_str(), page["has_password"].as_bool(), page["can_start_fresh"].as_bool()), (Some("victim@acme.test"), Some(true), Some(true)));
    assert!(page["login_created_on"].as_str().unwrap().contains("20"), "{page}");

    // A second browser with only the page's address gets nothing.
    let mut other = B::new(&h, "198.51.100.21");
    assert_eq!(call(&h, &mut other, "GET", "/api/auth/google/link", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&h, &mut other, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&h, &mut other, "POST", "/api/auth/google/link/password", Some(json!({"password": "stranger pass"}))).await.0, StatusCode::NOT_FOUND);
    // Another site can't make the choice for them, even with the cookie.
    victim.origin = Some("https://evil.test".into());
    assert_eq!(call(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::FORBIDDEN);
    victim.origin = None;
    assert_eq!(call(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::FORBIDDEN);
    victim.origin = Some(h.state.cfg.public_url().to_string());

    // "That wasn't me, start fresh": a new, empty account of their own.
    let me = ok(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None).await;
    assert_eq!(me["next"], "/onboarding");
    let new_account: Uuid = me["account"]["id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new_account, old_account);
    assert_eq!((me["role"].as_str(), me["user"]["email"].as_str(), me["email_confirmed"].as_bool()), (Some("owner"), Some("victim@acme.test"), Some(true)));
    assert!(me["restriction"].is_null());
    assert!(victim.has("pl_session") && !victim.has("pl_google_link"));
    let team = ok(&h, &mut victim, "GET", "/api/team", None).await;
    assert_eq!(team["members"].as_array().unwrap().len(), 1, "{team}");
    assert_eq!(me["accounts"].as_array().unwrap().len(), 1);
    // Used up.
    assert_eq!(call(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::NOT_FOUND);
    // Google signs them straight in from now on.
    let mut again = B::new(&h, "198.51.100.22");
    assert_eq!(google(&h, &mut again, "g-victim", "victim@acme.test", "/campaigns").await, "/campaigns");
    assert_eq!(ok(&h, &mut again, "GET", "/api/me", None).await["account"]["id"], new_account.to_string());

    // The old account: no way in for anyone.
    assert_eq!(call(&h, &mut second, "GET", "/api/campaigns", None).await.0, StatusCode::UNAUTHORIZED);
    let (st, _) = login(&h, &mut second, "second@evil.test", "member pass 1").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "the teammate's login is untouched but holds no account");
    assert_eq!(call(&h, &mut stranger, "GET", "/api/me", None).await.0, StatusCode::UNAUTHORIZED);
    let (st, _) = login(&h, &mut stranger, "victim@acme.test", "stranger pass").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "the old password opens nothing");
    for key in [&stranger_key, &second_key] {
        assert_eq!(bearer(&h, key, "GET", "/api/profiles", None).await, StatusCode::UNAUTHORIZED);
        let mcp = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_account", "arguments": {}}});
        assert_eq!(bearer(&h, key, "POST", "/mcp", Some(mcp)).await, StatusCode::UNAUTHORIZED);
    }
    // Even a key that somehow wasn't revoked is refused on a closed account.
    sqlx::query("UPDATE api_keys SET revoked_at = NULL WHERE account_id = $1").bind(old_account).execute(&h.state.db).await.unwrap();
    assert_eq!(bearer(&h, &stranger_key, "GET", "/api/profiles", None).await, StatusCode::UNAUTHORIZED);
    let (st, _, _) = h.raw("GET", &format!("/api/invites/{pending_invite}"), &[], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "the pending invite is gone");
    // The website snippet's key no longer finds it.
    let (st, _, _) = h.raw("POST", &format!("/client/events?k={public_key}"), &[("content-type", "application/json")], &json!({"metric": "Viewed", "profile": {"email": "a@b.test"}}).to_string()).await;
    assert!(st.is_client_error(), "{st}");
    let (closed, members, logins): (bool, i64, i64) = sqlx::query_as(
        "SELECT closed_at IS NOT NULL, (SELECT count(*) FROM memberships WHERE account_id = $1), (SELECT count(*) FROM users WHERE email = 'victim@acme.test')
         FROM accounts WHERE id = $1",
    )
    .bind(old_account)
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    assert_eq!((closed, members, logins), (true, 0, 1), "closed, nobody in it, and the address belongs to one login");
    let old_login: (String, bool, bool) = sqlx::query_as("SELECT email, disabled_at IS NOT NULL, password_hash IS NULL FROM users WHERE id = (SELECT invited_by FROM team_invites WHERE email = 'second@evil.test')")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(old_login.0.starts_with("removed+") && old_login.0.ends_with("@invalid.dumbmail") && old_login.1 && old_login.2, "{old_login:?}");
    // Its scheduled work doesn't run: the campaign stopped, the flow is off, and the scheduler
    // never starts a campaign on a closed account even if one is due.
    let (cstatus, fstatus): (String, String) = sqlx::query_as("SELECT (SELECT status FROM campaigns WHERE id = $1), (SELECT status FROM flows WHERE id = $2)").bind(campaign).bind(flow).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((cstatus.as_str(), fstatus.as_str()), ("failed", "draft"));
    sqlx::query("UPDATE campaigns SET status = 'scheduled', send_at = now() - interval '1 minute' WHERE id = $1").bind(campaign).execute(&h.state.db).await.unwrap();
    dumbmail::jobs::scheduler_tick(&h.state, 1).await.unwrap();
    let still: String = sqlx::query_scalar("SELECT status FROM campaigns WHERE id = $1").bind(campaign).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(still, "scheduled");
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'campaign_send'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(queued, 0);
    // Recorded on the closed account.
    let (action, details): (String, Value) = sqlx::query_as("SELECT action, details FROM audit_log WHERE account_id = $1 AND action = 'account.closed'").bind(old_account).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((action.as_str(), details["reason"].as_str()), ("account.closed", Some("email_owner_started_fresh")));
    h.cleanup().await;
}

#[tokio::test]
async fn starting_fresh_leaves_accounts_with_another_owner_to_them() {
    let h = harness!();
    let mut stranger = B::new(&h, "203.0.113.30");
    signup(&h, &mut stranger, "pat@acme.test", "stranger pass", "Bait").await;
    // Another account where the stranger is one of two owners.
    let mut boss = B::new(&h, "203.0.113.31");
    let shared: Uuid = signup(&h, &mut boss, "boss@shared.test", "boss pass 12", "Shared").await["account"]["id"].as_str().unwrap().parse().unwrap();
    let (old_uid, ..) = user_row(&h, "pat@acme.test").await;
    sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, 'owner')").bind(old_uid).bind(shared).execute(&h.state.db).await.unwrap();
    let mut victim = B::new(&h, "198.51.100.30");
    assert_eq!(google(&h, &mut victim, "g-pat", "pat@acme.test", "/").await, "/link-google");
    ok(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None).await;
    // The shared account stays open for its other owner; the old login just left it.
    assert_eq!(ok(&h, &mut boss, "GET", "/api/me", None).await["account"]["id"], shared.to_string());
    let (closed, left): (bool, i64) = sqlx::query_as("SELECT closed_at IS NOT NULL, (SELECT count(*) FROM memberships WHERE account_id = $1 AND user_id = $2) FROM accounts WHERE id = $1")
        .bind(shared)
        .bind(old_uid)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!((closed, left), (false, 0));
    h.cleanup().await;
}

// ---- "That was me" ----

#[tokio::test]
async fn that_was_me_with_the_password_links_google_and_keeps_everything() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.40");
    let account: Uuid = signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await["account"]["id"].as_str().unwrap().parse().unwrap();
    let mut pat = join_new(&h, &mut owner, "pat@acme.test", "admin", "198.51.100.41").await;
    let owner_key = new_key(&h, &mut owner, "owner").await;
    let pat_key = new_key(&h, &mut pat, "pat").await;

    // Wrong passwords: nothing is linked, the same error as the sign-in page, and the sign-in
    // page's limits apply.
    clear_of_rate_window(900).await;
    let mut guesser = B::new(&h, "198.51.100.42");
    assert_eq!(google(&h, &mut guesser, "g-owner", "owner@acme.test", "/").await, "/link-google");
    let (st, v) = call(&h, &mut guesser, "POST", "/api/auth/google/link/password", Some(json!({"password": "wrong one"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (_, plain) = login(&h, &mut B::new(&h, "198.51.100.43"), "owner@acme.test", "also wrong").await;
    assert_eq!(v["error"]["message"], plain["error"]["message"], "{v}");
    assert!(!guesser.has("pl_session"));
    for _ in 0..9 {
        assert_eq!(call(&h, &mut guesser, "POST", "/api/auth/google/link/password", Some(json!({"password": "wrong again"}))).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(call(&h, &mut guesser, "POST", "/api/auth/google/link/password", Some(json!({"password": "correct horse"}))).await.0, StatusCode::TOO_MANY_REQUESTS, "throttled like the sign-in page, even with the right password");
    let (_, sub, confirmed, _) = user_row(&h, "owner@acme.test").await;
    assert_eq!((sub, confirmed), (None, false));
    let failures: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE account_id = $1 AND action = 'login.failed'").bind(account).fetch_one(&h.state.db).await.unwrap();
    assert!(failures >= 10, "{failures}");

    // The right password from the owner's own browser: linked, nothing removed, signed in.
    let mut real = B::new(&h, "198.51.100.44");
    assert_eq!(google(&h, &mut real, "g-owner", "owner@acme.test", "/campaigns").await, "/link-google");
    let me = ok(&h, &mut real, "POST", "/api/auth/google/link/password", Some(json!({"password": "correct horse"}))).await;
    assert_eq!((me["account"]["id"].as_str(), me["next"].as_str(), me["email_confirmed"].as_bool()), (Some(account.to_string().as_str()), Some("/campaigns"), Some(true)));
    assert_eq!(me["login_methods"], json!({"password": true, "google": true}));
    assert!(me["restriction"].is_null());
    assert!(real.has("pl_session") && !real.has("pl_google_link"));
    let (_, sub, confirmed, has_password) = user_row(&h, "owner@acme.test").await;
    assert_eq!((sub.as_deref(), confirmed, has_password), (Some("g-owner"), true, true));
    // Everything stays: the teammate, both keys, the old session and the password.
    assert_eq!(ok(&h, &mut real, "GET", "/api/team", None).await["members"].as_array().unwrap().len(), 2);
    ok(&h, &mut pat, "GET", "/api/campaigns", None).await;
    ok(&h, &mut owner, "GET", "/api/campaigns", None).await;
    assert_eq!(bearer(&h, &owner_key, "GET", "/api/profiles", None).await, StatusCode::OK);
    assert_eq!(bearer(&h, &pat_key, "GET", "/api/profiles", None).await, StatusCode::OK);
    assert_eq!(login(&h, &mut B::new(&h, "198.51.100.45"), "owner@acme.test", "correct horse").await.0, StatusCode::OK);
    // Used up; and the guesser's waiting sign-in can't link a second Google account now.
    assert_eq!(call(&h, &mut real, "POST", "/api/auth/google/link/password", Some(json!({"password": "correct horse"}))).await.0, StatusCode::NOT_FOUND);
    // Google signs them straight in from now on.
    let mut later = B::new(&h, "198.51.100.46");
    assert_eq!(google(&h, &mut later, "g-owner", "owner@acme.test", "/flows").await, "/flows");
    assert!(later.has("pl_session"));
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE account_id = $1 AND action = 'login.google_linked'").bind(account).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(linked, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn that_was_me_still_needs_two_factor() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.50");
    signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await;
    sqlx::query("UPDATE users SET totp_enabled_at = now() WHERE email = 'owner@acme.test'").execute(&h.state.db).await.unwrap();
    let mut b = B::new(&h, "198.51.100.51");
    assert_eq!(google(&h, &mut b, "g-owner", "owner@acme.test", "/").await, "/link-google");
    let me = ok(&h, &mut b, "POST", "/api/auth/google/link/password", Some(json!({"password": "correct horse"}))).await;
    assert_eq!(me["restriction"], "two_factor_code");
    assert_eq!(call(&h, &mut b, "GET", "/api/campaigns", None).await.0, StatusCode::FORBIDDEN);
    h.cleanup().await;
}

// ---- Logins whose email is confirmed ----

#[tokio::test]
async fn confirmed_logins_link_directly_and_keep_everything() {
    let h = harness!();
    // 1. Through the confirmation link sent at sign-up.
    let mut ann = B::new(&h, "198.51.100.60");
    let ann_account = signup(&h, &mut ann, "ann@acme.test", "ann password", "Ann Co").await["account"]["id"].as_str().unwrap().to_string();
    let ann_key = new_key(&h, &mut ann, "ann").await;
    let token = link_token(&h, "ann@acme.test", "/confirm-email?token=").await;
    let mut link_browser = B::new(&h, "198.51.100.61");
    ok(&h, &mut link_browser, "POST", "/api/auth/confirm-email", Some(json!({"token": token}))).await;
    assert!(link_browser.cookies.is_empty(), "confirming signs nobody in");
    assert_eq!(ok(&h, &mut ann, "GET", "/api/me", None).await["email_confirmed"], true);
    let mut g = B::new(&h, "198.51.100.62");
    assert_eq!(google(&h, &mut g, "g-ann", "ann@acme.test", "/campaigns").await, "/campaigns");
    assert_eq!(ok(&h, &mut g, "GET", "/api/me", None).await["account"]["id"], ann_account);
    ok(&h, &mut ann, "GET", "/api/campaigns", None).await;
    assert_eq!(bearer(&h, &ann_key, "GET", "/api/profiles", None).await, StatusCode::OK);
    assert_eq!(login(&h, &mut B::new(&h, "198.51.100.63"), "ann@acme.test", "ann password").await.0, StatusCode::OK);

    // 2. Through a completed password reset.
    let mut bo = B::new(&h, "198.51.100.64");
    signup(&h, &mut bo, "bo@acme.test", "bo password", "Bo Co").await;
    ok(&h, &mut bo, "POST", "/api/auth/password/forgot", Some(json!({"email": "bo@acme.test"}))).await;
    let reset = link_token(&h, "bo@acme.test", "/reset-password?token=").await;
    ok(&h, &mut B::new(&h, "198.51.100.65"), "POST", "/api/auth/password/reset", Some(json!({"token": reset, "password": "bo new password"}))).await;
    assert!(user_row(&h, "bo@acme.test").await.2);
    let mut g = B::new(&h, "198.51.100.66");
    assert_eq!(google(&h, &mut g, "g-bo", "bo@acme.test", "/").await, "/");
    assert!(g.has("pl_session"));
    assert_eq!(login(&h, &mut B::new(&h, "198.51.100.67"), "bo@acme.test", "bo new password").await.0, StatusCode::OK);

    // 3. By accepting a team invite sent to its address: a new login, and a signed-in one.
    let mut owner = B::new(&h, "198.51.100.68");
    let team_account = signup(&h, &mut owner, "owner@team.test", "owner password", "Team").await["account"]["id"].as_str().unwrap().to_string();
    join_new(&h, &mut owner, "cy@team.test", "manager", "198.51.100.69").await;
    assert!(user_row(&h, "cy@team.test").await.2);
    let mut g = B::new(&h, "198.51.100.70");
    assert_eq!(google(&h, &mut g, "g-cy", "cy@team.test", "/").await, "/");
    assert_eq!(ok(&h, &mut g, "GET", "/api/me", None).await["account"]["id"], team_account);
    let mut di = B::new(&h, "198.51.100.71");
    signup(&h, &mut di, "di@team.test", "di password", "Di Co").await;
    assert!(!user_row(&h, "di@team.test").await.2);
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "di@team.test", "role": "content"}))).await;
    let inv = link_token(&h, "di@team.test", "/invite/").await;
    ok(&h, &mut di, "POST", &format!("/api/invites/{inv}/accept"), None).await;
    assert!(user_row(&h, "di@team.test").await.2);
    let mut g = B::new(&h, "198.51.100.72");
    assert!(google(&h, &mut g, "g-di", "di@team.test", "/").await == "/" && g.has("pl_session"));

    // A login already linked to another Google account is still refused.
    let mut intruder = B::new(&h, "198.51.100.73");
    assert_eq!(google(&h, &mut intruder, "g-someone-else", "ann@acme.test", "/").await, "/login?error=conflict");
    assert!(intruder.cookies.is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn with_sign_ups_off_google_links_as_before() {
    let Some(h) = Harness::with(|c| c.allow_signup = false).await else { return };
    // Logins made by the operator (there's no sign-up).
    let (_, account) = dumbmail::api::account::create_account(&h.state, "old@acme.test", "old password", None, "Old").await.unwrap();
    dumbmail::api::account::create_account(&h.state, "kept@acme.test", "kept password", None, "Kept").await.unwrap();
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE email = 'kept@acme.test'").execute(&h.state.db).await.unwrap();
    // Unconfirmed: linked directly, its other ways in cut off (as before).
    let mut b = B::new(&h, "198.51.100.80");
    assert_eq!(google(&h, &mut b, "g-old", "old@acme.test", "/").await, "/");
    assert_eq!(ok(&h, &mut b, "GET", "/api/me", None).await["account"]["id"], account.id.to_string());
    let (_, sub, confirmed, has_password) = user_row(&h, "old@acme.test").await;
    assert_eq!((sub.as_deref(), confirmed, has_password), (Some("g-old"), true, false));
    // Confirmed: linked, nothing removed.
    let mut c = B::new(&h, "198.51.100.81");
    assert_eq!(google(&h, &mut c, "g-kept", "kept@acme.test", "/").await, "/");
    assert!(user_row(&h, "kept@acme.test").await.3, "the password stays");
    assert_eq!(login(&h, &mut B::new(&h, "198.51.100.82"), "kept@acme.test", "kept password").await.0, StatusCode::OK);
    h.cleanup().await;
}

// ---- The waiting sign-in ----

#[tokio::test]
async fn a_waiting_google_sign_in_works_once_briefly_and_only_in_its_browser() {
    let h = harness!();
    let mut stranger = B::new(&h, "203.0.113.90");
    signup(&h, &mut stranger, "vic@acme.test", "stranger pass", "Bait").await;
    // Expired.
    let mut b = B::new(&h, "198.51.100.90");
    assert_eq!(google(&h, &mut b, "g-vic", "vic@acme.test", "/").await, "/link-google");
    sqlx::query("UPDATE google_link_pending SET expires_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    assert_eq!(call(&h, &mut b, "GET", "/api/auth/google/link", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&h, &mut b, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&h, &mut b, "POST", "/api/auth/google/link/password", Some(json!({"password": "stranger pass"}))).await.0, StatusCode::NOT_FOUND);
    // The cookie lasts 10 minutes too.
    let (_, _, headers) = {
        let mut c = B::new(&h, "198.51.100.91");
        let codes = mock_google();
        let (_, _, hd) = send(&h, &mut c, "GET", "/api/auth/google?next=/", None).await;
        let q: HashMap<String, String> = url::Url::parse(hd[header::LOCATION].to_str().unwrap()).unwrap().query_pairs().into_owned().collect();
        let now = chrono::Utc::now().timestamp();
        codes.lock().unwrap().insert("code-ttl".into(), json!({"iss": "https://accounts.google.com", "aud": "test-client.apps.googleusercontent.com", "sub": "g-vic", "email": "vic@acme.test", "email_verified": true, "exp": now + 600}));
        send(&h, &mut c, "GET", &format!("/api/auth/google/callback?code=code-ttl&state={}", enc(&q["state"])), None).await
    };
    let set = headers.get_all(header::SET_COOKIE).iter().map(|c| c.to_str().unwrap().to_string()).find(|c| c.starts_with("pl_google_link=")).unwrap();
    assert!(set.contains("HttpOnly") && set.contains("Max-Age=600") && set.contains("Path=/api/auth/google/link"), "{set}");
    // A forged token.
    let mut forged = B::new(&h, "198.51.100.92");
    forged.cookies.insert("pl_google_link".into(), "not-a-real-token".into());
    assert_eq!(call(&h, &mut forged, "GET", "/api/auth/google/link", None).await.0, StatusCode::NOT_FOUND);
    // Two choices racing: only one goes through.
    let mut d = B::new(&h, "198.51.100.93");
    assert_eq!(google(&h, &mut d, "g-vic", "vic@acme.test", "/").await, "/link-google");
    let mut d2 = B { cookies: d.cookies.clone(), ip: d.ip.clone(), origin: d.origin.clone() };
    let (a, b2) = tokio::join!(call(&h, &mut d, "POST", "/api/auth/google/link/fresh", None), call(&h, &mut d2, "POST", "/api/auth/google/link/password", Some(json!({"password": "stranger pass"}))));
    let wins = [a.0, b2.0].iter().filter(|s| s.is_success()).count();
    assert_eq!(wins, 1, "{a:?} {b2:?}");
    let logins: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'vic@acme.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(logins, 1);
    h.cleanup().await;
}

// ---- Confirmation links ----

#[tokio::test]
async fn email_confirmation_links_work_once_expire_and_are_limited() {
    let h = harness!();
    let mut ann = B::new(&h, "198.51.100.100");
    signup(&h, &mut ann, "ann@acme.test", "ann password", "Ann Co").await;
    let first = link_token(&h, "ann@acme.test", "/confirm-email?token=").await;
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM email_confirmations").fetch_one(&h.state.db).await.unwrap();
    assert_ne!(stored, first, "only the hash is kept");
    // Wrong token.
    let mut anon = B::new(&h, "198.51.100.101");
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": "nope"}))).await.0, StatusCode::BAD_REQUEST);
    // Expired.
    sqlx::query("UPDATE email_confirmations SET expires_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": first}))).await.0, StatusCode::BAD_REQUEST);
    assert!(!user_row(&h, "ann@acme.test").await.2);
    // Ask again: a new link; the old one stops working.
    let sent = ok(&h, &mut ann, "POST", "/api/me/email-confirmation", None).await;
    assert_eq!(sent["already_confirmed"], false);
    let mut second = String::new();
    for _ in 0..100 {
        second = link_token(&h, "ann@acme.test", "/confirm-email?token=").await;
        if second != first {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    assert_ne!(second, first);
    let expires_hours: f64 = sqlx::query_scalar("SELECT (extract(epoch FROM expires_at - created_at) / 3600)::float8 FROM email_confirmations WHERE used_at IS NULL").fetch_one(&h.state.db).await.unwrap();
    assert!((expires_hours - 24.0).abs() < 0.01, "{expires_hours}");
    // Single use, and it signs nobody in.
    ok(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": second}))).await;
    assert!(anon.cookies.is_empty());
    assert!(user_row(&h, "ann@acme.test").await.2);
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": second}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(ok(&h, &mut ann, "GET", "/api/me", None).await["email_confirmed"], true);
    assert_eq!(ok(&h, &mut ann, "POST", "/api/me/email-confirmation", None).await["already_confirmed"], true);
    // A link sent to an address the login no longer has doesn't count.
    let mut bo = B::new(&h, "198.51.100.102");
    signup(&h, &mut bo, "bo@acme.test", "bo password", "Bo Co").await;
    let bo_token = link_token(&h, "bo@acme.test", "/confirm-email?token=").await;
    sqlx::query("UPDATE users SET email = 'bo2@acme.test' WHERE email = 'bo@acme.test'").execute(&h.state.db).await.unwrap();
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": bo_token}))).await.0, StatusCode::BAD_REQUEST);
    assert!(!user_row(&h, "bo2@acme.test").await.2);
    // Asking: 3 an hour per login; never with an API key; another site can't confirm for you.
    clear_of_rate_window(3600).await;
    let mut cy = B::new(&h, "198.51.100.103");
    signup(&h, &mut cy, "cy@acme.test", "cy password", "Cy Co").await;
    for _ in 0..3 {
        ok(&h, &mut cy, "POST", "/api/me/email-confirmation", None).await;
    }
    assert_eq!(call(&h, &mut cy, "POST", "/api/me/email-confirmation", None).await.0, StatusCode::TOO_MANY_REQUESTS);
    let key = new_key(&h, &mut cy, "k").await;
    assert_eq!(bearer(&h, &key, "POST", "/api/me/email-confirmation", None).await, StatusCode::FORBIDDEN);
    let mut evil = B::new(&h, "198.51.100.104");
    evil.origin = Some("https://evil.test".into());
    assert_eq!(call(&h, &mut evil, "POST", "/api/auth/confirm-email", Some(json!({"token": "x"}))).await.0, StatusCode::FORBIDDEN);
    // Opening links: 20 an hour per address.
    let mut guesser = B::new(&h, "198.51.100.105");
    for _ in 0..20 {
        assert_eq!(call(&h, &mut guesser, "POST", "/api/auth/confirm-email", Some(json!({"token": "guess"}))).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(call(&h, &mut guesser, "POST", "/api/auth/confirm-email", Some(json!({"token": "guess"}))).await.0, StatusCode::TOO_MANY_REQUESTS);
    h.cleanup().await;
}

// ---- Upgrading ----

#[tokio::test]
async fn the_migration_confirms_google_logins_only() {
    let Ok(admin_url) = std::env::var("TEST_DATABASE_URL") else { return };
    let admin = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&admin_url).await.unwrap();
    let dbname = format!("dumbmail_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {dbname}")).execute(&admin).await.unwrap();
    let url = format!("{}/{dbname}", admin_url.rsplit_once('/').unwrap().0);
    let db = sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();
    // A database at main's version before this change: every migration but 0039.
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(dumbmail::MIGRATOR.migrations.iter().filter(|m| m.version != 39).cloned().collect()),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&db).await.unwrap();
    sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, 'pw@acme.test', 'x')").bind(Uuid::now_v7()).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO users (id, email, google_sub) VALUES ($1, 'g@acme.test', 'g-1')").bind(Uuid::now_v7()).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO users (id, email, password_hash, google_sub) VALUES ($1, 'both@acme.test', 'x', 'g-2')").bind(Uuid::now_v7()).execute(&db).await.unwrap();
    dumbmail::MIGRATOR.run(&db).await.unwrap();
    let rows: Vec<(String, bool)> = sqlx::query_as("SELECT email, email_confirmed_at IS NOT NULL FROM users ORDER BY email").fetch_all(&db).await.unwrap();
    assert_eq!(rows, vec![("both@acme.test".into(), true), ("g@acme.test".into(), true), ("pw@acme.test".into(), false)]);
    db.close().await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)")).execute(&admin).await.unwrap();
}

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

/// Sign in with "Google" as `claims` (merged into a valid ID token) and return where the
/// callback sends the browser.
async fn google_token(h: &Harness, b: &mut B, claims: Value, next: &str) -> String {
    let codes = mock_google();
    let (st, _, headers) = send(h, b, "GET", &format!("/api/auth/google?next={}", enc(next)), None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let loc = headers[header::LOCATION].to_str().unwrap().to_string();
    let q: HashMap<String, String> = url::Url::parse(&loc).unwrap().query_pairs().into_owned().collect();
    let code = format!("code-{}", Uuid::now_v7().simple());
    let now = chrono::Utc::now().timestamp();
    let mut token = json!({"iss": "https://accounts.google.com", "aud": "test-client.apps.googleusercontent.com", "email_verified": true, "exp": now + 600, "name": "Vic Tim", "given_name": "Vic"});
    for (k, v) in claims.as_object().unwrap() {
        token[k] = v.clone();
    }
    codes.lock().unwrap().insert(code.clone(), token);
    let (st, _, headers) = send(h, b, "GET", &format!("/api/auth/google/callback?code={code}&state={}", enc(&q["state"])), None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    headers[header::LOCATION].to_str().unwrap().to_string()
}

/// Sign in with "Google" as (`sub`, `email`): a Google Workspace account on the address's own
/// domain (`hd`), or a Gmail account, so Google is the authority for the address.
async fn google(h: &Harness, b: &mut B, sub: &str, email: &str, next: &str) -> String {
    let domain = email.rsplit('@').next().unwrap();
    let claims = if domain == "gmail.com" { json!({"sub": sub, "email": email}) } else { json!({"sub": sub, "email": email, "hd": domain}) };
    google_token(h, b, claims, next).await
}

fn totp_code(secret_b32: &str, offset_secs: i64) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut secret, mut buf, mut bits) = (Vec::new(), 0u32, 0u32);
    for c in secret_b32.trim_end_matches('=').bytes() {
        buf = (buf << 5) | A.iter().position(|&x| x == c).unwrap() as u32;
        bits += 5;
        if bits >= 8 {
            secret.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    let t = totp_rs::Builder::new().with_secret(secret).with_digits(6).with_step_duration(30).with_skew(1).build().unwrap();
    t.generate((chrono::Utc::now().timestamp() + offset_secs) as u64).to_string()
}

/// Turn two-factor on for a signed-in browser; returns the secret. The code used is this step's,
/// so the next one entered must be a later step's (`totp_code(secret, 30)`).
async fn turn_on_two_factor(h: &Harness, b: &mut B) -> String {
    let setup = ok(h, b, "POST", "/api/me/two-factor/setup", None).await;
    let secret = setup["secret"].as_str().unwrap().to_string();
    ok(h, b, "POST", "/api/me/two-factor/enable", Some(json!({"code": totp_code(&secret, 0)}))).await;
    secret
}

async fn pending_rows(h: &Harness) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM google_link_pending").fetch_one(&h.state.db).await.unwrap()
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

    // The sign-up's confirmation link goes to the real owner. Opening it takes the login's
    // password, which they don't have, so it confirms nothing.
    let link = link_token(&h, "victim@acme.test", "/confirm-email?token=").await;
    let mut inbox = B::new(&h, "198.51.100.19");
    assert_eq!(call(&h, &mut inbox, "POST", "/api/auth/confirm-email", Some(json!({"token": link, "password": "my own password"}))).await.0, StatusCode::BAD_REQUEST);
    assert!(!user_row(&h, "victim@acme.test").await.2);

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
    let local = old_login.0.strip_prefix("removed+").and_then(|r| r.strip_suffix("@removed.dumbmail.invalid")).unwrap_or_else(|| panic!("{old_login:?}"));
    assert!(local.len() == 32 && local.chars().all(|c| c.is_ascii_hexdigit()) && old_login.1 && old_login.2, "{old_login:?}");
    // Random, not derived from the login's id, at a domain nobody can sign up with.
    let old_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = $1").bind(&old_login.0).fetch_one(&h.state.db).await.unwrap();
    assert_ne!(local, old_id.simple().to_string());
    let (st, _) = call(&h, &mut B::new(&h, "198.51.100.29"), "POST", "/api/auth/signup", Some(json!({"email": "x@removed.dumbmail.invalid", "password": "whatever pw", "company": "X"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
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

// ---- Two-factor login always applies, and a link waits for it ----

#[tokio::test]
async fn that_was_me_links_only_after_two_factor_and_google_never_skips_it() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.50");
    signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await;
    let secret = turn_on_two_factor(&h, &mut owner).await;
    // "That was me" with the right password: a password sign-in waiting on its code.
    let mut b = B::new(&h, "198.51.100.51");
    assert_eq!(google(&h, &mut b, "g-owner", "owner@acme.test", "/").await, "/link-google");
    let me = ok(&h, &mut b, "POST", "/api/auth/google/link/password", Some(json!({"password": "correct horse"}))).await;
    assert_eq!(me["restriction"], "two_factor_code");
    assert_eq!(call(&h, &mut b, "GET", "/api/campaigns", None).await.0, StatusCode::FORBIDDEN);
    // Until the code is in, nothing about the login has changed.
    let (_, sub, confirmed, _) = user_row(&h, "owner@acme.test").await;
    assert_eq!((sub, confirmed), (None, false), "not linked or confirmed before two-factor");
    // The waiting choice was taken by this sign-in: it can't be used again.
    assert_eq!(call(&h, &mut b, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::NOT_FOUND);
    // The code finishes the sign-in, and the link is made.
    let me = ok(&h, &mut b, "POST", "/api/auth/two-factor", Some(json!({"code": totp_code(&secret, 30)}))).await;
    assert!(me["restriction"].is_null());
    let (_, sub, confirmed, _) = user_row(&h, "owner@acme.test").await;
    assert_eq!((sub.as_deref(), confirmed), (Some("g-owner"), true));
    // A later Google sign-in to the linked login still needs the login's own code.
    let mut later = B::new(&h, "198.51.100.52");
    assert_eq!(google(&h, &mut later, "g-owner", "owner@acme.test", "/").await, "/");
    assert_eq!(ok(&h, &mut later, "GET", "/api/me", None).await["restriction"], "two_factor_code");
    assert_eq!(call(&h, &mut later, "GET", "/api/campaigns", None).await.0, StatusCode::FORBIDDEN);
    h.cleanup().await;
}

#[tokio::test]
async fn an_abandoned_two_factor_step_leaves_the_login_unlinked() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.53");
    signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await;
    let secret = turn_on_two_factor(&h, &mut owner).await;
    let mut b = B::new(&h, "198.51.100.54");
    assert_eq!(google(&h, &mut b, "g-owner", "owner@acme.test", "/").await, "/link-google");
    ok(&h, &mut b, "POST", "/api/auth/google/link/password", Some(json!({"password": "correct horse"}))).await;
    // They walk away; the wait for the code runs out. A code entered later still signs them in
    // (it's a password sign-in), but links nothing.
    sqlx::query("UPDATE google_link_pending SET expires_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    let me = ok(&h, &mut b, "POST", "/api/auth/two-factor", Some(json!({"code": totp_code(&secret, 30)}))).await;
    assert!(me["restriction"].is_null());
    let (_, sub, confirmed, _) = user_row(&h, "owner@acme.test").await;
    assert_eq!((sub, confirmed), (None, false));
    // Google still offers the choice next time.
    assert_eq!(google(&h, &mut B::new(&h, "198.51.100.55"), "g-owner", "owner@acme.test", "/").await, "/link-google");
    h.cleanup().await;
}

#[tokio::test]
async fn a_confirmed_login_with_two_factor_needs_the_code_before_google_is_linked() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.56");
    signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await;
    let secret = turn_on_two_factor(&h, &mut owner).await;
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE email = 'owner@acme.test'").execute(&h.state.db).await.unwrap();
    let mut b = B::new(&h, "198.51.100.57");
    assert_eq!(google(&h, &mut b, "g-owner", "owner@acme.test", "/flows").await, "/flows");
    assert_eq!(ok(&h, &mut b, "GET", "/api/me", None).await["restriction"], "two_factor_code");
    assert_eq!(call(&h, &mut b, "GET", "/api/campaigns", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(user_row(&h, "owner@acme.test").await.1, None, "not linked before the code");
    ok(&h, &mut b, "POST", "/api/auth/two-factor", Some(json!({"code": totp_code(&secret, 30)}))).await;
    assert_eq!(user_row(&h, "owner@acme.test").await.1.as_deref(), Some("g-owner"));
    ok(&h, &mut b, "GET", "/api/campaigns", None).await;
    h.cleanup().await;
}

#[tokio::test]
async fn a_link_is_made_only_while_the_login_is_as_it_was_checked() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.58");
    signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await;
    let secret = turn_on_two_factor(&h, &mut owner).await;
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE email = 'owner@acme.test'").execute(&h.state.db).await.unwrap();
    let mut b = B::new(&h, "198.51.100.59");
    assert_eq!(google(&h, &mut b, "g-owner", "owner@acme.test", "/").await, "/");
    // The password changes while the sign-in waits on its code.
    sqlx::query("UPDATE users SET credential_version = credential_version + 1 WHERE email = 'owner@acme.test'").execute(&h.state.db).await.unwrap();
    ok(&h, &mut b, "POST", "/api/auth/two-factor", Some(json!({"code": totp_code(&secret, 30)}))).await;
    assert_eq!(user_row(&h, "owner@acme.test").await.1, None, "the login changed, so nothing is linked");
    h.cleanup().await;
}

// ---- Google speaks only for addresses it's the authority for ----

#[tokio::test]
async fn google_matches_a_login_only_for_an_address_it_is_the_authority_for() {
    let h = harness!();
    let mut pat = B::new(&h, "198.51.100.140");
    signup(&h, &mut pat, "pat@acme.test", "pat password", "Pat Co").await;
    // A Google account using a non-Gmail address with no Workspace domain: no match at all.
    let mut b = B::new(&h, "198.51.100.141");
    assert_eq!(google_token(&h, &mut b, json!({"sub": "g-pat", "email": "pat@acme.test"}), "/").await, "/login?error=use_password");
    assert!(!b.has("pl_session") && !b.has("pl_google_link"));
    assert_eq!(pending_rows(&h).await, 0, "no waiting choice");
    // A Workspace account on another domain: no match either.
    assert_eq!(google_token(&h, &mut b, json!({"sub": "g-pat", "email": "pat@acme.test", "hd": "other.test"}), "/").await, "/login?error=use_password");
    assert_eq!(pending_rows(&h).await, 0);
    // Its own domain: Google speaks for it, so the choice is offered.
    assert_eq!(google_token(&h, &mut b, json!({"sub": "g-pat", "email": "pat@acme.test", "hd": "acme.test"}), "/").await, "/link-google");
    // A confirmed login isn't linked from a non-authoritative sign-in either.
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE email = 'pat@acme.test'").execute(&h.state.db).await.unwrap();
    let mut c = B::new(&h, "198.51.100.142");
    assert_eq!(google_token(&h, &mut c, json!({"sub": "g-pat", "email": "pat@acme.test"}), "/").await, "/login?error=use_password");
    assert_eq!(user_row(&h, "pat@acme.test").await.1, None);
    // Gmail needs no hd.
    let mut vic = B::new(&h, "198.51.100.143");
    signup(&h, &mut vic, "vic@gmail.com", "vic password", "Vic Co").await;
    assert_eq!(google_token(&h, &mut B::new(&h, "198.51.100.144"), json!({"sub": "g-vic", "email": "vic@gmail.com"}), "/").await, "/link-google");
    // Once linked by its Google account, a login signs in with it whatever the address.
    assert_eq!(google_token(&h, &mut c, json!({"sub": "g-pat", "email": "pat@acme.test", "hd": "acme.test"}), "/").await, "/");
    let mut d = B::new(&h, "198.51.100.145");
    assert_eq!(google_token(&h, &mut d, json!({"sub": "g-pat", "email": "pat@acme.test"}), "/").await, "/");
    assert!(d.has("pl_session"));
    // New sign-ups work for any address; only an authoritative one starts confirmed.
    let mut e = B::new(&h, "198.51.100.146");
    assert_eq!(google_token(&h, &mut e, json!({"sub": "g-new", "email": "new@elsewhere.test"}), "/").await, "/onboarding");
    assert!(!user_row(&h, "new@elsewhere.test").await.2);
    assert_eq!(google_token(&h, &mut B::new(&h, "198.51.100.147"), json!({"sub": "g-gm", "email": "someone@gmail.com"}), "/").await, "/onboarding");
    assert!(user_row(&h, "someone@gmail.com").await.2);
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
    ok(&h, &mut link_browser, "POST", "/api/auth/confirm-email", Some(json!({"token": token, "password": "ann password"}))).await;
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

    // A login already linked to another Google account is still refused.
    let mut intruder = B::new(&h, "198.51.100.73");
    assert_eq!(google(&h, &mut intruder, "g-someone-else", "ann@acme.test", "/").await, "/login?error=conflict");
    assert!(intruder.cookies.is_empty());
    h.cleanup().await;
}

#[tokio::test]
async fn with_sign_ups_off_the_same_rules_apply() {
    let Some(h) = Harness::with(|c| c.allow_signup = false).await else { return };
    // Logins made by the operator (there's no sign-up); invites and single sign-on can make them too.
    dumbmail::api::account::create_account(&h.state, "old@acme.test", "old password", None, "Old").await.unwrap();
    dumbmail::api::account::create_account(&h.state, "kept@acme.test", "kept password", None, "Kept").await.unwrap();
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE email = 'kept@acme.test'").execute(&h.state.db).await.unwrap();
    // Unconfirmed: the choice page, never a direct link; starting fresh isn't offered.
    let mut b = B::new(&h, "198.51.100.80");
    assert_eq!(google(&h, &mut b, "g-old", "old@acme.test", "/").await, "/link-google");
    assert!(!b.has("pl_session"));
    let (_, sub, _, has_password) = user_row(&h, "old@acme.test").await;
    assert_eq!((sub, has_password), (None, true), "nothing linked, nothing wiped");
    assert_eq!(ok(&h, &mut b, "GET", "/api/auth/google/link", None).await["can_start_fresh"], false);
    assert_eq!(call(&h, &mut b, "POST", "/api/auth/google/link/fresh", None).await.0, StatusCode::FORBIDDEN);
    ok(&h, &mut b, "POST", "/api/auth/google/link/password", Some(json!({"password": "old password"}))).await;
    let (_, sub, confirmed, has_password) = user_row(&h, "old@acme.test").await;
    assert_eq!((sub.as_deref(), confirmed, has_password), (Some("g-old"), true, true));
    // Confirmed: linked, nothing removed.
    let mut c = B::new(&h, "198.51.100.81");
    assert_eq!(google(&h, &mut c, "g-kept", "kept@acme.test", "/").await, "/");
    assert!(user_row(&h, "kept@acme.test").await.3, "the password stays");
    assert_eq!(login(&h, &mut B::new(&h, "198.51.100.82"), "kept@acme.test", "kept password").await.0, StatusCode::OK);
    h.cleanup().await;
}

#[tokio::test]
async fn accepting_an_invite_confirms_nothing() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.68");
    let team_account = signup(&h, &mut owner, "owner@team.test", "owner password", "Team").await["account"]["id"].as_str().unwrap().to_string();
    // A new login made by accepting an emailed invite: not confirmed, so Google offers the choice.
    join_new(&h, &mut owner, "cy@team.test", "manager", "198.51.100.69").await;
    assert!(!user_row(&h, "cy@team.test").await.2);
    assert_eq!(google(&h, &mut B::new(&h, "198.51.100.70"), "g-cy", "cy@team.test", "/").await, "/link-google");
    // An existing login accepting while signed in: not confirmed either.
    let mut di = B::new(&h, "198.51.100.71");
    signup(&h, &mut di, "di@team.test", "di password", "Di Co").await;
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "di@team.test", "role": "content"}))).await;
    let inv = link_token(&h, "di@team.test", "/invite/").await;
    ok(&h, &mut di, "POST", &format!("/api/invites/{inv}/accept"), None).await;
    assert!(!user_row(&h, "di@team.test").await.2);
    // Signing up from an invite with Google: confirmed only when Google is the authority.
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "gi@team.test", "role": "content"}))).await;
    let t = link_token(&h, "gi@team.test", "/invite/").await;
    let mut gi = B::new(&h, "198.51.100.72");
    assert_eq!(google(&h, &mut gi, "g-gi", "gi@team.test", &format!("/invite/{t}")).await, "/");
    assert_eq!(ok(&h, &mut gi, "GET", "/api/me", None).await["account"]["id"], team_account);
    assert!(user_row(&h, "gi@team.test").await.2);
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "gx@else.test", "role": "content"}))).await;
    let t = link_token(&h, "gx@else.test", "/invite/").await;
    let mut gx = B::new(&h, "198.51.100.73");
    assert_eq!(google_token(&h, &mut gx, json!({"sub": "g-gx", "email": "gx@else.test"}), &format!("/invite/{t}")).await, "/");
    assert!(!user_row(&h, "gx@else.test").await.2);
    h.cleanup().await;
}

#[tokio::test]
async fn an_invite_link_shown_to_the_inviter_confirms_nothing() {
    // Without account email the inviter is handed the link, so a stranger could invite the login
    // they squatted and accept it themselves.
    let Some(h) = Harness::with(|c| c.support_email = None).await else { return };
    let mut squat = B::new(&h, "203.0.113.120");
    signup(&h, &mut squat, "sq@acme.test", "stranger pass", "Bait").await;
    let mut other = B::new(&h, "203.0.113.121");
    signup(&h, &mut other, "accomplice@evil.test", "accomplice pw", "Other").await;
    let inv = ok(&h, &mut other, "POST", "/api/team/invites", Some(json!({"email": "sq@acme.test", "role": "admin"}))).await;
    let url = inv["invite_url"].as_str().expect("the inviter sees the link").to_string();
    let token = url.rsplit('/').next().unwrap();
    ok(&h, &mut squat, "POST", &format!("/api/invites/{token}/accept"), None).await;
    assert!(!user_row(&h, "sq@acme.test").await.2, "not confirmed");
    let mut owner = B::new(&h, "198.51.100.120");
    assert_eq!(google(&h, &mut owner, "g-sq", "sq@acme.test", "/").await, "/link-google");
    h.cleanup().await;
}

// ---- Leaving and ownership changes can't cross ----

#[tokio::test]
async fn starting_fresh_and_an_ownership_transfer_cannot_leave_an_account_ownerless() {
    let h = harness!();
    // Account X is owned by M; the unconfirmed login L (victim@acme.test) is an admin there.
    let mut m = B::new(&h, "203.0.113.130");
    let x: Uuid = signup(&h, &mut m, "m@x.test", "m password 1", "X").await["account"]["id"].as_str().unwrap().parse().unwrap();
    let mut l = B::new(&h, "203.0.113.131");
    signup(&h, &mut l, "victim@acme.test", "stranger pass", "Bait").await;
    ok(&h, &mut m, "POST", "/api/team/invites", Some(json!({"email": "victim@acme.test", "role": "admin"}))).await;
    let t = link_token(&h, "victim@acme.test", "/invite/").await;
    ok(&h, &mut l, "POST", &format!("/api/invites/{t}/accept"), None).await;
    let (l_id, ..) = user_row(&h, "victim@acme.test").await;
    let (m_id, ..) = user_row(&h, "m@x.test").await;
    let mut victim = B::new(&h, "198.51.100.130");
    assert_eq!(google(&h, &mut victim, "g-victim", "victim@acme.test", "/").await, "/link-google");
    // M hands X to L in a transaction that's still open (the locks an ownership transfer takes:
    // the account, then the memberships it changes) while the victim starts fresh.
    let mut transfer = h.state.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM accounts WHERE id = $1 FOR UPDATE").bind(x).execute(&mut *transfer).await.unwrap();
    sqlx::query("SELECT user_id FROM memberships WHERE account_id = $1 AND user_id = ANY($2) ORDER BY user_id FOR UPDATE").bind(x).bind(vec![l_id, m_id]).execute(&mut *transfer).await.unwrap();
    sqlx::query("UPDATE memberships SET role = 'owner' WHERE account_id = $1 AND user_id = $2").bind(x).bind(l_id).execute(&mut *transfer).await.unwrap();
    sqlx::query("UPDATE memberships SET role = 'admin' WHERE account_id = $1 AND user_id = $2").bind(x).bind(m_id).execute(&mut *transfer).await.unwrap();
    let (fresh, ()) = tokio::join!(call(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None), async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        transfer.commit().await.unwrap();
    });
    assert!(fresh.0.is_success(), "{fresh:?}");
    // X ended with its only owner gone, so it was closed rather than left with nobody in charge.
    let (closed, owners): (bool, i64) = sqlx::query_as("SELECT closed_at IS NOT NULL, (SELECT count(*) FROM memberships WHERE account_id = $1 AND role = 'owner') FROM accounts WHERE id = $1")
        .bind(x)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(closed || owners > 0, "X has no owner and isn't closed");
    assert!(closed);
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
        codes.lock().unwrap().insert("code-ttl".into(), json!({"iss": "https://accounts.google.com", "aud": "test-client.apps.googleusercontent.com", "sub": "g-vic", "email": "vic@acme.test", "hd": "acme.test", "email_verified": true, "exp": now + 600}));
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
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": "nope", "password": "ann password"}))).await.0, StatusCode::BAD_REQUEST);
    // Expired.
    sqlx::query("UPDATE email_confirmations SET expires_at = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": first, "password": "ann password"}))).await.0, StatusCode::BAD_REQUEST);
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
    // The wrong password: the sign-in page's answer, nothing confirmed, and the link still works.
    let (st, wrong) = call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": second, "password": "not hers"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(wrong["error"]["message"].as_str().unwrap().contains("don't match"), "{wrong}");
    assert!(!user_row(&h, "ann@acme.test").await.2);
    // Single use, and it signs nobody in.
    ok(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": second, "password": "ann password"}))).await;
    assert!(anon.cookies.is_empty());
    assert!(user_row(&h, "ann@acme.test").await.2);
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": second, "password": "ann password"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(ok(&h, &mut ann, "GET", "/api/me", None).await["email_confirmed"], true);
    assert_eq!(ok(&h, &mut ann, "POST", "/api/me/email-confirmation", None).await["already_confirmed"], true);
    // A link sent to an address the login no longer has doesn't count.
    let mut bo = B::new(&h, "198.51.100.102");
    signup(&h, &mut bo, "bo@acme.test", "bo password", "Bo Co").await;
    let bo_token = link_token(&h, "bo@acme.test", "/confirm-email?token=").await;
    sqlx::query("UPDATE users SET email = 'bo2@acme.test' WHERE email = 'bo@acme.test'").execute(&h.state.db).await.unwrap();
    assert_eq!(call(&h, &mut anon, "POST", "/api/auth/confirm-email", Some(json!({"token": bo_token, "password": "bo password"}))).await.0, StatusCode::BAD_REQUEST);
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
    assert_eq!(call(&h, &mut evil, "POST", "/api/auth/confirm-email", Some(json!({"token": "x", "password": "y"}))).await.0, StatusCode::FORBIDDEN);
    // Opening links: 20 an hour per address.
    let mut guesser = B::new(&h, "198.51.100.105");
    for _ in 0..20 {
        assert_eq!(call(&h, &mut guesser, "POST", "/api/auth/confirm-email", Some(json!({"token": "guess", "password": "y"}))).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(call(&h, &mut guesser, "POST", "/api/auth/confirm-email", Some(json!({"token": "guess", "password": "y"}))).await.0, StatusCode::TOO_MANY_REQUESTS);
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

// ---- Races the last review named ----

#[tokio::test]
async fn the_link_write_rechecks_two_factor_under_the_login_lock() {
    let h = harness!();
    let mut owner = B::new(&h, "198.51.100.150");
    signup(&h, &mut owner, "owner@acme.test", "correct horse", "Acme").await;
    let (uid, ..) = user_row(&h, "owner@acme.test").await;
    sqlx::query("UPDATE users SET email_confirmed_at = now() WHERE id = $1").bind(uid).execute(&h.state.db).await.unwrap();
    let (sid, version): (Uuid, i32) = sqlx::query_as("SELECT s.id, u.credential_version FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.user_id = $1").bind(uid).fetch_one(&h.state.db).await.unwrap();
    // A Google link waiting on this (fully signed-in) session.
    sqlx::query(
        "INSERT INTO google_link_pending (token_hash, user_id, email, google_sub, google_authoritative, session_id, credential_version, was_confirmed, expires_at)
         VALUES ('race-1', $1, 'owner@acme.test', 'g-owner', true, $2, $3, true, now() + interval '15 minutes')",
    )
    .bind(uid)
    .bind(sid)
    .bind(version)
    .execute(&h.state.db)
    .await
    .unwrap();
    // Two-factor is turned on between the "is it signed in" check and the write.
    let mut on = h.state.db.begin().await.unwrap();
    sqlx::query("UPDATE users SET totp_enabled_at = now() WHERE id = $1").bind(uid).execute(&mut *on).await.unwrap();
    let meta = dumbmail::features::team::login::ReqMeta::default();
    let (linked, ()) = tokio::join!(dumbmail::features::team::google_link::complete_if_signed_in(&h.state, sid, &meta), async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        on.commit().await.unwrap();
    });
    assert!(!linked.unwrap(), "two-factor arrived first: the link waits for a code");
    assert_eq!(user_row(&h, "owner@acme.test").await.1, None);
    h.cleanup().await;
}

#[tokio::test]
async fn starting_fresh_takes_locks_in_the_one_order_and_cannot_deadlock() {
    let h = harness!();
    // Two logins, each the only owner of an account and an admin in the other's.
    let mut l1 = B::new(&h, "203.0.113.160");
    let a1: Uuid = signup(&h, &mut l1, "one@acme.test", "one password", "One").await["account"]["id"].as_str().unwrap().parse().unwrap();
    let mut l2 = B::new(&h, "203.0.113.161");
    let a2: Uuid = signup(&h, &mut l2, "two@acme.test", "two password", "Two").await["account"]["id"].as_str().unwrap().parse().unwrap();
    for (owner, who) in [(&mut l1, "two@acme.test"), (&mut l2, "one@acme.test")] {
        ok(&h, owner, "POST", "/api/team/invites", Some(json!({"email": who, "role": "admin"}))).await;
    }
    let t = link_token(&h, "two@acme.test", "/invite/").await;
    ok(&h, &mut l2, "POST", &format!("/api/invites/{t}/accept"), None).await;
    let t = link_token(&h, "one@acme.test", "/invite/").await;
    ok(&h, &mut l1, "POST", &format!("/api/invites/{t}/accept"), None).await;
    let mut victim = B::new(&h, "198.51.100.160");
    assert_eq!(google(&h, &mut victim, "g-one", "one@acme.test", "/").await, "/link-google");
    // Another change to both accounts in the one order (accounts, then memberships) is under way
    // as "start fresh" begins. Taken in the other order, the two would deadlock.
    let mut other = h.state.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM accounts WHERE id = ANY($1) ORDER BY id FOR UPDATE").bind(vec![a1, a2]).execute(&mut *other).await.unwrap();
    let (fresh, second) = tokio::join!(call(&h, &mut victim, "POST", "/api/auth/google/link/fresh", None), async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let r = sqlx::query("SELECT 1 FROM memberships WHERE account_id = ANY($1) ORDER BY account_id, user_id FOR UPDATE").bind(vec![a1, a2]).execute(&mut *other).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let c = other.commit().await;
        (r.is_ok(), c.is_ok())
    });
    assert_eq!(second, (true, true), "the other change wasn't aborted");
    assert!(fresh.0.is_success(), "{fresh:?}");
    h.cleanup().await;
}

#[tokio::test]
async fn joining_rechecks_that_the_login_is_still_active() {
    let h = harness!();
    let mut owner = B::new(&h, "203.0.113.170");
    let x: Uuid = signup(&h, &mut owner, "boss@x.test", "boss password", "X").await["account"]["id"].as_str().unwrap().parse().unwrap();
    let mut l = B::new(&h, "203.0.113.171");
    signup(&h, &mut l, "pat@acme.test", "pat password", "Pat").await;
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "pat@acme.test", "role": "admin"}))).await;
    let t = link_token(&h, "pat@acme.test", "/invite/").await;
    let (pid, ..) = user_row(&h, "pat@acme.test").await;
    // The login is being given up while it accepts.
    let mut gone = h.state.db.begin().await.unwrap();
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1").bind(pid).execute(&mut *gone).await.unwrap();
    let path = format!("/api/invites/{t}/accept");
    let (accept, ()) = tokio::join!(call(&h, &mut l, "POST", &path, None), async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        gone.commit().await.unwrap();
    });
    assert!(!accept.0.is_success(), "{accept:?}");
    let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND account_id = $2)").bind(pid).bind(x).fetch_one(&h.state.db).await.unwrap();
    assert!(!member, "a disabled login didn't join (or come to own) the account");
    h.cleanup().await;
}

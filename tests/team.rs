//! Team members, roles, several accounts per login, two-factor login, sessions, API key scopes,
//! the audit log and single sign-on. Needs TEST_DATABASE_URL (see tests/common).

#[macro_use]
mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use common::*;
use dumbmail::features::team::access;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

/// One browser: its session cookie and the address it calls from.
struct S {
    cookie: Option<String>,
    ip: String,
}

impl S {
    fn new(ip: &str) -> Self {
        S { cookie: None, ip: ip.into() }
    }
}

async fn send(h: &Harness, s: &mut S, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut b = Request::builder().method(method).uri(path).header("x-real-ip", &s.ip).header(header::USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Firefox/130.0");
    if let Some(c) = &s.cookie {
        b = b.header(header::COOKIE, c);
    }
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = h.router.clone().oneshot(req).await.unwrap();
    for set in res.headers().get_all(header::SET_COOKIE) {
        let v = set.to_str().unwrap().split(';').next().unwrap().to_string();
        if let Some(val) = v.strip_prefix("pl_session=") {
            s.cookie = if val.is_empty() { None } else { Some(v.clone()) };
        }
    }
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(json!({"_raw": String::from_utf8_lossy(&bytes)}));
    (status, v, headers)
}

async fn call(h: &Harness, s: &mut S, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (st, v, _) = send(h, s, method, path, body).await;
    (st, v)
}

async fn ok(h: &Harness, s: &mut S, method: &str, path: &str, body: Option<Value>) -> Value {
    let (st, v) = call(h, s, method, path, body).await;
    assert!(st.is_success(), "{method} {path} -> {st}: {v}");
    v
}

async fn status(h: &Harness, s: &mut S, method: &str, path: &str, body: Option<Value>) -> StatusCode {
    call(h, s, method, path, body).await.0
}

async fn bearer(h: &Harness, key: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(path).header(header::AUTHORIZATION, format!("Bearer {key}"));
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = h.router.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(json!({"_raw": String::from_utf8_lossy(&bytes)})))
}

/// Call an MCP tool with a key; returns (is_error, text, structured).
async fn mcp(h: &Harness, key: &str, tool: &str, args: Value) -> (bool, String, Value) {
    let (st, v) = bearer(h, key, "POST", "/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let r = &v["result"];
    (r["isError"].as_bool().unwrap_or(false), r["content"][0]["text"].as_str().unwrap_or("").to_string(), r["structuredContent"].clone())
}

async fn signup(h: &Harness, s: &mut S, email: &str, company: &str) -> Value {
    let me = ok(h, s, "POST", "/api/auth/signup", Some(json!({"email": email, "password": "correct horse", "company": company}))).await;
    ok(h, s, "PATCH", "/api/account", Some(json!({"address": "1 Main St, Portland, OR 97201", "from_email": format!("hello@{}", email.split('@').nth(1).unwrap())}))).await;
    me
}

async fn login(h: &Harness, s: &mut S, email: &str, password: &str) -> Value {
    ok(h, s, "POST", "/api/auth/login", Some(json!({"email": email, "password": password}))).await
}

/// The newest invite link token sent to `to`.
fn invite_token(h: &Harness, to: &str) -> String {
    let sent = h.state.email.sent();
    let m = sent.iter().rev().find(|e| e.to == to && e.text.contains("/invite/")).unwrap_or_else(|| panic!("no invite email to {to}"));
    let i = m.text.find("/invite/").unwrap() + "/invite/".len();
    m.text[i..].split_whitespace().next().unwrap().to_string()
}

/// Invite `email` with `role` and accept by creating a login. Returns the new member's browser.
async fn join(h: &Harness, owner: &mut S, email: &str, role: &str) -> (S, Uuid) {
    let inv = ok(h, owner, "POST", "/api/team/invites", Some(json!({"email": email, "role": role}))).await;
    assert_eq!(inv["email_sent"], true, "{inv}");
    let token = invite_token(h, email);
    let mut s = S::new("203.0.113.50");
    let me = ok(h, &mut s, "POST", &format!("/api/invites/{token}/accept"), Some(json!({"name": "Member", "password": "member pass 1"}))).await;
    assert_eq!(me["role"], role);
    (s, me["user"]["id"].as_str().unwrap().parse().unwrap())
}

fn draft_body() -> Value {
    json!({"channel": "email", "name": "Draft", "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>x</p>"}]}})
}

// ---- Every route and tool has a permission rule ----

/// Every `.route("path", get(..).post(..))` in the server's source.
fn routes_in_source() -> BTreeSet<(String, String)> {
    let method_re = regex::Regex::new(r"(?:^|[^A-Za-z0-9_])(get|post|put|patch|delete|options|head)\(").unwrap();
    let mut out = BTreeSet::new();
    let mut stack = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            let bytes = src.as_bytes();
            let mut from = 0;
            while let Some(i) = src[from..].find(".route(\"") {
                let start = from + i + ".route(".len();
                // The path literal, then everything up to the matching close paren.
                let path_end = src[start + 1..].find('"').unwrap() + start + 1;
                let path = src[start + 1..path_end].to_string();
                let (mut depth, mut j, mut in_str) = (1i32, path_end + 1, false);
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        b'"' if bytes[j - 1] != b'\\' => in_str = !in_str,
                        b'(' if !in_str => depth += 1,
                        b')' if !in_str => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                let args = &src[path_end + 1..j];
                for c in method_re.captures_iter(args) {
                    out.insert((c[1].to_uppercase(), path.clone()));
                }
                from = j;
            }
        }
    }
    out
}

#[tokio::test]
async fn every_route_and_tool_has_a_permission_rule() {
    let h = harness!();
    let in_source = routes_in_source();
    assert!(in_source.len() > 150, "found only {} routes; is the parser broken?", in_source.len());
    let rules: BTreeSet<(String, String)> = access::all_route_rules().iter().map(|r| (r.method.to_string(), r.path.to_string())).collect();
    let missing: Vec<_> = in_source.difference(&rules).collect();
    assert!(missing.is_empty(), "routes without a permission rule (add them to features::team::access::CORE_ROUTES or the feature's access()): {missing:?}");
    let stale: Vec<_> = rules.difference(&in_source).collect();
    assert!(stale.is_empty(), "permission rules for routes that don't exist: {stale:?}");

    // Every protected rule matches a real route that refuses anonymous callers.
    let mut wrong = Vec::new();
    for r in access::all_route_rules() {
        if matches!(r.perm, access::Perm::Public) {
            continue;
        }
        let path = regex::Regex::new(r"\{[a-z_]+\}").unwrap().replace_all(r.path, "0190a000-0000-7000-8000-000000000000").to_string();
        let mut b = Request::builder().method(r.method).uri(&path);
        if matches!(r.method, "POST" | "PATCH" | "PUT") {
            b = b.header(header::CONTENT_TYPE, "application/json");
        }
        let body = if matches!(r.method, "POST" | "PATCH" | "PUT") { Body::from("{}") } else { Body::empty() };
        let res = h.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        if res.status() != StatusCode::UNAUTHORIZED {
            wrong.push(format!("{} {} -> {}", r.method, r.path, res.status()));
        }
    }
    assert!(wrong.is_empty(), "protected routes that didn't ask for sign-in: {wrong:?}");

    // Anything unmapped is refused, even for an owner.
    let ctx = dumbmail::auth::Ctx::system(Uuid::now_v7());
    let r = access::authorize_route(&h.state, &ctx, &Method::GET, Some("/api/not-mapped"), None).await;
    assert!(matches!(r, Err(dumbmail::error::AppError::Forbidden(_))));
    let r = access::authorize_route(&h.state, &ctx, &Method::GET, None, None).await;
    assert!(matches!(r, Err(dumbmail::error::AppError::Forbidden(_))));

    // Every MCP tool has a rule.
    let mut s = S::new("198.51.100.1");
    signup(&h, &mut s, "owner@acme.test", "Acme").await;
    let key = ok(&h, &mut s, "POST", "/api/api-keys", Some(json!({"name": "all"}))).await;
    let (_, v) = bearer(&h, key["key"].as_str().unwrap(), "POST", "/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))).await;
    let tools = v["result"]["tools"].as_array().unwrap();
    assert!(tools.len() > 60);
    let unmapped: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).filter(|n| access::tool_perm(n, &json!({})).is_none()).collect();
    assert!(unmapped.is_empty(), "MCP tools without a permission rule: {unmapped:?}");
    h.cleanup().await;
}

// ---- Roles ----

#[tokio::test]
async fn roles_are_enforced_on_the_server() {
    let h = harness!();
    let mut owner = S::new("198.51.100.2");
    let me = signup(&h, &mut owner, "owner@acme.test", "Acme").await;
    assert_eq!(me["role"], "owner");
    let owner_id = me["user"]["id"].as_str().unwrap().to_string();
    let (mut admin, _) = join(&h, &mut owner, "admin@acme.test", "admin").await;
    let (mut manager, manager_id) = join(&h, &mut owner, "manager@acme.test", "manager").await;
    let (mut analyst, analyst_id) = join(&h, &mut owner, "analyst@acme.test", "analyst").await;
    let (mut content, _) = join(&h, &mut owner, "content@acme.test", "content").await;

    // Manager: marketing yes; team, billing, keys, domains and the audit log no; no escalation.
    assert_eq!(status(&h, &mut manager, "POST", "/api/team/invites", Some(json!({"email": "x@acme.test", "role": "admin"}))).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut manager, "PATCH", &format!("/api/team/members/{analyst_id}"), Some(json!({"role": "admin"}))).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut manager, "PATCH", &format!("/api/team/members/{manager_id}"), Some(json!({"role": "owner"}))).await, StatusCode::FORBIDDEN);
    let (st, err) = call(&h, &mut manager, "GET", "/api/billing", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(err["error"]["message"].as_str().unwrap().contains("Manager"), "{err}");
    for (m, p) in [("POST", "/api/api-keys"), ("GET", "/api/api-keys"), ("GET", "/api/audit"), ("POST", "/api/account/domains"), ("POST", "/api/phone-numbers"), ("PATCH", "/api/team/security")] {
        assert_eq!(status(&h, &mut manager, m, p, Some(json!({}))).await, StatusCode::FORBIDDEN, "manager {m} {p}");
    }
    let c1 = ok(&h, &mut manager, "POST", "/api/campaigns", Some(draft_body())).await;
    ok(&h, &mut manager, "GET", "/api/profiles", None).await;
    ok(&h, &mut manager, "GET", "/api/team", None).await;

    // Analyst: reads, changes nothing.
    ok(&h, &mut analyst, "GET", "/api/campaigns", None).await;
    ok(&h, &mut analyst, "GET", "/api/profiles", None).await;
    ok(&h, &mut analyst, "GET", "/api/analytics/overview", None).await;
    for (m, p, b) in [("POST", "/api/lists", json!({"name": "x"})), ("POST", "/api/campaigns", draft_body()), ("PATCH", "/api/account", json!({"name": "Hacked"})), ("POST", "/api/flows", json!({}))] {
        assert_eq!(status(&h, &mut analyst, m, p, Some(b)).await, StatusCode::FORBIDDEN, "analyst {m} {p}");
    }

    // Content: drafts yes; sending, profiles and scheduled campaigns no.
    let c2 = ok(&h, &mut content, "POST", "/api/campaigns", Some(draft_body())).await;
    let c2 = c2["id"].as_str().unwrap();
    ok(&h, &mut content, "PATCH", &format!("/api/campaigns/{c2}"), Some(json!({"name": "Edited"}))).await;
    for (m, p) in [("POST", format!("/api/campaigns/{c2}/send")), ("POST", format!("/api/campaigns/{c2}/schedule")), ("DELETE", format!("/api/campaigns/{c2}")), ("GET", "/api/profiles".into()), ("GET", "/api/events".into())] {
        assert_eq!(status(&h, &mut content, m, &p, Some(json!({}))).await, StatusCode::FORBIDDEN, "content {m} {p}");
    }
    let c1 = c1["id"].as_str().unwrap();
    sqlx::query("UPDATE campaigns SET status = 'scheduled', send_at = now() + interval '1 day' WHERE id = $1::uuid").bind(c1).execute(&h.state.db).await.unwrap();
    let (st, err) = call(&h, &mut content, "PATCH", &format!("/api/campaigns/{c1}"), Some(json!({"name": "Sneaky"}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{err}");
    ok(&h, &mut manager, "PATCH", &format!("/api/campaigns/{c1}"), Some(json!({"name": "Fine"}))).await;
    let me = ok(&h, &mut content, "GET", "/api/me", None).await;
    let perms: Vec<&str> = me["permissions"].as_array().unwrap().iter().filter_map(|p| p.as_str()).collect();
    assert!(perms.contains(&"campaigns.draft") && !perms.contains(&"campaigns.send") && !perms.contains(&"profiles.read"), "{perms:?}");

    // Admin: everything but owners and ownership.
    ok(&h, &mut admin, "GET", "/api/billing", None).await;
    ok(&h, &mut admin, "GET", "/api/audit", None).await;
    assert_eq!(status(&h, &mut admin, "POST", "/api/team/invites", Some(json!({"email": "boss@acme.test", "role": "owner"}))).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut admin, "PATCH", &format!("/api/team/members/{owner_id}"), Some(json!({"role": "analyst"}))).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut admin, "DELETE", &format!("/api/team/members/{owner_id}"), None).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut admin, "POST", "/api/team/transfer", Some(json!({"user_id": owner_id}))).await, StatusCode::FORBIDDEN);
    ok(&h, &mut admin, "PATCH", &format!("/api/team/members/{analyst_id}"), Some(json!({"role": "manager"}))).await;
    ok(&h, &mut analyst, "POST", "/api/lists", Some(json!({"name": "Now allowed"}))).await;

    // MCP follows the same rules: a key made by the admin can't do owner-only things.
    let key = ok(&h, &mut admin, "POST", "/api/api-keys", Some(json!({"name": "admin key"}))).await;
    let key = key["key"].as_str().unwrap();
    let (is_err, text, _) = mcp(&h, key, "invite_member", json!({"email": "boss@acme.test", "role": "owner", "confirm": true})).await;
    assert!(is_err && text.contains("owner"), "{text}");
    h.cleanup().await;
}

// ---- Invites ----

#[tokio::test]
async fn invites_accept_resend_revoke_and_abuse() {
    let h = harness!();
    let mut owner = S::new("198.51.100.3");
    let me = signup(&h, &mut owner, "owner@acme.test", "Acme Goods").await;
    let acme = me["account"]["id"].as_str().unwrap().to_string();
    let mut bob = S::new("198.51.100.4");
    signup(&h, &mut bob, "bob@other.test", "Bob Co").await;

    let inv = ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "Ann@Example.test", "role": "analyst"}))).await;
    assert_eq!(inv["invite"]["email"], "ann@example.test");
    assert!(inv.get("invite_url").is_none(), "links go by email when the server can send it");
    let mail = h.state.email.sent().into_iter().rev().find(|e| e.to == "ann@example.test").unwrap();
    assert!(mail.subject.contains("Acme Goods") && mail.from_email == "help@pl.test", "{} from {}", mail.subject, mail.from_email);
    let token = invite_token(&h, "ann@example.test");
    assert_eq!(status(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "ann@example.test", "role": "admin"}))).await, StatusCode::CONFLICT);

    let mut anon = S::new("198.51.100.5");
    let info = ok(&h, &mut anon, "GET", &format!("/api/invites/{token}"), None).await;
    assert_eq!((info["account_name"].as_str(), info["role"].as_str(), info["has_login"].as_bool()), (Some("Acme Goods"), Some("analyst"), Some(false)));

    // Someone else's login can't take it.
    let (st, err) = call(&h, &mut bob, "POST", &format!("/api/invites/{token}/accept"), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{err}");
    assert!(err["error"]["message"].as_str().unwrap().contains("a***@example.test"));
    assert_eq!(status(&h, &mut anon, "POST", &format!("/api/invites/{token}/accept"), Some(json!({"password": "short"}))).await, StatusCode::BAD_REQUEST);
    let me = ok(&h, &mut anon, "POST", &format!("/api/invites/{token}/accept"), Some(json!({"name": "Ann", "password": "ann password"}))).await;
    assert_eq!((me["account"]["id"].as_str(), me["role"].as_str(), me["user"]["email"].as_str()), (Some(acme.as_str()), Some("analyst"), Some("ann@example.test")));
    // Used links stop working.
    let mut other = S::new("198.51.100.6");
    assert_eq!(status(&h, &mut other, "GET", &format!("/api/invites/{token}"), None).await, StatusCode::NOT_FOUND);
    assert_eq!(status(&h, &mut other, "POST", &format!("/api/invites/{token}/accept"), Some(json!({"password": "another pass"}))).await, StatusCode::NOT_FOUND);
    assert_eq!(status(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "ann@example.test", "role": "admin"}))).await, StatusCode::CONFLICT);

    // Resend gives a new link; the old one stops working. Revoke and expiry end it.
    let inv = ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "carl@example.test", "role": "content"}))).await;
    let old = invite_token(&h, "carl@example.test");
    ok(&h, &mut owner, "POST", &format!("/api/team/invites/{}/resend", inv["invite"]["id"].as_str().unwrap()), None).await;
    let new = invite_token(&h, "carl@example.test");
    assert_ne!(old, new);
    assert_eq!(status(&h, &mut anon, "GET", &format!("/api/invites/{old}"), None).await, StatusCode::NOT_FOUND);
    ok(&h, &mut anon, "GET", &format!("/api/invites/{new}"), None).await;
    ok(&h, &mut owner, "DELETE", &format!("/api/team/invites/{}", inv["invite"]["id"].as_str().unwrap()), None).await;
    assert_eq!(status(&h, &mut anon, "GET", &format!("/api/invites/{new}"), None).await, StatusCode::NOT_FOUND);
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "dan@example.test", "role": "content"}))).await;
    let dan = invite_token(&h, "dan@example.test");
    sqlx::query("UPDATE team_invites SET expires_at = now() - interval '1 minute' WHERE email = 'dan@example.test'").execute(&h.state.db).await.unwrap();
    assert_eq!(status(&h, &mut anon, "GET", &format!("/api/invites/{dan}"), None).await, StatusCode::NOT_FOUND);

    // An email that already has a login signs in to accept; then the login has both accounts.
    ok(&h, &mut owner, "POST", "/api/team/invites", Some(json!({"email": "bob@other.test", "role": "manager"}))).await;
    let bt = invite_token(&h, "bob@other.test");
    let mut signed_out = S::new("198.51.100.7");
    assert_eq!(status(&h, &mut signed_out, "POST", &format!("/api/invites/{bt}/accept"), Some(json!({"password": "taking over"}))).await, StatusCode::CONFLICT);
    let me = ok(&h, &mut bob, "POST", &format!("/api/invites/{bt}/accept"), None).await;
    assert_eq!(me["account"]["id"].as_str(), Some(acme.as_str()));
    assert_eq!(me["accounts"].as_array().unwrap().len(), 2);

    let team = ok(&h, &mut owner, "GET", "/api/team", None).await;
    assert_eq!(team["members"].as_array().unwrap().len(), 3);
    assert_eq!(team["invites"].as_array().unwrap().len(), 1, "only dan's expired invite is still open: {team}");
    assert_eq!(team["invites"][0]["expired"], true);
    h.cleanup().await;
}

#[tokio::test]
async fn owners_cannot_be_lost_and_ownership_transfers() {
    let h = harness!();
    let mut owner = S::new("198.51.100.8");
    let me = signup(&h, &mut owner, "owner@acme.test", "Acme").await;
    let owner_id = me["user"]["id"].as_str().unwrap().to_string();
    assert_eq!(status(&h, &mut owner, "PATCH", &format!("/api/team/members/{owner_id}"), Some(json!({"role": "admin"}))).await, StatusCode::CONFLICT);
    assert_eq!(status(&h, &mut owner, "DELETE", &format!("/api/team/members/{owner_id}"), None).await, StatusCode::CONFLICT);
    assert_eq!(status(&h, &mut owner, "POST", "/api/team/leave", None).await, StatusCode::CONFLICT);

    let (mut admin, admin_id) = join(&h, &mut owner, "admin@acme.test", "admin").await;
    ok(&h, &mut owner, "POST", "/api/team/transfer", Some(json!({"user_id": admin_id}))).await;
    assert_eq!(ok(&h, &mut owner, "GET", "/api/me", None).await["role"], "admin");
    assert_eq!(ok(&h, &mut admin, "GET", "/api/me", None).await["role"], "owner");
    assert_eq!(status(&h, &mut owner, "POST", "/api/team/transfer", Some(json!({"user_id": owner_id}))).await, StatusCode::FORBIDDEN);

    // The old owner made a key; removing them revokes it and signs them out.
    let key = ok(&h, &mut owner, "POST", "/api/api-keys", Some(json!({"name": "mine"}))).await;
    let key = key["key"].as_str().unwrap().to_string();
    assert_eq!(bearer(&h, &key, "GET", "/api/lists", None).await.0, StatusCode::OK);
    ok(&h, &mut admin, "DELETE", &format!("/api/team/members/{owner_id}"), None).await;
    assert_eq!(bearer(&h, &key, "GET", "/api/lists", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(status(&h, &mut owner, "GET", "/api/me", None).await, StatusCode::UNAUTHORIZED);
    let (st, err) = call(&h, &mut S::new("198.51.100.9"), "POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "correct horse"}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{err}");
    h.cleanup().await;
}

// ---- Several accounts per login ----

#[tokio::test]
async fn several_accounts_per_login_stay_separate() {
    let h = harness!();
    let mut s = S::new("198.51.100.10");
    let me = signup(&h, &mut s, "owner@acme.test", "Brand A").await;
    let a = me["account"]["id"].as_str().unwrap().to_string();
    let secret = ok(&h, &mut s, "POST", "/api/lists", Some(json!({"name": "A secret list"}))).await;
    ok(&h, &mut s, "POST", "/api/profiles", Some(json!({"email": "a-customer@x.test"}))).await;
    let me = ok(&h, &mut s, "POST", "/api/accounts", Some(json!({"name": "Brand B"}))).await;
    let b = me["account"]["id"].as_str().unwrap().to_string();
    assert_ne!(a, b);
    assert_eq!((me["account"]["name"].as_str(), me["role"].as_str(), me["accounts"].as_array().unwrap().len()), (Some("Brand B"), Some("owner"), 2));

    // While B is open, nothing from A shows up, by listing or by id.
    let lists = ok(&h, &mut s, "GET", "/api/lists", None).await;
    assert!(!lists.to_string().contains("A secret list"), "{lists}");
    assert_eq!(status(&h, &mut s, "GET", &format!("/api/lists/{}", secret["id"].as_str().unwrap()), None).await, StatusCode::NOT_FOUND);
    assert!(!ok(&h, &mut s, "GET", "/api/profiles", None).await.to_string().contains("a-customer@x.test"));
    let bkey = ok(&h, &mut s, "POST", "/api/api-keys", Some(json!({"name": "b"}))).await;
    let (_, l) = bearer(&h, bkey["key"].as_str().unwrap(), "GET", "/api/lists", None).await;
    assert!(!l.to_string().contains("A secret list"));

    // Only accounts you belong to can be opened.
    let mut stranger = S::new("198.51.100.11");
    let other = signup(&h, &mut stranger, "x@other.test", "Other").await;
    assert_eq!(status(&h, &mut s, "POST", "/api/accounts/switch", Some(json!({"account_id": other["account"]["id"]}))).await, StatusCode::NOT_FOUND);
    let me = ok(&h, &mut s, "POST", "/api/accounts/switch", Some(json!({"account_id": a}))).await;
    assert_eq!(me["account"]["id"].as_str(), Some(a.as_str()));
    assert!(ok(&h, &mut s, "GET", "/api/lists", None).await.to_string().contains("A secret list"));
    // The next sign-in opens the account used last.
    ok(&h, &mut s, "POST", "/api/accounts/switch", Some(json!({"account_id": b}))).await;
    let mut again = S::new("198.51.100.12");
    assert_eq!(login(&h, &mut again, "owner@acme.test", "correct horse").await["account"]["id"].as_str(), Some(b.as_str()));

    // A role holds per account: an owner of their own brand is still only an analyst in A.
    ok(&h, &mut s, "POST", "/api/accounts/switch", Some(json!({"account_id": a}))).await;
    ok(&h, &mut s, "POST", "/api/team/invites", Some(json!({"email": "x@other.test", "role": "analyst"}))).await;
    let t = invite_token(&h, "x@other.test");
    let me = ok(&h, &mut stranger, "POST", &format!("/api/invites/{t}/accept"), None).await;
    assert_eq!((me["account"]["id"].as_str(), me["role"].as_str()), (Some(a.as_str()), Some("analyst")));
    assert_eq!(status(&h, &mut stranger, "POST", "/api/lists", Some(json!({"name": "nope"}))).await, StatusCode::FORBIDDEN);
    ok(&h, &mut stranger, "POST", "/api/accounts/switch", Some(json!({"account_id": other["account"]["id"]}))).await;
    ok(&h, &mut stranger, "POST", "/api/lists", Some(json!({"name": "mine"}))).await;
    h.cleanup().await;
}

// ---- Two-factor login ----

fn totp_code(secret_b32: &str, offset_secs: i64) -> String {
    let secret = base32_decode(secret_b32);
    let t = totp_rs::Builder::new().with_secret(secret).with_digits(6).with_step_duration(30).with_skew(1).build().unwrap();
    t.generate((chrono::Utc::now().timestamp() + offset_secs) as u64).to_string()
}

fn base32_decode(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut out, mut buf, mut bits) = (Vec::new(), 0u32, 0u32);
    for c in s.bytes() {
        buf = (buf << 5) | A.iter().position(|&x| x == c).unwrap() as u32;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    out
}

#[tokio::test]
async fn two_factor_login() {
    let h = harness!();
    let mut s = S::new("198.51.100.20");
    signup(&h, &mut s, "owner@acme.test", "Acme").await;
    // Requiring it for everyone needs it on for yourself first.
    assert_eq!(status(&h, &mut s, "PATCH", "/api/team/security", Some(json!({"require_two_factor": true}))).await, StatusCode::BAD_REQUEST);
    assert_eq!(ok(&h, &mut s, "GET", "/api/me/two-factor", None).await["enabled"], false);
    let setup = ok(&h, &mut s, "POST", "/api/me/two-factor/setup", None).await;
    assert!(setup["qr_code"].as_str().unwrap().starts_with("data:image/svg+xml;base64,"));
    assert!(setup["otpauth_url"].as_str().unwrap().starts_with("otpauth://totp/DumbMail:owner%40acme.test?"), "{}", setup["otpauth_url"]);
    let secret = setup["secret"].as_str().unwrap().to_string();
    assert_eq!(status(&h, &mut s, "POST", "/api/me/two-factor/enable", Some(json!({"code": "000000"}))).await, StatusCode::BAD_REQUEST);
    let on = ok(&h, &mut s, "POST", "/api/me/two-factor/enable", Some(json!({"code": totp_code(&secret, 0)}))).await;
    let recovery: Vec<String> = on["recovery_codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    assert_eq!(recovery.len(), 10);
    let stored: Vec<String> = sqlx::query_scalar("SELECT code_hash FROM recovery_codes").fetch_all(&h.state.db).await.unwrap();
    assert!(stored.iter().all(|c| !recovery.iter().any(|r| c.contains(&r.replace('-', "")))), "recovery codes are stored hashed");
    let enc: String = sqlx::query_scalar("SELECT totp_secret_enc FROM users").fetch_one(&h.state.db).await.unwrap();
    assert!(!enc.contains(&secret), "the secret is stored encrypted");

    // Password sign-in now stops at the code.
    let mut s2 = S::new("198.51.100.21");
    let me = login(&h, &mut s2, "owner@acme.test", "correct horse").await;
    assert_eq!(me["restriction"], "two_factor_code");
    assert!(me["account"].get("from_email").is_none(), "account details wait until sign-in is done");
    assert_eq!(status(&h, &mut s2, "GET", "/api/campaigns", None).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut s2, "GET", "/api/me/sessions", None).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut s2, "POST", "/api/me/two-factor/setup", None).await, StatusCode::CONFLICT, "can't swap in a new device before the code");
    assert_eq!(status(&h, &mut s2, "POST", "/api/auth/two-factor", Some(json!({"code": "123456"}))).await, StatusCode::BAD_REQUEST);
    let code = totp_code(&secret, 30);
    let me = ok(&h, &mut s2, "POST", "/api/auth/two-factor", Some(json!({"code": code}))).await;
    assert!(me["restriction"].is_null(), "{me}");
    ok(&h, &mut s2, "GET", "/api/campaigns", None).await;

    // The same code doesn't work twice.
    let mut s3 = S::new("198.51.100.22");
    login(&h, &mut s3, "owner@acme.test", "correct horse").await;
    assert_eq!(status(&h, &mut s3, "POST", "/api/auth/two-factor", Some(json!({"code": code}))).await, StatusCode::BAD_REQUEST);
    // A recovery code works once.
    ok(&h, &mut s3, "POST", "/api/auth/two-factor", Some(json!({"code": recovery[0].to_uppercase()}))).await;
    let mut s4 = S::new("198.51.100.23");
    login(&h, &mut s4, "owner@acme.test", "correct horse").await;
    assert_eq!(status(&h, &mut s4, "POST", "/api/auth/two-factor", Some(json!({"code": recovery[0]}))).await, StatusCode::BAD_REQUEST);
    assert_eq!(ok(&h, &mut s, "GET", "/api/me/two-factor", None).await["recovery_codes_left"], 9);

    // Wrong codes are limited.
    let mut last = StatusCode::OK;
    for _ in 0..8 {
        last = status(&h, &mut s4, "POST", "/api/auth/two-factor", Some(json!({"code": "999999"}))).await;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(status(&h, &mut s4, "POST", "/api/auth/two-factor", Some(json!({"code": recovery[1]}))).await, StatusCode::TOO_MANY_REQUESTS);

    // Requiring it for everyone: a new member sets it up before doing anything.
    ok(&h, &mut s, "PATCH", "/api/team/security", Some(json!({"require_two_factor": true}))).await;
    let (mut m, _) = {
        let inv = ok(&h, &mut s, "POST", "/api/team/invites", Some(json!({"email": "m@acme.test", "role": "manager"}))).await;
        assert_eq!(inv["email_sent"], true);
        let token = invite_token(&h, "m@acme.test");
        let mut m = S::new("198.51.100.24");
        let me = ok(&h, &mut m, "POST", &format!("/api/invites/{token}/accept"), Some(json!({"password": "member pass 1"}))).await;
        assert_eq!(me["restriction"], "two_factor_setup");
        (m, ())
    };
    assert_eq!(status(&h, &mut m, "GET", "/api/campaigns", None).await, StatusCode::FORBIDDEN);
    let setup = ok(&h, &mut m, "POST", "/api/me/two-factor/setup", None).await;
    ok(&h, &mut m, "POST", "/api/me/two-factor/enable", Some(json!({"code": totp_code(setup["secret"].as_str().unwrap(), 0)}))).await;
    ok(&h, &mut m, "GET", "/api/campaigns", None).await;
    let (st, err) = call(&h, &mut m, "POST", "/api/me/two-factor/disable", Some(json!({"code": totp_code(setup["secret"].as_str().unwrap(), 30)}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{err}");
    let sec = ok(&h, &mut s, "GET", "/api/team/security", None).await;
    assert_eq!((sec["require_two_factor"].as_bool(), sec["members_without_two_factor"].as_i64()), (Some(true), Some(0)));
    h.cleanup().await;
}

// ---- Sessions and passwords ----

#[tokio::test]
async fn sessions_passwords_and_resets() {
    let h = harness!();
    let mut s1 = S::new("198.51.100.30");
    signup(&h, &mut s1, "owner@acme.test", "Acme").await;
    let mut s2 = S::new("198.51.100.31");
    login(&h, &mut s2, "owner@acme.test", "correct horse").await;
    let list = ok(&h, &mut s1, "GET", "/api/me/sessions", None).await;
    let rows = list["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|r| r["current"] == true).count(), 1);
    assert!(rows.iter().all(|r| r["device"] == "Firefox on macOS"), "{list}");
    assert!(rows.iter().any(|r| r["ip"] == "198.51.100.31"));
    ok(&h, &mut s1, "POST", "/api/me/sessions/revoke-others", None).await;
    assert_eq!(status(&h, &mut s2, "GET", "/api/me", None).await, StatusCode::UNAUTHORIZED);

    // Revoke one by id.
    login(&h, &mut s2, "owner@acme.test", "correct horse").await;
    let list = ok(&h, &mut s1, "GET", "/api/me/sessions", None).await;
    let other = list["data"].as_array().unwrap().iter().find(|r| r["current"] == false).unwrap()["id"].as_str().unwrap().to_string();
    ok(&h, &mut s1, "DELETE", &format!("/api/me/sessions/{other}"), None).await;
    assert_eq!(status(&h, &mut s2, "GET", "/api/me", None).await, StatusCode::UNAUTHORIZED);

    // Changing the password signs out every other session.
    login(&h, &mut s2, "owner@acme.test", "correct horse").await;
    assert_eq!(status(&h, &mut s1, "POST", "/api/me/password", Some(json!({"current_password": "wrong one", "new_password": "a new password"}))).await, StatusCode::BAD_REQUEST);
    ok(&h, &mut s1, "POST", "/api/me/password", Some(json!({"current_password": "correct horse", "new_password": "a new password"}))).await;
    assert_eq!(status(&h, &mut s2, "GET", "/api/me", None).await, StatusCode::UNAUTHORIZED);
    ok(&h, &mut s1, "GET", "/api/me", None).await;
    assert_eq!(status(&h, &mut s2, "POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "correct horse"}))).await, StatusCode::BAD_REQUEST);
    login(&h, &mut s2, "owner@acme.test", "a new password").await;

    // No hint whether an email has a login.
    let (st1, unknown) = call(&h, &mut S::new("198.51.100.32"), "POST", "/api/auth/login", Some(json!({"email": "nobody@acme.test", "password": "whatever1"}))).await;
    let (st2, wrong) = call(&h, &mut S::new("198.51.100.32"), "POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "whatever1"}))).await;
    assert_eq!((st1, &unknown["error"]), (st2, &wrong["error"]));
    let f1 = ok(&h, &mut S::new("198.51.100.33"), "POST", "/api/auth/password/forgot", Some(json!({"email": "nobody@acme.test"}))).await;
    let f2 = ok(&h, &mut S::new("198.51.100.33"), "POST", "/api/auth/password/forgot", Some(json!({"email": "owner@acme.test"}))).await;
    assert_eq!(f1, f2);

    // The reset email arrives (it's sent in the background).
    let mut link = None;
    for _ in 0..50 {
        if let Some(m) = h.state.email.sent().into_iter().find(|e| e.to == "owner@acme.test" && e.text.contains("reset-password?token=")) {
            link = Some(m.text.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let text = link.expect("reset email");
    assert!(!h.state.email.sent().iter().any(|e| e.to == "nobody@acme.test"));
    let i = text.find("token=").unwrap() + 6;
    let token = text[i..].split_whitespace().next().unwrap().to_string();
    let mut anon = S::new("198.51.100.34");
    assert_eq!(status(&h, &mut anon, "POST", "/api/auth/password/reset", Some(json!({"token": "nope", "password": "reset password"}))).await, StatusCode::BAD_REQUEST);
    ok(&h, &mut anon, "POST", "/api/auth/password/reset", Some(json!({"token": token, "password": "reset password"}))).await;
    assert_eq!(status(&h, &mut s1, "GET", "/api/me", None).await, StatusCode::UNAUTHORIZED, "a reset signs out every session");
    assert_eq!(status(&h, &mut anon, "POST", "/api/auth/password/reset", Some(json!({"token": token, "password": "again password"}))).await, StatusCode::BAD_REQUEST);
    login(&h, &mut s1, "owner@acme.test", "reset password").await;

    // Too many wrong passwords for one email are refused for a while, even the right one.
    let mut last = StatusCode::OK;
    for i in 0..11 {
        last = status(&h, &mut S::new(&format!("192.0.2.{i}")), "POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "bad guess!"}))).await;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(status(&h, &mut S::new("192.0.2.99"), "POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "reset password"}))).await, StatusCode::TOO_MANY_REQUESTS);
    let audit = ok(&h, &mut s1, "GET", "/api/audit?action=login.failed", None).await;
    assert!(audit["total"].as_i64().unwrap() >= 10, "{audit}");
    h.cleanup().await;
}

// ---- API key scopes ----

#[tokio::test]
async fn api_key_scopes_expiry_and_creator() {
    let h = harness!();
    let mut owner = S::new("198.51.100.40");
    signup(&h, &mut owner, "owner@acme.test", "Acme").await;
    let mk = |name: &str, scopes: Value| json!({"name": name, "scopes": scopes});
    let read = ok(&h, &mut owner, "POST", "/api/api-keys", Some(mk("read", json!(["read"])))).await["key"].as_str().unwrap().to_string();
    let aud = ok(&h, &mut owner, "POST", "/api/api-keys", Some(mk("audience", json!(["audience"])))).await["key"].as_str().unwrap().to_string();
    let msg = ok(&h, &mut owner, "POST", "/api/api-keys", Some(mk("msg", json!(["messaging:read"])))).await["key"].as_str().unwrap().to_string();
    let acct = ok(&h, &mut owner, "POST", "/api/api-keys", Some(mk("acct", json!(["account", "audience"])))).await["key"].as_str().unwrap().to_string();
    assert_eq!(status(&h, &mut owner, "POST", "/api/api-keys", Some(mk("bad", json!(["admin"])))).await, StatusCode::BAD_REQUEST);
    let c = ok(&h, &mut owner, "POST", "/api/campaigns", Some(draft_body())).await;

    assert_eq!(bearer(&h, &read, "GET", "/api/lists", None).await.0, StatusCode::OK);
    let (st, err) = bearer(&h, &read, "POST", "/api/lists", Some(json!({"name": "x"}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(err["error"]["message"].as_str().unwrap().contains("audience scope"), "{err}");
    assert_eq!(bearer(&h, &aud, "POST", "/api/lists", Some(json!({"name": "x"}))).await.0, StatusCode::OK);
    assert_eq!(bearer(&h, &aud, "GET", "/api/campaigns", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(bearer(&h, &msg, "GET", "/api/campaigns", None).await.0, StatusCode::OK);
    assert_eq!(bearer(&h, &msg, "POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), Some(json!({}))).await.0, StatusCode::FORBIDDEN);
    // Keys never change security settings.
    assert_eq!(bearer(&h, &acct, "PATCH", "/api/team/security", Some(json!({"require_two_factor": false}))).await.0, StatusCode::FORBIDDEN);

    // MCP: a read-only key can't call a tool that writes.
    let (is_err, text, _) = mcp(&h, &read, "create_list", json!({"name": "via mcp"})).await;
    assert!(is_err && text.contains("forbidden"), "{text}");
    let (is_err, _, _) = mcp(&h, &read, "list_lists", json!({})).await;
    assert!(!is_err);
    let (is_err, _, _) = mcp(&h, &read, "delete", json!({"type": "campaign", "id": c["id"]})).await;
    assert!(is_err);
    // Key management over MCP needs the account scope, and can't make a stronger key.
    let (is_err, _, _) = mcp(&h, &read, "list_api_keys", json!({})).await;
    assert!(is_err);
    let (is_err, _, _) = mcp(&h, &aud, "create_api_key", json!({"name": "x"})).await;
    assert!(is_err);
    let (is_err, text, _) = mcp(&h, &acct, "create_api_key", json!({"name": "escalate", "scopes": ["full"]})).await;
    assert!(is_err && text.contains("scopes it has"), "{text}");
    let (is_err, _, made) = mcp(&h, &acct, "create_api_key", json!({"name": "narrower", "scopes": ["audience:read"], "expires_in_days": 30})).await;
    assert!(!is_err, "{made}");
    assert_eq!(made["scopes"], json!(["audience:read"]));
    assert!(made["expires_at"].is_string());
    let narrow = made["key"].as_str().unwrap().to_string();
    assert_eq!(bearer(&h, &narrow, "GET", "/api/lists", None).await.0, StatusCode::OK);
    let (is_err, _, listed) = mcp(&h, &acct, "list_api_keys", json!({})).await;
    assert!(!is_err);
    let row = listed["data"].as_array().unwrap().iter().find(|k| k["name"] == "narrower").unwrap().clone();
    assert_eq!(row["created_by"], "owner@acme.test", "keys made by keys belong to the member behind them");
    assert!(!listed.to_string().contains(&narrow), "keys are never shown again");
    let (is_err, _, dry) = mcp(&h, &acct, "revoke_api_key", json!({"id": row["id"]})).await;
    assert!(!is_err && dry["dry_run"] == true);
    let (is_err, _, _) = mcp(&h, &acct, "revoke_api_key", json!({"id": row["id"], "confirm": true})).await;
    assert!(!is_err);
    assert_eq!(bearer(&h, &narrow, "GET", "/api/lists", None).await.0, StatusCode::UNAUTHORIZED);

    // Expired keys stop working.
    sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second' WHERE name = 'msg'").execute(&h.state.db).await.unwrap();
    assert_eq!(bearer(&h, &msg, "GET", "/api/campaigns", None).await.0, StatusCode::UNAUTHORIZED);

    // A key can't do more than the member who made it, and stops when they leave.
    let (mut admin, admin_id) = join(&h, &mut owner, "admin@acme.test", "admin").await;
    let ak = ok(&h, &mut admin, "POST", "/api/api-keys", Some(json!({"name": "admin full"}))).await["key"].as_str().unwrap().to_string();
    assert_eq!(bearer(&h, &ak, "POST", "/api/lists", Some(json!({"name": "y"}))).await.0, StatusCode::OK);
    ok(&h, &mut owner, "PATCH", &format!("/api/team/members/{admin_id}"), Some(json!({"role": "analyst"}))).await;
    assert_eq!(bearer(&h, &ak, "POST", "/api/lists", Some(json!({"name": "z"}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(bearer(&h, &ak, "GET", "/api/lists", None).await.0, StatusCode::OK);
    ok(&h, &mut owner, "DELETE", &format!("/api/team/members/{admin_id}"), None).await;
    assert_eq!(bearer(&h, &ak, "GET", "/api/lists", None).await.0, StatusCode::UNAUTHORIZED);

    // Keys from before scopes keep full access.
    let account_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let legacy = dumbmail::api::account::new_api_key(&h.state, account_id, "old").await.unwrap();
    let legacy = legacy["key"].as_str().unwrap();
    assert_eq!(bearer(&h, legacy, "POST", "/api/lists", Some(json!({"name": "legacy"}))).await.0, StatusCode::OK);
    assert_eq!(bearer(&h, legacy, "GET", "/api/billing", None).await.0, StatusCode::OK);
    let keys = ok(&h, &mut owner, "GET", "/api/api-keys", None).await;
    assert_eq!(keys["data"].as_array().unwrap().iter().find(|k| k["name"] == "old").unwrap()["scopes"], json!(["full"]));
    h.cleanup().await;
}

// ---- Audit log ----

#[tokio::test]
async fn audit_log_records_filters_and_exports() {
    let h = harness!();
    let mut owner = S::new("198.51.100.50");
    signup(&h, &mut owner, "owner@acme.test", "Acme").await;
    assert_eq!(status(&h, &mut S::new("198.51.100.51"), "POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "not it at all"}))).await, StatusCode::BAD_REQUEST);
    let list = ok(&h, &mut owner, "POST", "/api/lists", Some(json!({"name": "Doomed"}))).await;
    let list_id = list["id"].as_str().unwrap().to_string();
    ok(&h, &mut owner, "DELETE", &format!("/api/lists/{list_id}"), None).await;
    let c = ok(&h, &mut owner, "POST", "/api/campaigns", Some(draft_body())).await;
    ok(&h, &mut owner, "DELETE", &format!("/api/campaigns/{}", c["id"].as_str().unwrap()), None).await;
    ok(&h, &mut owner, "PATCH", "/api/account", Some(json!({"name": "Acme Co"}))).await;
    let key = ok(&h, &mut owner, "POST", "/api/api-keys", Some(json!({"name": "robot", "scopes": ["content", "audience"]}))).await;
    let k = key["key"].as_str().unwrap();
    let (is_err, _, _) = mcp(&h, k, "create_template", json!({"name": "T", "channel": "email", "content": {"subject": "Private subject", "blocks": [{"type": "text", "html": "<p>secret body</p>"}]}})).await;
    assert!(!is_err);
    let (is_err, _, _) = mcp(&h, k, "create_list", json!({"name": "From MCP"})).await;
    assert!(!is_err);
    let (_, mut analyst) = (0, join(&h, &mut owner, "analyst@acme.test", "analyst").await.0);

    let v = ok(&h, &mut owner, "GET", "/api/audit?action=list.delete", None).await;
    assert_eq!(v["total"], 1, "{v}");
    let e = &v["data"][0];
    assert_eq!((e["target"].as_str(), e["actor"]["label"].as_str(), e["actor"]["type"].as_str(), e["ip"].as_str()), (Some(list_id.as_str()), Some("owner@acme.test"), Some("user"), Some("198.51.100.50")));
    for action in ["login.failed", "campaign.delete", "account.settings_change", "api_key.create", "team.invite", "team.join", "mcp.create_list", "account.signup"] {
        let v = ok(&h, &mut owner, "GET", &format!("/api/audit?action={action}"), None).await;
        assert!(v["total"].as_i64().unwrap() >= 1, "{action} not recorded");
    }
    let mcp_entry = ok(&h, &mut owner, "GET", "/api/audit?action=mcp.create_template", None).await;
    assert_eq!(mcp_entry["data"][0]["actor"]["label"], format!("robot ({}…)", key["prefix"].as_str().unwrap()));
    let all = ok(&h, &mut owner, "GET", "/api/audit?limit=200", None).await.to_string();
    assert!(!all.contains("secret body") && !all.contains("Private subject"), "message content never reaches the audit log");
    assert!(!all.contains(k), "keys never reach the audit log");
    let team = ok(&h, &mut owner, "GET", "/api/audit?action=team.", None).await;
    assert!(team["data"].as_array().unwrap().iter().all(|e| e["action"].as_str().unwrap().starts_with("team.")));
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).format("%Y-%m-%d").to_string();
    assert!(ok(&h, &mut owner, "GET", &format!("/api/audit?from={today}&to={today}"), None).await["total"].as_i64().unwrap() > 5);
    assert_eq!(ok(&h, &mut owner, "GET", "/api/audit?from=2020-01-01&to=2020-01-02", None).await["total"], 0);
    let (is_err, _, via_mcp) = mcp(&h, k, "get_audit_log", json!({"action": "list.delete"})).await;
    assert!(is_err, "the audit log needs the account scope: {via_mcp}");

    // CSV export, and the export is itself recorded.
    let (st, _, headers) = send(&h, &mut owner, "GET", "/api/audit/export?action=list.", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/csv"));
    let raw = {
        let req = Request::builder().uri("/api/audit/export").header(header::COOKIE, owner.cookie.clone().unwrap()).body(Body::empty()).unwrap();
        let res = h.router.clone().oneshot(req).await.unwrap();
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    };
    assert!(raw.starts_with("time,actor,actor_type,action,target,details,ip,user_agent"));
    assert!(raw.contains("list.delete"));
    assert!(ok(&h, &mut owner, "GET", "/api/audit?action=audit.export", None).await["total"].as_i64().unwrap() >= 1);
    assert_eq!(status(&h, &mut analyst, "GET", "/api/audit", None).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&h, &mut analyst, "GET", "/api/audit/export", None).await, StatusCode::FORBIDDEN);

    // Entries older than 13 months are pruned.
    let account_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO audit_log (id, account_id, created_at, actor_type, action) VALUES ($1, $2, now() - interval '14 months', 'system', 'ancient')").bind(Uuid::now_v7()).bind(account_id).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO audit_log (id, account_id, created_at, actor_type, action) VALUES ($1, $2, now() - interval '12 months', 'system', 'old_but_kept')").bind(Uuid::now_v7()).bind(account_id).execute(&h.state.db).await.unwrap();
    dumbmail::features::team::audit::prune(&h.state).await.unwrap();
    let left: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log WHERE action IN ('ancient', 'old_but_kept')").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(left, vec!["old_but_kept".to_string()]);
    h.cleanup().await;
}

// ---- Single sign-on (OpenID Connect) ----

mod oidc {
    use super::*;
    use openidconnect::core::{CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey};
    use openidconnect::{AccessToken, Audience, EmptyAdditionalClaims, EndUserEmail, IssuerUrl, JsonWebKeyId, Nonce, PrivateSigningKey, StandardClaims, SubjectIdentifier};
    use rsa::pkcs1::EncodeRsaPrivateKey;

    /// What the next token request for a code should answer with.
    #[derive(Clone)]
    pub struct Grant {
        pub email: String,
        pub sub: String,
        pub nonce: String,
        pub challenge: String,
        pub audience: String,
        pub bad_key: bool,
    }

    #[derive(Clone, Default)]
    pub struct Idp {
        pub base: String,
        pub grants: Arc<Mutex<HashMap<String, Grant>>>,
        pub keys: Arc<Mutex<Option<(String, String)>>>,
    }

    fn pem() -> String {
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string()
    }

    fn signing(pem: &str) -> CoreRsaPrivateSigningKey {
        CoreRsaPrivateSigningKey::from_pem(pem, Some(JsonWebKeyId::new("k1".into()))).unwrap()
    }

    /// A small OpenID Connect provider following the spec's discovery, JWKS and token endpoints.
    pub async fn start() -> Idp {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let idp = Idp { base: base.clone(), keys: Arc::new(Mutex::new(Some((pem(), pem())))), ..Default::default() };
        use axum::extract::{Form, State};
        use axum::routing::{get, post};
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State(i): State<Idp>| async move {
                    axum::Json(json!({
                        "issuer": i.base, "authorization_endpoint": format!("{}/authorize", i.base), "token_endpoint": format!("{}/token", i.base),
                        "jwks_uri": format!("{}/jwks", i.base), "response_types_supported": ["code"], "subject_types_supported": ["public"],
                        "id_token_signing_alg_values_supported": ["RS256"], "token_endpoint_auth_methods_supported": ["client_secret_basic"],
                    }))
                }),
            )
            .route(
                "/jwks",
                get(|State(i): State<Idp>| async move {
                    let (good, _) = i.keys.lock().unwrap().clone().unwrap();
                    axum::Json(serde_json::to_value(CoreJsonWebKeySet::new(vec![signing(&good).as_verification_key()])).unwrap())
                }),
            )
            .route(
                "/token",
                post(|State(i): State<Idp>, headers: axum::http::HeaderMap, Form(f): Form<HashMap<String, String>>| async move {
                    use base64::Engine;
                    use sha2::Digest;
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
                    let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("client-1:shh-its-a-secret"));
                    if auth != want {
                        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "invalid_client"})));
                    }
                    let Some(g) = f.get("code").and_then(|c| i.grants.lock().unwrap().remove(c)) else {
                        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"})));
                    };
                    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
                    if challenge != g.challenge || f.get("grant_type").map(String::as_str) != Some("authorization_code") {
                        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"})));
                    }
                    let (good, bad) = i.keys.lock().unwrap().clone().unwrap();
                    let key = signing(if g.bad_key { &bad } else { &good });
                    let at = AccessToken::new("access-123".into());
                    let claims = CoreIdTokenClaims::new(
                        IssuerUrl::new(i.base.clone()).unwrap(),
                        vec![Audience::new(g.audience.clone())],
                        chrono::Utc::now() + chrono::Duration::minutes(5),
                        chrono::Utc::now(),
                        StandardClaims::new(SubjectIdentifier::new(g.sub.clone())).set_email(Some(EndUserEmail::new(g.email.clone()))).set_email_verified(Some(true)),
                        EmptyAdditionalClaims {},
                    )
                    .set_nonce(Some(Nonce::new(g.nonce.clone())));
                    let id_token = CoreIdToken::new(claims, &key, CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256, Some(&at), None).unwrap();
                    (StatusCode::OK, axum::Json(json!({"access_token": "access-123", "token_type": "Bearer", "expires_in": 300, "id_token": id_token})))
                }),
            )
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        idp
    }
}

/// Start SSO for `email`, have the provider sign them in (tweaked by `grant`), and return the
/// login page error code or the new session.
async fn sso_round(h: &Harness, idp: &oidc::Idp, email: &str, sub: &str, tweak: impl FnOnce(&mut oidc::Grant)) -> Result<S, String> {
    let mut s = S::new("198.51.100.70");
    let (st, _, headers) = send(h, &mut s, "GET", &format!("/api/auth/sso?email={}", urlencode(email)), None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let loc = headers[header::LOCATION].to_str().unwrap().to_string();
    if !loc.starts_with(&idp.base) {
        return Err(loc);
    }
    let u = url::Url::parse(&loc).unwrap();
    let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["redirect_uri"], "https://pl.test/api/auth/sso/callback");
    assert!(q["scope"].contains("openid"));
    let sso_cookie = headers.get_all(header::SET_COOKIE).iter().map(|c| c.to_str().unwrap().split(';').next().unwrap().to_string()).find(|c| c.starts_with("pl_sso=")).unwrap();
    let mut g = oidc::Grant { email: email.into(), sub: sub.into(), nonce: q["nonce"].clone(), challenge: q["code_challenge"].clone(), audience: q["client_id"].clone(), bad_key: false };
    tweak(&mut g);
    let code = format!("code-{}", Uuid::now_v7().simple());
    idp.grants.lock().unwrap().insert(code.clone(), g);
    s.cookie = Some(sso_cookie);
    let (st, _, headers) = send(h, &mut s, "GET", &format!("/api/auth/sso/callback?code={code}&state={}", urlencode(&q["state"])), None).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let loc = headers[header::LOCATION].to_str().unwrap().to_string();
    if loc.contains("/login?error=") {
        return Err(loc.rsplit('=').next().unwrap().to_string());
    }
    assert_eq!(loc, "https://pl.test/");
    assert!(s.cookie.as_deref().is_some_and(|c| c.starts_with("pl_session=")));
    Ok(s)
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[tokio::test]
async fn single_sign_on_with_openid_connect() {
    let h = harness!();
    dumbmail::features::team::sso::allow_private_hosts_for_tests();
    let idp = oidc::start().await;
    let mut owner = S::new("198.51.100.60");
    let me = signup(&h, &mut owner, "owner@acme.test", "Acme").await;
    let account_id = me["account"]["id"].as_str().unwrap().to_string();
    let cfg = json!({"issuer": idp.base, "client_id": "client-1", "client_secret": "shh-its-a-secret", "domains": ["acme.test"], "default_role": "content"});

    // The domain must be the account's own (a verified sending domain).
    let (st, err) = call(&h, &mut owner, "PUT", "/api/team/sso", Some(cfg.clone())).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
    sqlx::query("INSERT INTO sending_domains (id, account_id, domain, status) VALUES ($1, $2::uuid, 'mail.acme.test', 'verified')").bind(Uuid::now_v7()).bind(&account_id).execute(&h.state.db).await.unwrap();
    assert_eq!(status(&h, &mut owner, "PUT", "/api/team/sso", Some(json!({"issuer": "http://127.0.0.1:1", "client_id": "c", "client_secret": "s", "domains": ["acme.test"]}))).await, StatusCode::BAD_REQUEST, "a provider we can't reach isn't saved");
    let saved = ok(&h, &mut owner, "PUT", "/api/team/sso", Some(cfg.clone())).await;
    assert_eq!(saved["sso"]["client_secret_hint"], "…cret");
    assert!(!saved.to_string().contains("shh-its-a-secret"));
    let enc: String = sqlx::query_scalar("SELECT client_secret_enc FROM account_sso").fetch_one(&h.state.db).await.unwrap();
    assert!(!enc.contains("shh"));
    // Another account can't claim the same domain.
    let mut rival = S::new("198.51.100.61");
    let r = signup(&h, &mut rival, "boss@rival.test", "Rival").await;
    sqlx::query("INSERT INTO sending_domains (id, account_id, domain, status) VALUES ($1, $2::uuid, 'acme.test', 'verified')").bind(Uuid::now_v7()).bind(r["account"]["id"].as_str().unwrap()).execute(&h.state.db).await.unwrap();
    assert_eq!(status(&h, &mut rival, "PUT", "/api/team/sso", Some(cfg.clone())).await, StatusCode::CONFLICT);
    // Members below admin can't see or change it; API keys never can.
    let (mut manager, _) = join(&h, &mut owner, "pat@acme.test", "manager").await;
    assert_eq!(status(&h, &mut manager, "GET", "/api/team/sso", None).await, StatusCode::FORBIDDEN);

    // Someone new signs in through the provider and joins with the default role.
    let mut jo = sso_round(&h, &idp, "jo@acme.test", "idp-jo", |_| {}).await.expect("sso sign-in");
    let me = ok(&h, &mut jo, "GET", "/api/me", None).await;
    assert_eq!((me["user"]["email"].as_str(), me["role"].as_str(), me["sign_in_method"].as_str(), me["account"]["id"].as_str()), (Some("jo@acme.test"), Some("content"), Some("sso"), Some(account_id.as_str())));
    ok(&h, &mut jo, "GET", "/api/campaigns", None).await;
    // An SSO session is only good for this account: it can't change the login or open other accounts.
    for (m, p) in [("POST", "/api/me/two-factor/setup"), ("POST", "/api/accounts"), ("GET", "/api/me/sessions"), ("POST", "/api/me/password"), ("POST", "/api/accounts/switch")] {
        assert_eq!(status(&h, &mut jo, m, p, Some(json!({"name": "x", "account_id": r["account"]["id"]}))).await, StatusCode::FORBIDDEN, "{m} {p}");
    }
    // Signing in again finds the same login by its identity.
    let mut jo2 = sso_round(&h, &idp, "jo@acme.test", "idp-jo", |_| {}).await.unwrap();
    assert_eq!(ok(&h, &mut jo2, "GET", "/api/me", None).await["user"]["id"], me["user"]["id"]);

    // Tokens that don't check out are refused.
    assert_eq!(sso_round(&h, &idp, "jo@acme.test", "idp-jo", |g| g.nonce = "replayed".into()).await.err().as_deref(), Some("sso_failed"));
    assert_eq!(sso_round(&h, &idp, "jo@acme.test", "idp-jo", |g| g.bad_key = true).await.err().as_deref(), Some("sso_failed"));
    assert_eq!(sso_round(&h, &idp, "jo@acme.test", "idp-jo", |g| g.audience = "someone-else".into()).await.err().as_deref(), Some("sso_failed"));
    assert_eq!(sso_round(&h, &idp, "jo@acme.test", "idp-jo", |g| g.challenge = "not-the-challenge".into()).await.err().as_deref(), Some("sso_failed"));
    // The provider can't sign people in on domains this account doesn't own.
    assert_eq!(sso_round(&h, &idp, "jo@acme.test", "idp-evil", |g| g.email = "ceo@rival.test".into()).await.err().as_deref(), Some("sso_domain"));
    // Unknown email domains don't reach a provider at all.
    assert!(sso_round(&h, &idp, "x@nowhere.test", "x", |_| {}).await.err().unwrap().ends_with("error=sso_unavailable"));
    // A mismatched state is refused.
    let mut s = S::new("198.51.100.71");
    let (_, _, hd) = send(&h, &mut s, "GET", "/api/auth/sso?email=jo%40acme.test", None).await;
    s.cookie = hd.get_all(header::SET_COOKIE).iter().map(|c| c.to_str().unwrap().split(';').next().unwrap().to_string()).find(|c| c.starts_with("pl_sso="));
    let (_, _, hd) = send(&h, &mut s, "GET", "/api/auth/sso/callback?code=x&state=forged", None).await;
    assert!(hd[header::LOCATION].to_str().unwrap().ends_with("error=sso_expired"));

    // Without auto-join, only existing members can use it.
    ok(&h, &mut owner, "PUT", "/api/team/sso", Some(json!({"issuer": idp.base, "client_id": "client-1", "domains": ["acme.test"], "auto_join": false, "enforce": true}))).await;
    assert_eq!(sso_round(&h, &idp, "new@acme.test", "idp-new", |_| {}).await.err().as_deref(), Some("sso_not_member"));
    // Enforced: members on the domain must use SSO, owners keep their password.
    assert_eq!(ok(&h, &mut manager, "GET", "/api/me", None).await["restriction"], "sso_required");
    assert_eq!(status(&h, &mut manager, "GET", "/api/campaigns", None).await, StatusCode::FORBIDDEN);
    assert!(ok(&h, &mut owner, "GET", "/api/me", None).await["restriction"].is_null());
    let mut pat = sso_round(&h, &idp, "pat@acme.test", "idp-pat", |_| {}).await.unwrap();
    assert_eq!(ok(&h, &mut pat, "GET", "/api/me", None).await["role"], "manager");
    ok(&h, &mut pat, "GET", "/api/campaigns", None).await;

    let logins = ok(&h, &mut owner, "GET", "/api/audit?action=login", None).await;
    assert!(logins["data"].as_array().unwrap().iter().any(|e| e["details"]["method"] == "sso"));
    assert!(ok(&h, &mut owner, "GET", "/api/audit?action=sso.configure", None).await["total"].as_i64().unwrap() == 1);
    ok(&h, &mut owner, "DELETE", "/api/team/sso", None).await;
    assert_eq!(sso_round(&h, &idp, "jo@acme.test", "idp-jo", |_| {}).await.err().map(|e| e.ends_with("sso_unavailable")), Some(true));
    h.cleanup().await;
}

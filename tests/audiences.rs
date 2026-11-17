//! Ad audience sync against local stand-ins for the Meta Marketing API, Google's OAuth and Data
//! Manager API, and TikTok's API for Business, following their documented request and response
//! shapes. Several tests drive a run one step at a time to put an opt-out, a deletion or a crash
//! exactly where it hurts.

#[macro_use]
mod common;
use common::*;

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use base64::Engine;
use dumbmail::features::audiences::{store, sync};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[derive(Debug, Clone)]
struct Call {
    method: String,
    path: String,
    query: String,
    headers: HeaderMap,
    body: String,
}

impl Call {
    fn form(&self) -> std::collections::HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes()).into_owned().collect()
    }
    fn query(&self) -> std::collections::HashMap<String, String> {
        url::form_urlencoded::parse(self.query.as_bytes()).into_owned().collect()
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Default)]
struct MockState {
    calls: Vec<Call>,
    /// Responses to give, in order, to the next calls whose path ends with the key.
    fail: Vec<(String, u16, Value)>,
    /// Google requestStatus:retrieve answers, in order; SUCCESS when empty.
    google_status: VecDeque<Value>,
    meta_audiences: i64,
    google_requests: i64,
}

type Mock = Arc<Mutex<MockState>>;

trait MockExt {
    fn calls(&self, path_end: &str) -> Vec<Call>;
    fn fail_next(&self, path_end: &str, status: u16, body: Value);
    fn google_status_next(&self, body: Value);
    fn clear(&self);
    fn all_bodies(&self) -> String;
}

impl MockExt for Mock {
    fn calls(&self, path_end: &str) -> Vec<Call> {
        self.lock().unwrap().calls.iter().filter(|c| c.path.ends_with(path_end)).cloned().collect()
    }
    fn fail_next(&self, path_end: &str, status: u16, body: Value) {
        self.lock().unwrap().fail.push((path_end.to_string(), status, body));
    }
    fn google_status_next(&self, body: Value) {
        self.lock().unwrap().google_status.push_back(body);
    }
    fn clear(&self) {
        self.lock().unwrap().calls.clear();
    }
    fn all_bodies(&self) -> String {
        self.lock().unwrap().calls.iter().map(|c| format!("{} {} {}", c.path, c.query, c.body)).collect::<Vec<_>>().join("\n")
    }
}

fn fake_id_token(aud: &str, email: &str) -> String {
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({"iss": "https://accounts.google.com", "aud": aud, "sub": "g-123", "email": email, "email_verified": true, "exp": chrono::Utc::now().timestamp() + 3600});
    format!("e30.{}.sig", b.encode(claims.to_string()))
}

const DM: &str = "/datamanager/accountTypes/GOOGLE_ADS/accounts/1234567890";

async fn respond(m: Mock, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> (StatusCode, axum::Json<Value>) {
    let path = uri.path().to_string();
    let call = Call { method: method.to_string(), path: path.clone(), query: uri.query().unwrap_or_default().to_string(), headers, body: String::from_utf8_lossy(&body).into_owned() };
    let mut st = m.lock().unwrap();
    st.calls.push(call.clone());
    if let Some(i) = st.fail.iter().position(|(k, _, _)| path.ends_with(k.as_str())) {
        let (_, code, body) = st.fail.remove(i);
        return (StatusCode::from_u16(code).unwrap(), axum::Json(body));
    }
    let ok = |v: Value| (StatusCode::OK, axum::Json(v));
    let m_ = method.as_str();
    match (m_, path.as_str()) {
        // ----- Meta Graph API -----
        ("GET", "/meta/oauth/access_token") => {
            let q = call.query();
            if q.get("grant_type").map(String::as_str) == Some("fb_exchange_token") {
                ok(json!({"access_token": "meta-long", "token_type": "bearer", "expires_in": 5_184_000}))
            } else {
                assert_eq!(q.get("redirect_uri").map(String::as_str), Some("https://pl.test/api/ad-audiences/oauth/meta/callback"));
                ok(json!({"access_token": "meta-short", "token_type": "bearer", "expires_in": 3600}))
            }
        }
        ("GET", "/meta/me") => ok(json!({"id": "1", "name": "Ann Admin"})),
        ("GET", "/meta/me/adaccounts") => ok(json!({"data": [{"id": "act_111", "account_id": "111", "name": "Acme Ads"}], "paging": {"cursors": {"before": "a", "after": "b"}}})),
        // An audience the customer made by hand, with text that looks like DumbMail's: never adopted.
        ("GET", "/meta/act_111/customaudiences") => ok(json!({"data": [{"id": "5999", "name": "VIPs (DumbMail)", "description": "Kept in step by DumbMail"}]})),
        ("POST", "/meta/act_111/customaudiences") => {
            st.meta_audiences += 1;
            ok(json!({"id": format!("{}", 6000 + st.meta_audiences)}))
        }
        ("POST" | "DELETE", p) if p.starts_with("/meta/") && p.ends_with("/users") => {
            let f = call.form();
            let payload: Value = serde_json::from_str(f.get("payload").map(String::as_str).unwrap_or("{}")).unwrap();
            let session: Value = serde_json::from_str(f.get("session").map(String::as_str).unwrap_or("{}")).unwrap();
            let aud = p.trim_start_matches("/meta/").trim_end_matches("/users");
            ok(json!({"audience_id": aud, "session_id": session["session_id"], "num_received": payload["data"].as_array().map(Vec::len), "num_invalid_entries": 0, "invalid_entry_samples": {}}))
        }
        ("GET", p) if p.starts_with("/meta/600") => ok(json!({"id": "x", "approximate_count_lower_bound": 1000, "approximate_count_upper_bound": 1200, "delivery_status": {"code": 200, "description": "ready"}, "operation_status": {"code": 200, "description": "normal"}})),
        ("DELETE", p) if p.starts_with("/meta/600") => ok(json!({"success": true})),
        ("DELETE", "/meta/me/permissions") => ok(json!({"success": true})),
        // ----- Google OAuth and Data Manager API -----
        ("POST", "/google/token") => {
            let f = call.form();
            if f.get("grant_type").map(String::as_str) == Some("refresh_token") {
                assert_eq!(f.get("refresh_token").map(String::as_str), Some("g-refresh"));
                ok(json!({"access_token": "g-access-2", "expires_in": 3599, "token_type": "Bearer"}))
            } else {
                ok(json!({"access_token": "g-access", "expires_in": 3599, "refresh_token": "g-refresh", "scope": "x", "token_type": "Bearer", "id_token": fake_id_token("gads-client", "ads@acme.test")}))
            }
        }
        ("POST", "/google/revoke") => ok(json!({})),
        ("GET", p) if p == format!("{DM}/userLists") => ok(json!({"userLists": []})),
        ("GET", p) if p.starts_with("/datamanager/accountTypes/GOOGLE_ADS/accounts/") && p.ends_with("/userLists") => {
            (StatusCode::FORBIDDEN, axum::Json(json!({"error": {"code": 403, "message": "The caller does not have permission", "status": "PERMISSION_DENIED"}})))
        }
        ("POST", p) if p == format!("{DM}/userLists") => ok(json!({"name": format!("{}/userLists/555", DM.trim_start_matches("/datamanager/")), "id": "555", "displayName": call.json()["displayName"]})),
        ("POST", "/datamanager/audienceMembers:ingest") | ("POST", "/datamanager/audienceMembers:remove") => {
            st.google_requests += 1;
            ok(json!({"requestId": format!("req-{}", st.google_requests)}))
        }
        ("GET", "/datamanager/requestStatus:retrieve") => match st.google_status.pop_front() {
            Some(v) => ok(v),
            None => ok(json!({"requestStatusPerDestination": [{"requestStatus": "SUCCESS", "audienceMembersIngestionStatus": {"userDataIngestionStatus": {"recordCount": "1"}}}]})),
        },
        ("GET", p) if p == format!("{DM}/userLists/555") => ok(json!({
            "id": "555", "sizeInfo": {"displayNetworkMembersCount": "0", "searchNetworkMembersCount": "0"},
            "ingestedUserListInfo": {"uploadKeyTypes": ["CONTACT_ID"], "contactIdInfo": {"matchRatePercentage": 40}}
        })),
        ("DELETE", p) if p == format!("{DM}/userLists/555") => ok(json!({})),
        // ----- TikTok API for Business -----
        ("POST", "/tiktok/oauth2/access_token/") => {
            let with_refresh = call.json()["auth_code"] == "tt-code";
            let mut data = json!({"access_token": "tt-access", "advertiser_ids": ["777"], "scope": [1, 2]});
            if with_refresh {
                data["refresh_token"] = json!("tt-refresh");
            }
            ok(json!({"code": 0, "message": "OK", "request_id": "1", "data": data}))
        }
        ("GET", "/tiktok/oauth2/advertiser/get/") => ok(json!({"code": 0, "message": "OK", "data": {"list": [{"advertiser_id": "777", "advertiser_name": "Acme TT"}]}})),
        ("POST", "/tiktok/oauth2/revoke_token/") => ok(json!({"code": 0, "message": "OK", "data": {}})),
        ("POST", "/tiktok/dmp/custom_audience/file/upload/") => ok(json!({"code": 0, "message": "OK", "data": {"file_path": format!("fp-{}", st.calls.len())}})),
        ("POST", "/tiktok/dmp/custom_audience/create/") => ok(json!({"code": 0, "message": "OK", "data": {"custom_audience_id": format!("tt-aud-{}", st.calls.len())}})),
        ("POST", "/tiktok/dmp/custom_audience/update/") => ok(json!({"code": 0, "message": "OK", "data": {"custom_audience_id": call.json()["custom_audience_id"]}})),
        ("GET", "/tiktok/dmp/custom_audience/get/") => ok(json!({"code": 0, "message": "OK", "data": {"list": [{"audience_details": {"cover_num": 1500, "is_valid": true, "is_expired": false}}]}})),
        ("POST", "/tiktok/dmp/custom_audience/delete/") => ok(json!({"code": 0, "message": "OK", "data": {}})),
        _ => (StatusCode::NOT_FOUND, axum::Json(json!({"error": {"message": format!("mock has no {m_} {path}")}}))),
    }
}

async fn mock() -> (Mock, String) {
    let m: Mock = Default::default();
    let mm = m.clone();
    let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| respond(mm.clone(), method, uri, headers, body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, format!("http://{addr}"))
}

async fn harness_with_mock() -> Option<(Harness, Mock)> {
    let (m, base) = mock().await;
    let h = Harness::with(|c| {
        c.meta_app_id = Some("meta-app".into());
        c.meta_app_secret = Some("meta-secret".into());
        c.google_ads_client_id = Some("gads-client".into());
        c.google_ads_client_secret = Some("gads-secret".into());
        c.tiktok_app_id = Some("tt-app".into());
        c.tiktok_app_secret = Some("tt-secret".into());
        c.ad_platform_api_base = Some(base);
    })
    .await?;
    Some((h, m))
}

macro_rules! mock_harness {
    () => {
        match harness_with_mock().await {
            Some(x) => x,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

/// Sign in to a platform the way a browser would: start, then the callback with the state.
async fn connect(h: &mut Harness, platform: &str, code: &str) -> String {
    let start = h.ok("POST", &format!("/api/ad-audiences/connections/{platform}/start"), None).await;
    let url = url::Url::parse(start["url"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let st = q["state"].clone();
    let cookie = h.cookie.clone().unwrap();
    let param = if platform == "tiktok" { "auth_code" } else { "code" };
    let (s, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/{platform}/callback?{param}={code}&state={st}"), &[("cookie", &cookie)], "").await;
    assert!(s.is_redirection(), "{s}");
    headers.get("location").unwrap().to_str().unwrap().to_string()
}

async fn profile(h: &mut Harness, body: Value) -> String {
    h.ok("POST", "/api/profiles", Some(body)).await["id"].as_str().unwrap().to_string()
}

/// A profile subscribed to email, in a list.
async fn sub(h: &mut Harness, email: &str, list: &str, extra: Value) -> String {
    let mut b = json!({"email": email, "email_consent": "subscribed", "list_ids": [list]});
    if let (Some(o), Some(e)) = (b.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    profile(h, b).await
}

async fn sync_status(h: &mut Harness, id: &str) -> Value {
    h.ok("GET", &format!("/api/ad-audiences/syncs/{id}"), None).await
}

async fn run(h: &mut Harness, id: &str) {
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
}

async fn new_list(h: &mut Harness, name: &str) -> String {
    h.ok("POST", "/api/lists", Some(json!({"name": name}))).await["id"].as_str().unwrap().to_string()
}

async fn meta_sync(h: &mut Harness, list: &str) -> String {
    h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await["id"].as_str().unwrap().to_string()
}

fn meta_rows(calls: &[Call]) -> Vec<Vec<String>> {
    calls
        .iter()
        .flat_map(|c| {
            let p: Value = serde_json::from_str(&c.form()["payload"]).unwrap();
            serde_json::from_value::<Vec<Vec<String>>>(p["data"].clone()).unwrap()
        })
        .collect()
}

/// The hashed emails in Meta calls of one method.
fn meta_emails(m: &Mock, method: &str) -> BTreeSet<String> {
    meta_rows(&m.calls("/users").into_iter().filter(|c| c.method == method).collect::<Vec<_>>()).into_iter().map(|r| r[0].clone()).filter(|e| !e.is_empty()).collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|e| sha(e)).collect()
}

async fn key_states(h: &Harness) -> Vec<(String, String)> {
    sqlx::query_as("SELECT key_hash, state FROM ad_audience_members ORDER BY key_hash").fetch_all(&h.state.db).await.unwrap()
}

/// Run the sync's current run by hand, one step at a time, until it reaches `phase` (or ends, for "").
async fn drive(h: &Harness, phase: &str) -> Uuid {
    let run: Uuid = sqlx::query_scalar("SELECT id FROM ad_audience_runs WHERE status = 'running' ORDER BY started_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    for _ in 0..200 {
        let (p, status): (String, String) = sqlx::query_as("SELECT phase, status FROM ad_audience_runs WHERE id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
        if status != "running" || (!phase.is_empty() && p == phase) {
            return run;
        }
        sqlx::query("DELETE FROM jobs WHERE kind = 'audiences_run'").execute(&h.state.db).await.unwrap();
        sync::step(&h.state, run).await.unwrap();
    }
    panic!("the run didn't reach {phase:?}");
}

async fn set_props(h: &Harness, email: &str, props: Value) {
    sqlx::query("UPDATE profiles SET properties = properties || $2, updated_at = now() WHERE email = $1").bind(email).bind(props).execute(&h.state.db).await.unwrap();
}

#[tokio::test]
async fn meta_only_sends_subscribed_people_and_keeps_in_step() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    let loc = connect(&mut h, "meta", "meta-code").await;
    assert_eq!(loc, "/settings/ad-audiences?connected=meta");
    let conns = h.ok("GET", "/api/ad-audiences/connections", None).await;
    assert!(!conns.to_string().contains("meta-long") && !conns.to_string().contains("token_hint"), "no token or hint comes back: {conns}");
    assert_eq!(m.calls("/meta/me")[0].query()["appsecret_proof"].len(), 64);

    let list = new_list(&mut h, "VIPs").await;
    let ann = sub(&mut h, " Ann@Example.com", &list, json!({"phone_number": "+15035550100", "sms_consent": "subscribed"})).await;
    let gil = sub(&mut h, "gil@example.com", &list, json!({"phone_number": "+15035550199"})).await;
    profile(&mut h, json!({"email": "never@example.com", "list_ids": [list]})).await;
    profile(&mut h, json!({"email": "bob@example.com", "email_consent": "unsubscribed", "list_ids": [list]})).await;
    let cat = sub(&mut h, "cat@example.com", &list, json!({})).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard bounce' WHERE id = $1::uuid").bind(&cat).execute(&h.state.db).await.unwrap();
    sub(&mut h, "dan@example.com", &list, json!({"properties": {"ads_opt_out": true}})).await;
    sub(&mut h, "eve@example.com", &list, json!({"properties": {"do_not_sell": "true"}})).await;

    // Terms must be accepted, and the ad account must be one the sign-in can see.
    let body = json!({"platform": "meta", "ad_account_id": "act_111", "source_type": "list", "source_id": list});
    let (s, v) = h.req("POST", "/api/ad-audiences/syncs", Some(body.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let mut bad = body.clone();
    bad["ad_account_id"] = json!("222");
    bad["accept_terms"] = json!(true);
    assert_eq!(h.req("POST", "/api/ad-audiences/syncs", Some(bad)).await.0, StatusCode::BAD_REQUEST);
    let mut old = body.clone();
    old["include_unsubscribed"] = json!(true);
    old["accept_terms"] = json!(true);
    assert_eq!(h.req("POST", "/api/ad-audiences/syncs", Some(old)).await.0, StatusCode::BAD_REQUEST, "there is no way to include unsubscribed people");
    let id = meta_sync(&mut h, &list).await;
    assert_eq!(h.req("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await.0, StatusCode::CONFLICT);

    h.drain().await;
    assert!(m.calls("/meta/act_111/customaudiences").iter().all(|c| c.method == "POST"), "never looks for an existing audience to adopt");
    let f = m.calls("/meta/act_111/customaudiences")[0].form();
    assert_eq!((f["subtype"].as_str(), f["customer_file_source"].as_str(), f["name"].as_str()), ("CUSTOM", "USER_PROVIDED_ONLY", "VIPs (DumbMail)"));
    let adds = m.calls("/meta/6001/users");
    assert_eq!(adds.len(), 1);
    let payload: Value = serde_json::from_str(&adds[0].form()["payload"]).unwrap();
    assert_eq!(payload["schema"], json!(["EMAIL", "PHONE"]));
    let session: Value = serde_json::from_str(&adds[0].form()["session"]).unwrap();
    assert_eq!((session["batch_seq"].as_i64(), session["last_batch_flag"].as_bool()), (Some(1), Some(true)));
    assert!(session.get("estimated_num_total").is_none());
    let mut rows = meta_rows(&adds);
    rows.sort();
    let mut want = vec![vec![sha("ann@example.com"), String::new()], vec![String::new(), sha("15035550100")], vec![sha("gil@example.com"), String::new()]];
    want.sort();
    assert_eq!(rows, want, "only subscribed, unsuppressed people who didn't opt out; Gil's phone wasn't opted in to texts");
    let everything = m.all_bodies();
    for raw in ["ann@example.com", "never@example.com", "15035550100", "Ann@Example"] {
        assert!(!everything.contains(raw), "raw identifier {raw} left the server");
    }
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["sent"].as_i64(), st["confirmed"].as_i64(), st["pending_removal"].as_i64(), st["last_added"].as_i64()), (Some(3), Some(3), Some(0), Some(3)), "{st}");
    assert_eq!((st["platform_size"].as_i64(), st["platform_status"].as_str(), st["audience_id"].as_str()), (Some(1000), Some("Ready"), Some("6001")));
    assert_eq!(st["verified_live"], false);
    assert_eq!(st["runs"][0]["status"], "done");

    // Nothing changed: a re-run sends nothing.
    m.clear();
    run(&mut h, &id).await;
    assert!(m.calls("/users").is_empty(), "idempotent re-run");

    // Ann leaves, Gil is deleted, Hal joins, Ivy joins with GPC: exact removals with the same hashes.
    m.clear();
    h.ok("DELETE", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [ann]}))).await;
    h.ok("DELETE", &format!("/api/profiles/{gil}"), None).await;
    sub(&mut h, "hal@example.com", &list, json!({})).await;
    sub(&mut h, "ivy@example.com", &list, json!({"properties": {"gpc": true}})).await;
    run(&mut h, &id).await;
    let removes: Vec<Call> = m.calls("/meta/6001/users").into_iter().filter(|c| c.method == "DELETE").collect();
    let mut removed = meta_rows(&removes);
    removed.sort();
    assert_eq!(removed, want, "removals use exactly the hashes that were sent, even for the deleted profile");
    assert_eq!(meta_emails(&m, "POST"), set(&["hal@example.com"]));
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["confirmed"].as_i64(), st["last_added"].as_i64(), st["last_removed"].as_i64()), (Some(1), Some(1), Some(3)), "{st}");

    // Pausing stops additions but not removals: Hal opts out while paused and is still removed.
    let paused = h.ok("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"status": "paused"}))).await;
    assert_eq!((paused["status"].as_str(), paused["pause_reason"].as_str()), (Some("paused"), Some("user")));
    assert_eq!(h.req("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await.0, StatusCode::BAD_REQUEST);
    m.clear();
    sub(&mut h, "joy@example.com", &list, json!({})).await;
    set_props(&h, "hal@example.com", json!({"ads_opt_out": "yes"})).await;
    sqlx::query("UPDATE ad_audience_syncs SET next_run_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    dumbmail::features::tick(&h.state, 0).await;
    h.drain().await;
    assert_eq!(meta_emails(&m, "DELETE"), set(&["hal@example.com"]));
    assert!(meta_emails(&m, "POST").is_empty(), "nobody is added while paused");

    // Deleting needs confirm=true over REST too; with it, the audience goes on the platform first.
    let dry = h.ok("DELETE", &format!("/api/ad-audiences/syncs/{id}?delete_on_platform=true"), None).await;
    assert_eq!(dry["dry_run"], true);
    assert!(m.calls("/meta/6001").iter().all(|c| c.method != "DELETE"));
    let del = h.ok("DELETE", &format!("/api/ad-audiences/syncs/{id}?delete_on_platform=true&confirm=true"), None).await;
    assert_eq!(del["deleted_on_platform"], true);
    assert_eq!(m.calls("/meta/6001").iter().filter(|c| c.method == "DELETE").count(), 1);

    // Disconnect: tokens deleted, and whether Meta confirmed the revoke is reported.
    let d = h.ok("DELETE", "/api/ad-audiences/connections/meta", None).await;
    assert_eq!(d["revoked"], true);
    let (status, tok, generation): (String, Option<String>, i64) = sqlx::query_as("SELECT status, access_token_enc, generation FROM ad_connections").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok, generation), ("disconnected", None, 2));
    h.cleanup().await;
}

#[tokio::test]
async fn every_opt_out_spelling_keeps_people_out() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "All").await;
    let opted_out = [
        json!({"ads_opt_out": true}),
        json!({"ads_opt_out": " true "}),
        json!({"ads_opt_out": "yes"}),
        json!({"ads_opt_out": 1}),
        json!({"ads_opt_out": "denied"}),
        json!({"ads_opt_out": {}}),
        json!({"ads_opt_out": "false\u{a0}"}),
        json!({"do_not_sell": " TRUE "}),
        json!({"do_not_sell": "Y"}),
        json!({"gpc": "1"}),
        json!({"gpc": true}),
        json!({"ads_consent": false}),
        json!({"ads_consent": " false "}),
        json!({"ads_consent": "No"}),
        json!({"ads_consent": "0"}),
        json!({"ads_consent": ""}),
    ];
    let allowed = [
        json!({"ads_opt_out": false}),
        json!({"ads_opt_out": "false"}),
        json!({"ads_opt_out": " FALSE "}),
        json!({"ads_opt_out": "0"}),
        json!({"ads_opt_out": 0}),
        json!({"ads_opt_out": "no"}),
        json!({"ads_opt_out": ""}),
        json!({"ads_opt_out": null, "gpc": "\tfalse\n"}),
        json!({"ads_consent": true}),
        json!({"ads_consent": "granted"}),
        json!({}),
    ];
    for (i, p) in opted_out.iter().enumerate() {
        sub(&mut h, &format!("out{i}@x.test"), &list, json!({"properties": p})).await;
    }
    let mut want = Vec::new();
    for (i, p) in allowed.iter().enumerate() {
        let e = format!("in{i}@x.test");
        sub(&mut h, &e, &list, json!({"properties": p})).await;
        want.push(e);
    }
    let pending = sub(&mut h, "pending@x.test", &list, json!({})).await;
    sqlx::query("UPDATE profiles SET email_consent = 'pending' WHERE id = $1::uuid").bind(&pending).execute(&h.state.db).await.unwrap();
    profile(&mut h, json!({"email": "never@x.test", "list_ids": [list]})).await;

    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let dry = mcp(&h, &key, "create_audience_sync", json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list})).await;
    assert_eq!(dry["people_now"].as_i64(), Some(allowed.len() as i64), "the estimate uses the same predicate: {dry}");
    meta_sync(&mut h, &list).await;
    h.drain().await;
    let wanted: Vec<&str> = want.iter().map(String::as_str).collect();
    assert_eq!(meta_emails(&m, "POST"), set(&wanted));
    h.cleanup().await;
}

#[tokio::test]
async fn browser_events_cannot_add_anyone() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "Buyers").await;
    sub(&mut h, "ann@example.com", &list, json!({})).await;
    let segment = h
        .ok("POST", "/api/segments", Some(json!({"name": "Everyone with email", "definition": {"conditions": [{"type": "property", "field": "tier", "op": "eq", "value": "vip"}]}})))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let key: String = sqlx::query_scalar("SELECT public_key FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let event = |profile: Value| json!({"metric": "Viewed Product", "profile": profile}).to_string();
    // A browser makes a new profile that matches the segment and claims consent: never subscribed.
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &event(json!({"email": "forged@evil.test", "email_consent": "subscribed", "properties": {"tier": "vip", "ads_consent": true}}))).await;
    assert!(s.is_success());
    // And fills in a phone number and location on a real subscriber.
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={key}"), &[("content-type", "application/json")], &event(json!({"email": "ann@example.com", "phone_number": "+15035550111", "properties": {"tier": "vip"}}))).await;
    assert!(s.is_success());
    meta_sync(&mut h, &list).await;
    h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "segment", "source_id": segment, "accept_terms": true}))).await;
    h.drain().await;
    let rows = meta_rows(&m.calls("/users"));
    assert!(rows.iter().all(|r| r[1].is_empty()), "the browser-filled phone number wasn't opted in, so it's never sent: {rows:?}");
    assert_eq!(meta_emails(&m, "POST"), set(&["ann@example.com"]), "the forged profile isn't subscribed");
    // ads_consent from a browser grants nothing on Google either: Google needs the account setting.
    connect(&mut h, "google", "g-code").await;
    let (s, v) = h.req("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "google", "ad_account_id": "1234567890", "source_type": "list", "source_id": list, "accept_terms": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("owner or an admin"), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn opt_outs_and_deletions_after_staging_are_never_uploaded() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "VIPs").await;
    sub(&mut h, "ann@example.com", &list, json!({})).await;
    sub(&mut h, "bea@example.com", &list, json!({})).await;
    let cy = sub(&mut h, "cy@example.com", &list, json!({})).await;
    let dee = sub(&mut h, "dee@example.com", &list, json!({})).await;
    let id = meta_sync(&mut h, &list).await;

    // Everyone is staged; then, before the first batch is claimed, Bea opts out, Cy is deleted and
    // Dee changes address.
    drive(&h, "add").await;
    let staged: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_desired WHERE NOT veto").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(staged, 4);
    set_props(&h, "bea@example.com", json!({"do_not_sell": " TRUE "})).await;
    h.ok("DELETE", &format!("/api/profiles/{cy}"), None).await;
    sqlx::query("UPDATE profiles SET email = 'dee.new@example.com' WHERE id = $1::uuid").bind(&dee).execute(&h.state.db).await.unwrap();
    drive(&h, "").await;
    assert_eq!(meta_emails(&m, "POST"), set(&["ann@example.com"]), "only Ann: the batch claim re-checks everyone against the live rules");

    // Ann is on Meta now. In the next run she opts out after the run staged her: she's removed in
    // that same run.
    m.clear();
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    drive(&h, "add").await;
    set_props(&h, "ann@example.com", json!({"gpc": " TRUE "})).await;
    drive(&h, "").await;
    assert_eq!(meta_emails(&m, "DELETE"), set(&["ann@example.com"]));
    // Dee's new address was staged by this run, so it goes out as itself.
    assert_eq!(meta_emails(&m, "POST"), set(&["dee.new@example.com"]));
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["confirmed"].as_i64(), st["pending_removal"].as_i64()), (Some(1), Some(0)), "{st}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_crash_after_the_platform_call_loses_nothing() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "VIPs").await;
    sub(&mut h, "ann@example.com", &list, json!({})).await;
    let bea = sub(&mut h, "bea@example.com", &list, json!({})).await;

    // Meta's answer can't be read (no num_received): nothing is recorded as confirmed.
    m.fail_next("/users", 200, json!({"audience_id": "6001"}));
    let id = meta_sync(&mut h, &list).await;
    h.drain().await;
    let batches: Vec<(Uuid, String, i32, i64)> = sqlx::query_as("SELECT id, status, attempts, meta_session_id FROM ad_audience_batches").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!((batches[0].1.as_str(), batches[0].2), ("pending", 1), "written before the call, still unsettled");
    assert!(key_states(&h).await.iter().all(|(_, s)| s == "adding"), "not in the snapshot as confirmed");
    // The worker that sent it died holding the lease: nobody else touches the sync until it expires.
    sqlx::query("UPDATE ad_audience_syncs SET lease_token = gen_random_uuid(), lease_until = now() + interval '10 minutes'").execute(&h.state.db).await.unwrap();
    m.clear();
    h.fast_forward().await;
    assert!(m.calls("/users").is_empty(), "a live lease fences other workers");
    // The lease expires and the scheduler picks the run up again: the same batch is resent, with a
    // fresh Meta session (never a reused sequence number), and confirmed.
    sqlx::query("UPDATE ad_audience_syncs SET lease_until = now() - interval '1 second'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE ad_audience_runs SET heartbeat_at = now() - interval '3 hours'").execute(&h.state.db).await.unwrap();
    dumbmail::features::tick(&h.state, 0).await;
    h.drain().await;
    let resent = m.calls("/meta/6001/users");
    assert_eq!(resent.len(), 1);
    let session: Value = serde_json::from_str(&resent[0].form()["session"]).unwrap();
    assert_eq!(session["batch_seq"], 1);
    assert_ne!(session["session_id"].as_i64(), Some(batches[0].3));
    assert_eq!(meta_emails(&m, "POST"), set(&["ann@example.com", "bea@example.com"]));
    let after: Vec<(Uuid, String, i32)> = sqlx::query_as("SELECT id, status, attempts FROM ad_audience_batches").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(after, vec![(batches[0].0, "confirmed".to_string(), 2)], "the same batch, settled once");
    assert_eq!(sync_status(&mut h, &id).await["confirmed"], 2);

    // A removal whose answer is lost stays pending (and counted) until Meta confirms it.
    m.clear();
    h.ok("DELETE", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [bea]}))).await;
    m.fail_next("/users", 200, json!({"oops": true}));
    run(&mut h, &id).await;
    assert_eq!(key_states(&h).await.iter().filter(|(_, s)| s == "removing").count(), 1, "the removal isn't forgotten");
    h.fast_forward().await;
    assert_eq!(meta_emails(&m, "DELETE"), set(&["bea@example.com"]));
    assert_eq!(m.calls("/users").len(), 2, "resent once");
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["confirmed"].as_i64(), st["pending_removal"].as_i64()), (Some(1), Some(0)), "{st}");

    // An addition left unsettled is resent only if everyone in it still qualifies. Cy and Dee are
    // in a batch whose answer was lost; Cy opts out before the resend. The batch is given up on
    // (both count as possibly sent), Dee goes out in a new batch, and Cy is removed.
    m.clear();
    sub(&mut h, "cy@example.com", &list, json!({})).await;
    sub(&mut h, "dee@example.com", &list, json!({})).await;
    m.fail_next("/users", 200, json!({}));
    run(&mut h, &id).await;
    set_props(&h, "cy@example.com", json!({"ads_opt_out": true})).await;
    m.clear();
    h.fast_forward().await;
    assert_eq!(meta_emails(&m, "POST"), set(&["dee@example.com"]));
    assert_eq!(meta_emails(&m, "DELETE"), set(&["cy@example.com"]), "possibly sent, so removed");
    let abandoned: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_batches WHERE status = 'abandoned'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(abandoned, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn malformed_and_partial_answers_are_not_success() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "VIPs").await;
    sub(&mut h, "ann@example.com", &list, json!({})).await;
    sub(&mut h, "bea@example.com", &list, json!({})).await;

    // Meta counts one invalid entry and names it: only that one isn't confirmed.
    m.fail_next("/users", 200, json!({"audience_id": "6001", "num_received": 2, "num_invalid_entries": 1, "invalid_entry_samples": [sha("bea@example.com")]}));
    let id = meta_sync(&mut h, &list).await;
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["sent"].as_i64(), st["confirmed"].as_i64()), (Some(2), Some(1)), "{st}");
    assert_eq!(st["runs"][0]["rejected"], 1);
    assert!(key_states(&h).await.contains(&(sha("bea@example.com"), "unconfirmed".into())));
    // A count that doesn't match what was sent is unreadable: nothing confirmed.
    sub(&mut h, "cy@example.com", &list, json!({})).await;
    m.fail_next("/users", 200, json!({"audience_id": "6001", "num_received": 7, "num_invalid_entries": 0}));
    run(&mut h, &id).await;
    assert!(!key_states(&h).await.contains(&(sha("cy@example.com"), "confirmed".into())));
    assert_eq!(sync_status(&mut h, &id).await["running"], true, "waiting to resend");
    h.fast_forward().await;
    assert!(key_states(&h).await.contains(&(sha("cy@example.com"), "confirmed".into())));
    assert!(key_states(&h).await.contains(&(sha("bea@example.com"), "confirmed".into())), "the unconfirmed one is retried on a later run");

    // Google: no requestId is unreadable; a partial result leaves the failed record unconfirmed; a
    // failed removal stays pending.
    sqlx::query("DELETE FROM ad_audience_syncs").execute(&h.state.db).await.unwrap();
    connect(&mut h, "google", "g-code").await;
    h.ok("PUT", "/api/ad-audiences/settings", Some(json!({"google_consent": true}))).await;
    m.fail_next("audienceMembers:ingest", 200, json!({}));
    m.google_status_next(json!({"requestStatusPerDestination": [{"requestStatus": "PARTIAL_SUCCESS", "errorInfo": {"errorCounts": [{"recordCount": "1", "reason": "PROCESSING_ERROR_REASON_INVALID_FORMAT", "recordIndices": [1]}]}}]}));
    let gid = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "google", "ad_account_id": "1234567890", "source_type": "list", "source_id": list, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.drain().await;
    assert_eq!(sync_status(&mut h, &gid).await["running"], true);
    h.fast_forward().await;
    let ingests = m.calls("audienceMembers:ingest");
    assert_eq!(ingests.len(), 2, "resent after the unreadable answer");
    let second = ingests[1].json()["audienceMembers"][1]["userData"]["userIdentifiers"][0]["emailAddress"].as_str().unwrap().to_string();
    let st = sync_status(&mut h, &gid).await;
    assert_eq!((st["sent"].as_i64(), st["confirmed"].as_i64()), (Some(3), Some(2)), "{st}");
    assert!(key_states(&h).await.contains(&(second, "unconfirmed".into())));
    // Removing everyone: Google says the removal failed, so it stays pending removal.
    sqlx::query("DELETE FROM list_members").execute(&h.state.db).await.unwrap();
    m.google_status_next(json!({"requestStatusPerDestination": [{"requestStatus": "FAILED"}]}));
    run(&mut h, &gid).await;
    let st = sync_status(&mut h, &gid).await;
    assert_eq!((st["confirmed"].as_i64(), st["pending_removal"].as_i64()), (Some(0), Some(3)), "never shown as removed: {st}");
    run(&mut h, &gid).await;
    let st = sync_status(&mut h, &gid).await;
    assert_eq!(st["pending_removal"], 0, "retried and confirmed: {st}");

    // TikTok: a 2xx without TikTok's code isn't success.
    sqlx::query("DELETE FROM ad_audience_syncs").execute(&h.state.db).await.unwrap();
    connect(&mut h, "tiktok", "tt-code").await;
    let big = bulk_list(&h, "Big", 1001).await;
    m.fail_next("/custom_audience/create/", 200, json!({"data": {"custom_audience_id": "x"}}));
    let tid = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "tiktok", "ad_account_id": "777", "source_type": "list", "source_id": big, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.drain().await;
    let st = sync_status(&mut h, &tid).await;
    assert_eq!((st["status"].as_str(), st["pause_reason"].as_str(), st["audience_id"].as_str()), (Some("paused"), Some("create_unclear"), None), "{st}");
    assert!(st["pause_message"].as_str().unwrap().contains("Big (DumbMail)"));
    assert_eq!(m.calls("/custom_audience/create/").len(), 1, "a create that may have worked is never repeated on its own");
    h.cleanup().await;
}

async fn bulk_list(h: &Harness, name: &str, n: i64) -> String {
    let account: Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let list = Uuid::now_v7();
    sqlx::query("INSERT INTO lists (id, account_id, name) VALUES ($1, $2, $3)").bind(list).bind(account).bind(name).execute(&h.state.db).await.unwrap();
    sqlx::query(
        "WITH p AS (INSERT INTO profiles (id, account_id, email, email_consent) SELECT gen_random_uuid(), $1, $3 || g || '@bulk.test', 'subscribed' FROM generate_series(1, $4::int) g RETURNING id)
         INSERT INTO list_members (list_id, profile_id) SELECT $2, id FROM p",
    )
    .bind(account)
    .bind(list)
    .bind(name.to_lowercase())
    .bind(n as i32)
    .execute(&h.state.db)
    .await
    .unwrap();
    // A real profiles table has statistics; one filled a moment ago in a fresh test database doesn't.
    sqlx::query("ANALYZE profiles, list_members").execute(&h.state.db).await.unwrap();
    list.to_string()
}

#[tokio::test]
async fn audiences_dumbmail_did_not_create_are_never_touched() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "VIPs").await;
    sub(&mut h, "ann@example.com", &list, json!({})).await;
    let id = meta_sync(&mut h, &list).await;
    h.drain().await;
    assert!(m.calls("/meta/5999").is_empty() && m.calls("/meta/act_111/customaudiences").iter().all(|c| c.method == "POST"));

    // Meta says the audience is gone: DumbMail keeps its records and asks before making a new one.
    sub(&mut h, "bea@example.com", &list, json!({})).await;
    m.fail_next("/users", 400, json!({"error": {"message": "Unsupported post request", "code": 100, "error_subcode": 33}}));
    run(&mut h, &id).await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["status"].as_str(), st["pause_reason"].as_str(), st["audience_id"].as_str()), (Some("paused"), Some("audience_missing"), Some("6001")), "{st}");
    assert!(!st.to_string().contains("Unsupported post request"), "the platform's own words are never shown");
    assert_eq!(st["confirmed"], 1, "cleanup records kept");
    assert_eq!(h.req("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"status": "active"}))).await.0, StatusCode::BAD_REQUEST);
    // Delete it (the stand-in confirms deleting 6001) and start again with a new sync.
    h.ok("DELETE", &format!("/api/ad-audiences/syncs/{id}?delete_on_platform=true&confirm=true"), None).await;
    let id = meta_sync(&mut h, &list).await;
    h.drain().await;
    m.fail_next("/users", 400, json!({"error": {"code": 100, "error_subcode": 33}}));
    sub(&mut h, "cy@example.com", &list, json!({})).await;
    run(&mut h, &id).await;
    // This time Meta also says it can't find the audience to delete: the sync and its records stay.
    m.fail_next("/meta/6002", 400, json!({"error": {"code": 100, "error_subcode": 33}}));
    let (s, _) = h.req("DELETE", &format!("/api/ad-audiences/syncs/{id}?delete_on_platform=true&confirm=true"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(sync_status(&mut h, &id).await["audience_id"], "6002", "an ambiguous not-found keeps the record");
    // Making a new audience needs confirming.
    assert_eq!(h.req("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"recreate_audience": true}))).await.0, StatusCode::BAD_REQUEST);
    m.clear();
    h.ok("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"recreate_audience": true, "confirm": true}))).await;
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["audience_id"].as_str(), st["status"].as_str(), st["confirmed"].as_i64()), (Some("6003"), Some("active"), Some(3)), "{st}");
    h.cleanup().await;
}

#[tokio::test]
async fn tiktok_minimum_deletes_the_audience_instead_of_keeping_opt_outs() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "tiktok", "tt-code").await;
    // Under 1,000: TikTok can't hold it, so nothing is uploaded.
    let small = new_list(&mut h, "Small").await;
    sub(&mut h, "ann@shop.com", &small, json!({})).await;
    let sid = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "tiktok", "ad_account_id": "777", "source_type": "list", "source_id": small, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.drain().await;
    assert!(m.calls("/custom_audience/file/upload/").is_empty());
    assert!(sync_status(&mut h, &sid).await["platform_status"].as_str().unwrap().contains("1,000"));

    let list = bulk_list(&h, "Fans", 1003).await;
    let phone_only = profile(&mut h, json!({"phone_number": "+15035550111", "sms_consent": "subscribed", "email_consent": "subscribed", "list_ids": [list]})).await;
    let id = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "tiktok", "ad_account_id": "777", "source_type": "list", "source_id": list, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.drain().await;
    let up = m.calls("/custom_audience/file/upload/");
    assert_eq!(up.len(), 1);
    let body = &up[0].body;
    assert!(body.contains("name=\"calculate_type\"\r\n\r\nEMAIL_SHA256"));
    let start = body.find("Content-Type: text/csv\r\n\r\n").unwrap() + "Content-Type: text/csv\r\n\r\n".len();
    let file = &body[start..body[start..].find("\r\n--").unwrap() + start];
    let lines: Vec<&str> = file.lines().collect();
    assert_eq!(lines[0], "email_SHA256", "a header line");
    assert_eq!(lines.len(), 1004, "one hashed email per line; the phone-only profile isn't sent ({phone_only})");
    assert_eq!(lines[1].len(), 64);
    let sig_start = body.find("name=\"file_signature\"\r\n\r\n").unwrap() + "name=\"file_signature\"\r\n\r\n".len();
    use md5::Digest as _;
    assert_eq!(&body[sig_start..sig_start + 32], hex::encode(md5::Md5::digest(file.as_bytes())));
    let st = sync_status(&mut h, &id).await;
    let first = st["audience_id"].as_str().unwrap().to_string();
    assert_eq!(st["confirmed"], 1003);

    // Four people opt out: 999 would be left, which TikTok refuses. The audience is deleted.
    m.clear();
    for i in 1..=4 {
        set_props(&h, &format!("fans{i}@bulk.test"), json!({"ads_opt_out": true})).await;
    }
    run(&mut h, &id).await;
    assert!(m.calls("/custom_audience/update/").is_empty(), "no removal TikTok would refuse");
    assert_eq!(m.calls("/custom_audience/delete/")[0].json()["custom_audience_ids"], json!([first]));
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["status"].as_str(), st["pause_reason"].as_str(), st["audience_id"].as_str(), st["confirmed"].as_i64()), (Some("paused"), Some("tiktok_minimum"), None, Some(0)), "{st}");
    assert!(st["pause_message"].as_str().unwrap().contains("below TikTok's minimum of 1,000 people"));

    // Enough people again: the next run makes a new audience.
    m.clear();
    for i in 0..3 {
        sub(&mut h, &format!("new{i}@shop.com"), &list, json!({})).await;
    }
    run(&mut h, &id).await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["status"].as_str(), st["confirmed"].as_i64()), (Some("active"), Some(1002)), "{st}");
    assert_ne!(st["audience_id"].as_str().unwrap(), first);

    // TikTok refuses a removal anyway: opt-outs still win, the audience goes.
    m.clear();
    set_props(&h, "new0@shop.com", json!({"gpc": true})).await;
    m.fail_next("/custom_audience/update/", 200, json!({"code": 40002, "message": "audience size too small"}));
    run(&mut h, &id).await;
    assert_eq!(m.calls("/custom_audience/delete/").len(), 1);
    assert_eq!(sync_status(&mut h, &id).await["pause_reason"], "tiktok_minimum");
    h.cleanup().await;
}

#[tokio::test]
async fn google_needs_the_consent_setting_and_matches_on_hashed_email_and_phone() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    let start = h.ok("POST", "/api/ad-audiences/connections/google/start", None).await;
    let url = url::Url::parse(start["url"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!((q["access_type"].as_str(), q["code_challenge_method"].as_str()), ("offline", "S256"));
    assert_eq!(q["scope"], "openid email https://www.googleapis.com/auth/datamanager", "no adwords scope");
    let cookie = h.cookie.clone().unwrap();
    let (_, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/google/callback?code=g-code&state={}", q["state"]), &[("cookie", &cookie)], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?connected=google");
    let verifier = &m.calls("/google/token")[0].form()["code_verifier"];
    assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())), q["code_challenge"]);
    let accounts = h.ok("GET", "/api/ad-audiences/connections/google/accounts", None).await;
    assert_eq!(accounts["manual_entry"], true);

    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "VIPs", "definition": {"conditions": [{"type": "property", "field": "tier", "op": "eq", "value": "vip"}]}}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let vip = |email: &str| json!({"email": email, "email_consent": "subscribed", "properties": {"tier": "vip", "country": "DE", "zip": "10115"}});
    profile(&mut h, vip("Cloudy.SF+shop@gmail.com")).await;
    profile(&mut h, vip("cloudysf@gmail.com")).await;
    let mut lea = vip("lea@example.de");
    lea["phone_number"] = json!("+4915112345678");
    lea["sms_consent"] = json!("subscribed");
    profile(&mut h, lea).await;
    let create = json!({"platform": "google", "ad_account_id": "123-456-7890", "login_account_id": "444-000-0000", "source_type": "segment", "source_id": seg, "accept_terms": true});

    // No consent setting: Google syncs can't be created. An API key can't turn it on; the owner can.
    let (s, _) = h.req("POST", "/api/ad-audiences/syncs", Some(create.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let (s, _, _) = h.raw("PUT", "/api/ad-audiences/settings", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], r#"{"google_consent": true}"#).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let settings = h.ok("PUT", "/api/ad-audiences/settings", Some(json!({"google_consent": true}))).await;
    assert_eq!((settings["google_consent"].as_bool(), settings["google_consent_by"].as_str()), (Some(true), Some("owner@acme.test")));
    // Google matches on email and phone only; an ad account the sign-in can't reach is refused.
    let mut loc = create.clone();
    loc["match_fields"] = json!("contact_and_location");
    assert_eq!(h.req("POST", "/api/ad-audiences/syncs", Some(loc)).await.0, StatusCode::BAD_REQUEST);
    let mut other = create.clone();
    other["ad_account_id"] = json!("999");
    let (s, v) = h.req("POST", "/api/ad-audiences/syncs", Some(other)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("can't manage audiences"), "{v}");
    let created = h.ok("POST", "/api/ad-audiences/syncs", Some(create)).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["ad_account_id"], "1234567890");
    let check = m.calls(&format!("{DM}/userLists"))[0].clone();
    assert_eq!(check.headers["login-account"], "accountTypes/GOOGLE_ADS/accounts/4440000000");
    // The access token is about to expire: refreshed before use.
    sqlx::query("UPDATE ad_connections SET token_expires_at = now() + interval '1 minute'").execute(&h.state.db).await.unwrap();
    h.drain().await;
    let lc = m.calls(&format!("{DM}/userLists")).into_iter().find(|c| c.method == "POST").unwrap();
    assert_eq!(lc.json()["membershipDuration"], "46656000s");
    assert_eq!(lc.headers["login-account"], "accountTypes/GOOGLE_ADS/accounts/4440000000");
    let ingest = m.calls("audienceMembers:ingest");
    assert_eq!(ingest.len(), 1);
    assert_eq!(ingest[0].headers["authorization"], "Bearer g-access-2");
    let b = ingest[0].json();
    assert_eq!(b["consent"], json!({"adUserData": "CONSENT_GRANTED", "adPersonalization": "CONSENT_GRANTED"}));
    assert_eq!(b["termsOfService"]["customerMatchTermsOfServiceStatus"], "ACCEPTED");
    assert_eq!(b["destinations"][0]["loginAccount"], json!({"accountType": "GOOGLE_ADS", "accountId": "4440000000"}));
    let idents: BTreeSet<String> = b["audienceMembers"].as_array().unwrap().iter().map(|mm| mm["userData"]["userIdentifiers"][0].to_string()).collect();
    let want: BTreeSet<String> = [json!({"emailAddress": sha("cloudysf@gmail.com")}), json!({"emailAddress": sha("lea@example.de")}), json!({"phoneNumber": sha("+4915112345678")})].iter().map(|v| v.to_string()).collect();
    assert_eq!(idents, want, "the two Gmail aliases are one identifier; no address or postal code is sent");
    assert!(!m.all_bodies().contains("10115") && !m.all_bodies().contains("\"DE\""), "no plain-text location leaves the server");
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["confirmed"].as_i64(), st["match_rate"].as_f64()), (Some(3), Some(40.0)), "confirmed once Google's request status said SUCCESS: {st}");
    assert_eq!(m.calls("requestStatus:retrieve")[0].query()["requestId"], "req-1");

    // Turning the setting off pauses Google syncs; resuming needs it on again.
    h.ok("PUT", "/api/ad-audiences/settings", Some(json!({"google_consent": false}))).await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["status"].as_str(), st["pause_reason"].as_str()), (Some("paused"), Some("google_consent")));
    assert_eq!(h.req("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"status": "active"}))).await.0, StatusCode::BAD_REQUEST);

    h.ok("DELETE", "/api/ad-audiences/connections/google", None).await;
    assert_eq!(m.calls("/google/revoke")[0].form()["token"], "g-refresh");
    h.cleanup().await;
}

#[tokio::test]
async fn aliases_share_a_key_and_an_opt_out_wins() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "google", "g-code").await;
    h.ok("PUT", "/api/ad-audiences/settings", Some(json!({"google_consent": true}))).await;
    let list = new_list(&mut h, "VIPs").await;
    let a = sub(&mut h, "a.b+x@gmail.com", &list, json!({})).await;
    sub(&mut h, "ab@gmail.com", &list, json!({})).await;
    let id = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "google", "ad_account_id": "1234567890", "source_type": "list", "source_id": list, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.drain().await;
    assert_eq!(m.calls("audienceMembers:ingest")[0].json()["audienceMembers"].as_array().unwrap().len(), 1);
    // One alias leaves the list: the shared hash stays.
    m.clear();
    h.ok("DELETE", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [a]}))).await;
    run(&mut h, &id).await;
    assert!(m.calls("audienceMembers:remove").is_empty(), "still needed by the other alias");
    // The alias that left opts out of ads (outside the list): the hash goes anyway.
    set_props(&h, "a.b+x@gmail.com", json!({"do_not_sell": true})).await;
    run(&mut h, &id).await;
    assert_eq!(m.calls("audienceMembers:remove").len(), 1);
    assert!(m.calls("audienceMembers:ingest").is_empty(), "and is never added back");
    h.cleanup().await;
}

#[tokio::test]
async fn sign_ins_are_bound_to_the_member_who_started_them() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    let owner = h.cookie.clone().unwrap();
    // An API key can't start a sign-in.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let (s, _, _) = h.raw("POST", "/api/ad-audiences/connections/tiktok/start", &[("authorization", &format!("Bearer {key}"))], "").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let start = h.ok("POST", "/api/ad-audiences/connections/tiktok/start", None).await;
    let st: String = url::Url::parse(start["url"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();

    // Another account's session can't use the state, or burn it.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@rival.test", "password": "correct horse", "company": "Rival"}))).await;
    let rival = h.cookie.clone().unwrap();
    let (_, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state={st}"), &[("cookie", &rival)], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?platform=tiktok&error=wrong_account");
    let (_, headers, _) = h.raw("GET", "/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state=nope", &[], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?platform=tiktok&error=signed_out");
    assert!(m.calls("/oauth2/access_token/").is_empty());
    let (_, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state={st}"), &[("cookie", &owner)], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?connected=tiktok", "the owner's sign-in still works");
    let (_, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state={st}"), &[("cookie", &owner)], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?platform=tiktok&error=expired", "single use");

    // A reconnect never inherits the old refresh token.
    h.cookie = Some(owner.clone());
    let has_refresh = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, bool>("SELECT refresh_token_enc IS NOT NULL FROM ad_connections WHERE platform = 'tiktok'").fetch_one(&db).await.unwrap() }
    };
    assert!(has_refresh(&h).await);
    connect(&mut h, "tiktok", "tt-code-2").await;
    assert!(!has_refresh(&h).await);

    // A Google refresh that started before a reconnect can't overwrite it.
    connect(&mut h, "google", "g-code").await;
    sqlx::query("UPDATE ad_connections SET token_expires_at = now() + interval '1 minute' WHERE platform = 'google'").execute(&h.state.db).await.unwrap();
    let account: Uuid = sqlx::query_scalar("SELECT account_id FROM ad_connections WHERE platform = 'google'").fetch_one(&h.state.db).await.unwrap();
    let stale = store::connection(&h.state.db, account, dumbmail::features::audiences::platforms::Platform::Google).await.unwrap().unwrap();
    connect(&mut h, "google", "g-code").await;
    assert!(store::access_token(&h.state, &stale).await.is_err(), "the stale refresh isn't used");
    let enc: String = sqlx::query_scalar("SELECT access_token_enc FROM ad_connections WHERE platform = 'google'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(dumbmail::crypto::decrypt(&h.state.cfg.secret_key, &enc).unwrap(), "g-access", "the reconnect's token stays");

    // A disconnect whose revoke the platform doesn't confirm says so; the tokens are gone anyway.
    connect(&mut h, "meta", "meta-code").await;
    m.fail_next("/me/permissions", 500, json!({"error": {"code": 2}}));
    let d = h.ok("DELETE", "/api/ad-audiences/connections/meta", None).await;
    assert_eq!(d["revoked"], false);
    assert!(d["note"].as_str().unwrap().contains("didn't confirm"));
    let conns = h.ok("GET", "/api/ad-audiences/connections", None).await;
    assert_eq!(conns["data"][0]["last_revoke"], "not_confirmed");
    h.cleanup().await;
}

#[tokio::test]
async fn accounts_are_isolated() {
    let (mut h, _m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = new_list(&mut h, "VIPs").await;
    let id = meta_sync(&mut h, &list).await;
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@rival.test", "password": "correct horse", "company": "Rival"}))).await;
    for (method, path) in [("GET", format!("/api/ad-audiences/syncs/{id}")), ("POST", format!("/api/ad-audiences/syncs/{id}/run")), ("DELETE", format!("/api/ad-audiences/syncs/{id}?confirm=true"))] {
        assert_eq!(h.req(method, &path, None).await.0, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(h.req("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"status": "paused"}))).await.0, StatusCode::NOT_FOUND);
    assert!(h.ok("GET", "/api/ad-audiences/syncs", None).await["data"].as_array().unwrap().is_empty());
    assert_eq!(h.ok("GET", "/api/ad-audiences/connections", None).await["data"][0]["status"], "not_connected");
    let (s, _) = h.req("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn unconfigured_platforms_are_unavailable() {
    let mut h = harness!();
    h.signup().await;
    let conns = h.ok("GET", "/api/ad-audiences/connections", None).await;
    for c in conns["data"].as_array().unwrap() {
        assert_eq!(c["available"], false, "{c}");
    }
    let (s, v) = h.req("POST", "/api/ad-audiences/connections/meta/start", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["message"], "Meta isn't available on this server.");
    h.cleanup().await;
}

async fn mcp(h: &Harness, key: &str, name: &str, args: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["result"]["isError"], false, "{name}: {v}");
    v["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn mcp_tools_dry_run_before_sending_or_deleting() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let list = new_list(&mut h, "VIPs").await;
    sub(&mut h, "ann@example.com", &list, json!({})).await;

    let tools = {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
        let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
        serde_json::from_str::<Value>(&out).unwrap()
    };
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for t in ["list_ad_connections", "list_ad_accounts", "list_audience_syncs", "get_audience_sync", "create_audience_sync", "update_audience_sync", "run_audience_sync", "delete_audience_sync", "disconnect_ad_platform"] {
        assert!(names.contains(&t), "{t}");
    }
    assert!(!tools.to_string().contains("include_unsubscribed"));
    let conns = mcp(&h, &key, "list_ad_connections", json!({})).await;
    assert_eq!(conns["connect_url"], "https://pl.test/settings/ad-audiences");
    assert_eq!(conns["settings"]["google_consent"], false);
    assert_eq!(mcp(&h, &key, "list_ad_accounts", json!({"platform": "meta"})).await["data"][0]["id"], "111");

    let args = json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list});
    let dry = mcp(&h, &key, "create_audience_sync", args.clone()).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["people_now"].as_i64()), (Some(true), Some(1)), "{dry}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_syncs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0, "the dry run creates nothing");
    let mut go = args.clone();
    go["confirm"] = json!(true);
    go["accept_terms"] = json!(true);
    let id = mcp(&h, &key, "create_audience_sync", go).await["id"].as_str().unwrap().to_string();
    h.drain().await;
    assert_eq!(mcp(&h, &key, "get_audience_sync", json!({"id": id})).await["confirmed"], 1);
    assert_eq!(mcp(&h, &key, "update_audience_sync", json!({"id": id, "interval_hours": 12})).await["interval_hours"], 12);
    assert_eq!(mcp(&h, &key, "update_audience_sync", json!({"id": id, "match_fields": "contact_and_location"})).await["dry_run"], true, "sending more needs confirming");
    assert_eq!(mcp(&h, &key, "update_audience_sync", json!({"id": id, "match_fields": "contact_and_location", "confirm": true})).await["match_fields"], "contact_and_location");
    mcp(&h, &key, "run_audience_sync", json!({"id": id})).await;
    assert_eq!(mcp(&h, &key, "list_audience_syncs", json!({"source_type": "list", "source_id": list})).await["data"].as_array().unwrap().len(), 1);
    h.drain().await;

    let dry = mcp(&h, &key, "delete_audience_sync", json!({"id": id, "delete_on_platform": true})).await;
    assert_eq!(dry["dry_run"], true);
    assert!(m.calls("/meta/6001").iter().all(|c| c.method != "DELETE"));
    let dry = mcp(&h, &key, "delete_audience_sync", json!({"id": id})).await;
    assert!(dry["note"].as_str().unwrap().contains("no longer remove people who opt out"));
    mcp(&h, &key, "delete_audience_sync", json!({"id": id, "delete_on_platform": true, "confirm": true})).await;
    assert_eq!(m.calls("/meta/6001").iter().filter(|c| c.method == "DELETE").count(), 1);
    assert_eq!(mcp(&h, &key, "disconnect_ad_platform", json!({"platform": "meta"})).await["dry_run"], true);
    mcp(&h, &key, "disconnect_ad_platform", json!({"platform": "meta", "confirm": true})).await;
    assert_eq!(h.ok("GET", "/api/ad-audiences/connections", None).await["data"][0]["status"], "not_connected");

    let doc = {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/ad-audiences"}}).to_string();
        let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
        serde_json::from_str::<Value>(&out).unwrap()
    };
    assert!(doc["result"]["contents"][0]["text"].as_str().unwrap().contains("Redirect URL"));
    h.cleanup().await;
}

#[tokio::test]
async fn large_audiences_upload_in_batches() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = bulk_list(&h, "Everyone", 23_000).await;
    let started = std::time::Instant::now();
    let id = meta_sync(&mut h, &list).await;
    h.drain().await;
    let first_run = started.elapsed();
    let adds = m.calls("/meta/6001/users");
    assert_eq!(adds.len(), 3, "10,000 per request");
    let sessions: Vec<Value> = adds.iter().map(|c| serde_json::from_str(&c.form()["session"]).unwrap()).collect();
    assert!(sessions.iter().all(|s| s["batch_seq"] == 1 && s["last_batch_flag"] == true), "one batch per session");
    let ids: BTreeSet<i64> = sessions.iter().map(|s| s["session_id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(meta_rows(&adds).len(), 23_000);
    assert_eq!(sync_status(&mut h, &id).await["confirmed"], 23_000);
    // A re-run with nothing changed sends nothing, and half the list leaving is removed exactly.
    m.clear();
    let started = std::time::Instant::now();
    run(&mut h, &id).await;
    let rerun = started.elapsed();
    assert!(m.calls("/users").is_empty());
    sqlx::query("DELETE FROM list_members WHERE profile_id IN (SELECT id FROM profiles WHERE email LIKE 'everyone1%')").execute(&h.state.db).await.unwrap();
    run(&mut h, &id).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM list_members").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sync_status(&mut h, &id).await["confirmed"].as_i64(), Some(left));
    eprintln!("23,000 people: first sync {first_run:?}, unchanged re-run {rerun:?}");
    h.cleanup().await;
}


//! Ad audience sync against local stand-ins for the Meta Marketing API, Google's OAuth, Data
//! Manager and Google Ads APIs, and TikTok's API for Business, following their documented request
//! and response shapes.

#[macro_use]
mod common;
use common::*;

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
    /// Error responses to give, in order, to the next calls whose path ends with the key.
    fail: Vec<(String, u16, Value)>,
    meta_count: i64,
}

type Mock = Arc<Mutex<MockState>>;

impl MockStateExt for Mock {
    fn calls(&self, path_end: &str) -> Vec<Call> {
        self.lock().unwrap().calls.iter().filter(|c| c.path.ends_with(path_end)).cloned().collect()
    }
    fn fail_next(&self, path_end: &str, status: u16, body: Value) {
        self.lock().unwrap().fail.push((path_end.to_string(), status, body));
    }
    fn clear(&self) {
        self.lock().unwrap().calls.clear();
    }
    fn all_bodies(&self) -> String {
        self.lock().unwrap().calls.iter().map(|c| format!("{} {} {}", c.path, c.query, c.body)).collect::<Vec<_>>().join("\n")
    }
}

trait MockStateExt {
    fn calls(&self, path_end: &str) -> Vec<Call>;
    fn fail_next(&self, path_end: &str, status: u16, body: Value);
    fn clear(&self);
    fn all_bodies(&self) -> String;
}

fn fake_id_token(aud: &str, email: &str) -> String {
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({"iss": "https://accounts.google.com", "aud": aud, "sub": "g-123", "email": email, "email_verified": true, "exp": chrono::Utc::now().timestamp() + 3600});
    format!("e30.{}.sig", b.encode(claims.to_string()))
}

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
                st.meta_count += 1;
                ok(json!({"access_token": format!("meta-long-{}", st.meta_count), "token_type": "bearer", "expires_in": 5_184_000}))
            } else {
                assert_eq!(q.get("code").map(String::as_str), Some("meta-code"));
                assert_eq!(q.get("redirect_uri").map(String::as_str), Some("https://pl.test/api/ad-audiences/oauth/meta/callback"));
                ok(json!({"access_token": "meta-short", "token_type": "bearer", "expires_in": 3600}))
            }
        }
        ("GET", "/meta/me") => ok(json!({"id": "1", "name": "Ann Admin"})),
        ("GET", "/meta/me/adaccounts") => ok(json!({"data": [{"id": "act_111", "account_id": "111", "name": "Acme Ads"}], "paging": {"cursors": {"before": "a", "after": "b"}}})),
        ("GET", "/meta/act_111/customaudiences") => ok(json!({"data": []})),
        ("POST", "/meta/act_111/customaudiences") => ok(json!({"id": "6001"})),
        ("POST" | "DELETE", p) if p.starts_with("/meta/") && p.ends_with("/users") => {
            let f = call.form();
            let payload: Value = serde_json::from_str(f.get("payload").map(String::as_str).unwrap_or("{}")).unwrap();
            let session: Value = serde_json::from_str(f.get("session").map(String::as_str).unwrap_or("{}")).unwrap();
            ok(json!({"audience_id": "6001", "session_id": session["session_id"], "num_received": payload["data"].as_array().map(Vec::len), "num_invalid_entries": 0, "invalid_entry_samples": {}}))
        }
        ("GET", "/meta/6001") => ok(json!({"id": "6001", "approximate_count_lower_bound": 1000, "approximate_count_upper_bound": 1200, "delivery_status": {"code": 200, "description": "ready"}, "operation_status": {"code": 200, "description": "normal"}})),
        ("DELETE", "/meta/6001") => ok(json!({"success": true})),
        ("DELETE", "/meta/me/permissions") => ok(json!({"success": true})),
        // ----- Google OAuth, Data Manager API, Google Ads API -----
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
        ("GET", "/googleads/customers:listAccessibleCustomers") => ok(json!({"resourceNames": ["customers/1234567890", "customers/999"]})),
        ("POST", "/googleads/customers/1234567890/googleAds:search") => ok(json!({"results": [{"customer": {"id": "1234567890", "descriptiveName": "Acme Google", "manager": false}}]})),
        ("POST", "/googleads/customers/999/googleAds:search") => ok(json!({"results": [{"customer": {"id": "999", "descriptiveName": "Agency MCC", "manager": true}}]})),
        ("GET", "/datamanager/accountTypes/GOOGLE_ADS/accounts/1234567890/userLists") => ok(json!({"userLists": []})),
        ("POST", "/datamanager/accountTypes/GOOGLE_ADS/accounts/1234567890/userLists") => {
            ok(json!({"name": "accountTypes/GOOGLE_ADS/accounts/1234567890/userLists/555", "id": "555", "displayName": call.json()["displayName"]}))
        }
        ("POST", "/datamanager/audienceMembers:ingest") | ("POST", "/datamanager/audienceMembers:remove") => ok(json!({"requestId": "req-1"})),
        ("GET", "/datamanager/accountTypes/GOOGLE_ADS/accounts/1234567890/userLists/555") => ok(json!({
            "id": "555", "sizeInfo": {"displayNetworkMembersCount": "0", "searchNetworkMembersCount": "0"},
            "ingestedUserListInfo": {"uploadKeyTypes": ["CONTACT_ID"], "contactIdInfo": {"matchRatePercentage": 40}}
        })),
        // ----- TikTok API for Business -----
        ("POST", "/tiktok/oauth2/access_token/") => {
            assert_eq!(call.json()["auth_code"], "tt-code");
            ok(json!({"code": 0, "message": "OK", "request_id": "1", "data": {"access_token": "tt-access", "advertiser_ids": ["777"], "scope": [1, 2]}}))
        }
        ("GET", "/tiktok/oauth2/advertiser/get/") => ok(json!({"code": 0, "message": "OK", "data": {"list": [{"advertiser_id": "777", "advertiser_name": "Acme TT"}]}})),
        ("POST", "/tiktok/oauth2/revoke_token/") => ok(json!({"code": 0, "message": "OK", "data": {}})),
        ("POST", "/tiktok/dmp/custom_audience/file/upload/") => ok(json!({"code": 0, "message": "OK", "data": {"file_path": format!("fp-{}", st.calls.len())}})),
        ("POST", "/tiktok/dmp/custom_audience/create/") => ok(json!({"code": 0, "message": "OK", "data": {"custom_audience_id": "tt-aud-1"}})),
        ("POST", "/tiktok/dmp/custom_audience/update/") => ok(json!({"code": 0, "message": "OK", "data": {"custom_audience_id": "tt-aud-1"}})),
        ("GET", "/tiktok/dmp/custom_audience/list/") => ok(json!({"code": 0, "message": "OK", "data": {"list": [], "page_info": {"page": 1, "page_size": 100, "total_number": 0, "total_page": 1}}})),
        ("GET", "/tiktok/dmp/custom_audience/get/") => ok(json!({"code": 0, "message": "OK", "data": {"list": [{"audience_details": {"audience_id": "tt-aud-1", "cover_num": 1500, "is_valid": true, "is_expired": false}}]}})),
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
        c.google_ads_developer_token = Some("dev-token".into());
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

async fn sync_status(h: &mut Harness, id: &str) -> Value {
    h.ok("GET", &format!("/api/ad-audiences/syncs/{id}"), None).await
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

#[tokio::test]
async fn meta_connect_sync_and_keep_in_step() {
    let (mut h, m) = mock_harness!();
    h.signup().await;

    let conns = h.ok("GET", "/api/ad-audiences/connections", None).await;
    let meta = conns["data"].as_array().unwrap().iter().find(|c| c["platform"] == "meta").unwrap().clone();
    assert_eq!((meta["available"].as_bool(), meta["status"].as_str()), (Some(true), Some("not_connected")));

    // Connect: the sign-in URL carries our state; the callback swaps the code for a long-lived token.
    let start = h.ok("POST", "/api/ad-audiences/connections/meta/start", None).await;
    let url = start["url"].as_str().unwrap();
    assert!(url.starts_with("https://www.facebook.com/v27.0/dialog/oauth?"), "{url}");
    assert!(url.contains("scope=ads_management") && url.contains("client_id=meta-app"), "{url}");
    let loc = connect(&mut h, "meta", "meta-code").await;
    assert_eq!(loc, "/settings/ad-audiences?connected=meta");
    let conns = h.ok("GET", "/api/ad-audiences/connections", None).await;
    let meta = conns["data"].as_array().unwrap().iter().find(|c| c["platform"] == "meta").unwrap().clone();
    assert_eq!(meta["status"], "connected");
    assert_eq!(meta["connected_as"], "Ann Admin");
    assert!(!conns.to_string().contains("meta-long-1"), "the token never comes back: {conns}");
    let stored: String = sqlx::query_scalar("SELECT access_token_enc FROM ad_connections").fetch_one(&h.state.db).await.unwrap();
    assert!(!stored.contains("meta-long"), "tokens are stored encrypted");
    // Every Graph call proves it came from the app's server.
    let me = &m.calls("/meta/me")[0];
    assert_eq!(me.query()["appsecret_proof"].len(), 64);

    let accounts = h.ok("GET", "/api/ad-audiences/connections/meta/accounts", None).await;
    assert_eq!(accounts["data"][0], json!({"id": "111", "name": "Acme Ads", "manager": false}));

    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIPs"}))).await["id"].as_str().unwrap().to_string();
    let ann = profile(&mut h, json!({"email": " Ann@Example.com", "phone_number": "+15035550100", "email_consent": "subscribed", "list_ids": [list]})).await;
    let _unsub = profile(&mut h, json!({"email": "bob@example.com", "email_consent": "unsubscribed", "list_ids": [list]})).await;
    let sup = profile(&mut h, json!({"email": "cat@example.com", "email_consent": "subscribed", "list_ids": [list]})).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard bounce' WHERE id = $1::uuid").bind(&sup).execute(&h.state.db).await.unwrap();
    let _opted = profile(&mut h, json!({"email": "dan@example.com", "properties": {"ads_opt_out": true}, "list_ids": [list]})).await;
    let _dns = profile(&mut h, json!({"email": "eve@example.com", "properties": {"do_not_sell": "true"}, "list_ids": [list]})).await;
    let fay = profile(&mut h, json!({"phone_number": "+447700900123", "list_ids": [list]})).await;

    // Terms must be accepted; a list from another account or an ad account the sign-in can't see is refused.
    let body = json!({"platform": "meta", "ad_account_id": "act_111", "source_type": "list", "source_id": list});
    let (s, v) = h.req("POST", "/api/ad-audiences/syncs", Some(body.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("terms"), "{v}");
    let mut bad = body.clone();
    bad["ad_account_id"] = json!("222");
    bad["accept_terms"] = json!(true);
    let (s, _) = h.req("POST", "/api/ad-audiences/syncs", Some(bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let mut good = body.clone();
    good["accept_terms"] = json!(true);
    let created = h.ok("POST", "/api/ad-audiences/syncs", Some(good.clone())).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], "VIPs (DumbMail)");
    assert_eq!(created["ad_account_name"], "Acme Ads");
    assert_eq!(created["interval_hours"], 6);
    let (s, _) = h.req("POST", "/api/ad-audiences/syncs", Some(good)).await;
    assert_eq!(s, StatusCode::CONFLICT, "one sync per list and ad account");

    h.drain().await;
    let create = &m.calls("/meta/act_111/customaudiences").into_iter().filter(|c| c.method == "POST").collect::<Vec<_>>()[0];
    let f = create.form();
    assert_eq!((f["subtype"].as_str(), f["customer_file_source"].as_str(), f["name"].as_str()), ("CUSTOM", "USER_PROVIDED_ONLY", "VIPs (DumbMail)"));
    assert!(f["description"].contains("Synced by DumbMail"));
    let adds = m.calls("/meta/6001/users");
    assert_eq!(adds.len(), 1);
    assert_eq!(adds[0].method, "POST");
    let payload: Value = serde_json::from_str(&adds[0].form()["payload"]).unwrap();
    assert_eq!(payload["schema"], json!(["EMAIL", "PHONE"]));
    let session: Value = serde_json::from_str(&adds[0].form()["session"]).unwrap();
    assert_eq!((session["batch_seq"].as_i64(), session["last_batch_flag"].as_bool()), (Some(1), Some(true)));
    assert!(session["session_id"].as_i64().unwrap() > 0);
    let mut rows = meta_rows(&adds);
    rows.sort();
    let mut want = vec![vec![sha("ann@example.com"), sha("15035550100")], vec![String::new(), sha("447700900123")]];
    want.sort();
    assert_eq!(rows, want, "only Ann and Fay: unsubscribed, suppressed and opted-out people are never sent");
    let everything = m.all_bodies();
    for raw in ["ann@example.com", "bob@example.com", "15035550100", "Ann@Example"] {
        assert!(!everything.contains(raw), "raw identifier {raw} left the server");
    }
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["members_sent"].as_i64(), st["last_added"].as_i64(), st["last_removed"].as_i64()), (Some(2), Some(2), Some(0)), "{st}");
    assert_eq!((st["platform_size"].as_i64(), st["platform_size_upper"].as_i64()), (Some(1000), Some(1200)));
    assert_eq!(st["platform_status"], "Ready");
    assert_eq!(st["audience_id"], "6001");
    assert_eq!(st["running"], false);
    assert!(st["error"].is_null());
    assert_eq!(st["runs"][0]["status"], "done");

    // Nothing changed: a re-run sends nothing.
    m.clear();
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    assert!(m.calls("/users").is_empty(), "idempotent re-run");

    // Ann leaves the list, Fay is deleted, Gus joins, Hal joins but opts out of ads: exact removals.
    m.clear();
    h.ok("DELETE", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [ann]}))).await;
    h.ok("DELETE", &format!("/api/profiles/{fay}"), None).await;
    profile(&mut h, json!({"email": "gus@example.com", "list_ids": [list]})).await;
    profile(&mut h, json!({"email": "hal@example.com", "list_ids": [list], "properties": {"gpc": true}})).await;
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    let calls = m.calls("/meta/6001/users");
    let removes: Vec<Call> = calls.iter().filter(|c| c.method == "DELETE").cloned().collect();
    let adds: Vec<Call> = calls.iter().filter(|c| c.method == "POST").cloned().collect();
    let mut removed = meta_rows(&removes);
    removed.sort();
    assert_eq!(removed, want, "the same hashed rows that were added are removed");
    assert_eq!(meta_rows(&adds), vec![vec![sha("gus@example.com"), String::new()]]);
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["members_sent"].as_i64(), st["last_added"].as_i64(), st["last_removed"].as_i64()), (Some(1), Some(1), Some(2)), "{st}");

    // Marking someone ads_opt_out later removes them on the next sync.
    m.clear();
    sqlx::query("UPDATE profiles SET properties = properties || '{\"ads_opt_out\": true}' WHERE email = 'gus@example.com'").execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    assert_eq!(meta_rows(&m.calls("/meta/6001/users")), vec![vec![sha("gus@example.com"), String::new()]]);
    assert_eq!(m.calls("/meta/6001/users")[0].method, "DELETE");

    // Pause, resume; delete keeps the audience unless asked.
    let paused = h.ok("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"status": "paused"}))).await;
    assert_eq!(paused["status"], "paused");
    let (s, _) = h.req("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.req("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"interval_hours": 0}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "at most hourly");
    h.ok("PATCH", &format!("/api/ad-audiences/syncs/{id}"), Some(json!({"status": "active", "interval_hours": 1}))).await;
    m.clear();
    let del = h.ok("DELETE", &format!("/api/ad-audiences/syncs/{id}?delete_on_platform=true"), None).await;
    assert_eq!(del["deleted_on_platform"], true);
    assert_eq!(m.calls("/meta/6001").iter().filter(|c| c.method == "DELETE").count(), 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_members").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);

    // Disconnect revokes and deletes the tokens.
    h.ok("DELETE", "/api/ad-audiences/connections/meta", None).await;
    assert_eq!(m.calls("/meta/me/permissions").len(), 1);
    let (status, tok): (String, Option<String>) = sqlx::query_as("SELECT status, access_token_enc FROM ad_connections").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), tok), ("disconnected", None));
    h.cleanup().await;
}

#[tokio::test]
async fn rate_limits_back_off_and_expired_sign_ins_ask_to_reconnect() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Buyers"}))).await["id"].as_str().unwrap().to_string();
    profile(&mut h, json!({"email": "ann@example.com", "list_ids": [list]})).await;

    // Meta's custom audience rate limit (80003) on the first upload: the run waits and retries.
    m.fail_next("/users", 400, json!({"error": {"message": "There have been too many calls", "type": "OAuthException", "code": 80003, "fbtrace_id": "x"}}));
    let created = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await;
    let id = created["id"].as_str().unwrap().to_string();
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!(st["running"], true, "still running, waiting to retry: {st}");
    assert!(st["runs"][0]["error"].as_str().unwrap().contains("slow down"));
    let wait: f64 = sqlx::query_scalar("SELECT extract(epoch FROM run_at - now())::float8 FROM jobs WHERE kind = 'audiences_run'").fetch_one(&h.state.db).await.unwrap();
    assert!(wait > 30.0, "backs off before retrying ({wait}s)");
    h.fast_forward().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["running"].as_bool(), st["members_sent"].as_i64()), (Some(false), Some(1)), "{st}");
    assert!(st["error"].is_null());
    assert_eq!(m.calls("/meta/6001/users").len(), 2, "the batch was sent again after the wait");

    // The sign-in was revoked on Meta (190): plain words, and the connection asks to reconnect.
    profile(&mut h, json!({"email": "bea@example.com", "list_ids": [list]})).await;
    m.fail_next("/users", 400, json!({"error": {"message": "Error validating access token", "type": "OAuthException", "code": 190}}));
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!(st["error"], "Meta sign-in expired or was revoked. Reconnect Meta.");
    assert_eq!(st["connection_status"], "needs_reconnect");
    assert_eq!(st["runs"][0]["status"], "failed");
    assert_eq!(st["members_sent"], 1, "the snapshot only holds what Meta accepted");
    // Scheduled runs wait for a reconnect.
    sqlx::query("UPDATE ad_audience_syncs SET next_run_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    dumbmail::features::tick(&h.state, 0).await;
    let running: Option<uuid::Uuid> = sqlx::query_scalar("SELECT current_run_id FROM ad_audience_syncs").fetch_one(&h.state.db).await.unwrap();
    assert!(running.is_none());
    // Reconnecting keeps the sync, and it carries on.
    connect(&mut h, "meta", "meta-code").await;
    dumbmail::features::tick(&h.state, 0).await;
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["members_sent"].as_i64(), st["connection_status"].as_str()), (Some(2), Some("connected")), "{st}");

    // Missing permission on the ad account (Custom Audience terms not accepted) is explained.
    profile(&mut h, json!({"email": "cy@example.com", "list_ids": [list]})).await;
    m.fail_next("/users", 400, json!({"error": {"message": "Permissions error", "code": 200, "error_subcode": 1870090}}));
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert!(st["error"].as_str().unwrap().contains("Custom Audience terms"), "{st}");
    assert_eq!(st["connection_status"], "connected");
    h.cleanup().await;
}

#[tokio::test]
async fn google_customer_match_with_consent_and_token_refresh() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    let start = h.ok("POST", "/api/ad-audiences/connections/google/start", None).await;
    let url = url::Url::parse(start["url"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!((q["access_type"].as_str(), q["code_challenge_method"].as_str()), ("offline", "S256"));
    assert!(q["scope"].contains("https://www.googleapis.com/auth/datamanager"));
    let cookie = h.cookie.clone().unwrap();
    let (s, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/google/callback?code=g-code&state={}", q["state"]), &[("cookie", &cookie)], "").await;
    assert!(s.is_redirection());
    assert_eq!(headers["location"], "/settings/ad-audiences?connected=google");
    // PKCE: the verifier sent with the code matches the challenge in the sign-in URL.
    let token_call = m.calls("/google/token")[0].form();
    let verifier = &token_call["code_verifier"];
    assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())), q["code_challenge"]);
    let conns = h.ok("GET", "/api/ad-audiences/connections", None).await;
    assert_eq!(conns["data"][1]["connected_as"], "ads@acme.test");

    let accounts = h.ok("GET", "/api/ad-audiences/connections/google/accounts", None).await;
    assert_eq!(accounts["data"][0]["name"], "Acme Google");
    assert_eq!(m.calls("customers:listAccessibleCustomers")[0].headers["developer-token"], "dev-token");

    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Everyone", "definition": {"conditions": [{"type": "property", "field": "tier", "op": "eq", "value": "vip"}]}}))).await["id"].as_str().unwrap().to_string();
    profile(&mut h, json!({"email": "Cloudy.SF+shop@gmail.com", "first_name": "Mrs. Ann", "last_name": "Lee Jr.", "properties": {"tier": "vip", "country": "US", "zip": "94043-1351"}})).await;
    profile(&mut h, json!({"email": "max@example.de", "properties": {"tier": "vip", "country": "DE"}})).await;
    profile(&mut h, json!({"email": "lea@example.de", "properties": {"tier": "vip", "country": "DE", "ads_consent": true}})).await;
    profile(&mut h, json!({"email": "zed@example.com", "properties": {"tier": "regular"}})).await;

    // A manager account can't hold the list.
    let (s, v) = h.req("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "google", "ad_account_id": "999", "source_type": "segment", "source_id": seg, "accept_terms": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("manager"));
    let created = h
        .ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "google", "ad_account_id": "123-456-7890", "source_type": "segment", "source_id": seg, "accept_terms": true, "match_fields": "contact_and_location"})))
        .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["ad_account_id"], "1234567890");
    // The access token is about to expire: it's refreshed before use.
    sqlx::query("UPDATE ad_connections SET token_expires_at = now() + interval '1 minute'").execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(m.calls("/google/token").len(), 2, "refreshed once");
    let list_create = &m.calls("/accounts/1234567890/userLists").into_iter().filter(|c| c.method == "POST").collect::<Vec<_>>()[0];
    let lc = list_create.json();
    assert_eq!(lc["ingestedUserListInfo"]["uploadKeyTypes"], json!(["CONTACT_ID"]));
    assert_eq!(lc["membershipStatus"], "OPEN");
    let ingest = m.calls("audienceMembers:ingest");
    assert_eq!(ingest.len(), 1);
    assert_eq!(ingest[0].headers["authorization"], "Bearer g-access-2");
    let b = ingest[0].json();
    assert_eq!(b["encoding"], "HEX");
    assert_eq!(b["termsOfService"]["customerMatchTermsOfServiceStatus"], "ACCEPTED");
    assert_eq!(b["destinations"][0], json!({"operatingAccount": {"accountType": "GOOGLE_ADS", "accountId": "1234567890"}, "productDestinationId": "555"}));
    let members = b["audienceMembers"].as_array().unwrap();
    assert_eq!(members.len(), 2, "the German profile without ad consent is left out: {b}");
    let us = members.iter().find(|m| m["userData"]["userIdentifiers"][0]["emailAddress"] == json!(sha("cloudysf@gmail.com"))).expect("gmail normalized per Google's rules");
    assert!(us.get("consent").is_none());
    assert_eq!(us["userData"]["userIdentifiers"][1]["address"], json!({"givenName": sha("ann"), "familyName": sha("lee"), "regionCode": "US", "postalCode": "94043"}));
    let de = members.iter().find(|m| m["userData"]["userIdentifiers"][0]["emailAddress"] == json!(sha("lea@example.de"))).unwrap();
    assert_eq!(de["consent"], json!({"adUserData": "CONSENT_GRANTED", "adPersonalization": "CONSENT_GRANTED"}));
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["members_sent"].as_i64(), st["runs"][0]["skipped"].as_i64()), (Some(2), Some(1)), "{st}");
    assert_eq!(st["match_rate"].as_f64(), Some(40.0));
    assert!(st["platform_status"].as_str().unwrap().contains("Too small"));

    // Lea withdraws consent: removed with the same identifiers.
    m.clear();
    sqlx::query("UPDATE profiles SET properties = properties || '{\"ads_consent\": false}' WHERE email = 'lea@example.de'").execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    let rm = m.calls("audienceMembers:remove");
    assert_eq!(rm.len(), 1);
    assert_eq!(rm[0].json()["audienceMembers"][0]["userData"]["userIdentifiers"][0]["emailAddress"], json!(sha("lea@example.de")));
    assert!(m.calls("audienceMembers:ingest").is_empty());

    // Google's quota error backs off; a permission error is shown in plain words.
    m.fail_next("audienceMembers:ingest", 403, json!({"error": {"code": 403, "message": "The caller does not have permission", "status": "PERMISSION_DENIED"}}));
    profile(&mut h, json!({"email": "new@example.com", "properties": {"tier": "vip"}})).await;
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    let st = sync_status(&mut h, &id).await;
    assert!(st["error"].as_str().unwrap().starts_with("This Google account can't manage audiences"), "{st}");

    h.ok("DELETE", "/api/ad-audiences/connections/google", None).await;
    assert_eq!(m.calls("/google/revoke")[0].form()["token"], "g-refresh");
    h.cleanup().await;
}

#[tokio::test]
async fn tiktok_customer_files() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    let loc = connect(&mut h, "tiktok", "tt-code").await;
    assert_eq!(loc, "/settings/ad-audiences?connected=tiktok");
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Fans"}))).await["id"].as_str().unwrap().to_string();
    let ann = profile(&mut h, json!({"email": "Ann@Shop.com", "list_ids": [list]})).await;
    profile(&mut h, json!({"email": "bo@shop.com", "list_ids": [list]})).await;
    profile(&mut h, json!({"phone_number": "+15035550111", "list_ids": [list]})).await;
    let created = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "tiktok", "ad_account_id": "777", "source_type": "list", "source_id": list, "accept_terms": true}))).await;
    let id = created["id"].as_str().unwrap().to_string();
    h.drain().await;

    let up = m.calls("/custom_audience/file/upload/");
    assert_eq!(up.len(), 1);
    assert_eq!(up[0].headers["access-token"], "tt-access");
    let body = &up[0].body;
    assert!(body.contains("name=\"calculate_type\"\r\n\r\nEMAIL_SHA256"), "{body}");
    let file_start = body.find("Content-Type: text/csv\r\n\r\n").unwrap() + "Content-Type: text/csv\r\n\r\n".len();
    let file = &body[file_start..body[file_start..].find("\r\n--").unwrap() + file_start];
    let mut lines: Vec<&str> = file.lines().collect();
    lines.sort();
    let mut want = vec![sha("ann@shop.com"), sha("bo@shop.com")];
    want.sort();
    assert_eq!(lines, want, "email hashes only; the phone-only profile can't match on TikTok");
    let sig_start = body.find("name=\"file_signature\"\r\n\r\n").unwrap() + "name=\"file_signature\"\r\n\r\n".len();
    let sig = &body[sig_start..sig_start + 32];
    use md5::Digest as _;
    assert_eq!(sig, hex::encode(md5::Md5::digest(file.as_bytes())), "file_signature is the file's MD5");
    assert!(m.calls("/custom_audience/list/").is_empty(), "never matched by name to an audience the customer made");
    let create = m.calls("/custom_audience/create/")[0].json();
    assert_eq!(create["calculate_type"], "EMAIL_SHA256");
    assert_eq!(create["advertiser_id"], "777");
    assert_eq!(create["custom_audience_name"], "Fans (DumbMail)");
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["audience_id"].as_str(), st["members_sent"].as_i64(), st["runs"][0]["skipped"].as_i64()), (Some("tt-aud-1"), Some(2), Some(1)), "{st}");
    assert_eq!(st["platform_size"], 1500);

    m.clear();
    h.ok("DELETE", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [ann]}))).await;
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    let upd = m.calls("/custom_audience/update/");
    assert_eq!(upd.len(), 1);
    assert_eq!(upd[0].json()["action"], "REMOVE");
    assert!(m.calls("/custom_audience/file/upload/")[0].body.contains(&sha("ann@shop.com")));

    // TikTok's rate limit code (40100) backs off instead of failing.
    profile(&mut h, json!({"email": "cy@shop.com", "list_ids": [list]})).await;
    m.fail_next("/custom_audience/file/upload/", 200, json!({"code": 40100, "message": "Too many requests"}));
    h.ok("POST", &format!("/api/ad-audiences/syncs/{id}/run"), None).await;
    h.drain().await;
    assert_eq!(sync_status(&mut h, &id).await["running"], true);
    h.fast_forward().await;
    let st = sync_status(&mut h, &id).await;
    assert_eq!((st["running"].as_bool(), st["members_sent"].as_i64()), (Some(false), Some(2)), "{st}");
    assert_eq!(m.calls("/custom_audience/update/").last().unwrap().json()["action"], "APPEND");
    h.cleanup().await;
}

#[tokio::test]
async fn exclusion_audiences_can_include_unsubscribed_but_never_opt_outs() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Past buyers"}))).await["id"].as_str().unwrap().to_string();
    profile(&mut h, json!({"email": "sub@example.com", "email_consent": "subscribed", "list_ids": [list]})).await;
    profile(&mut h, json!({"email": "unsub@example.com", "email_consent": "unsubscribed", "list_ids": [list]})).await;
    profile(&mut h, json!({"email": "out@example.com", "email_consent": "unsubscribed", "properties": {"ads_opt_out": "yes"}, "list_ids": [list]})).await;
    h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true, "include_unsubscribed": true}))).await;
    h.drain().await;
    let mut rows: Vec<String> = meta_rows(&m.calls("/meta/6001/users")).into_iter().map(|r| r[0].clone()).collect();
    rows.sort();
    let mut want = vec![sha("sub@example.com"), sha("unsub@example.com")];
    want.sort();
    assert_eq!(rows, want);
    h.cleanup().await;
}

#[tokio::test]
async fn accounts_are_isolated_and_sign_ins_are_bound_to_who_started_them() {
    let (mut h, _m) = mock_harness!();
    h.signup().await;
    let owner = h.cookie.clone().unwrap();
    connect(&mut h, "meta", "meta-code").await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIPs"}))).await["id"].as_str().unwrap().to_string();
    let id = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    // The owner starts a sign-in; an attacker's session finishing it gets nothing.
    let start = h.ok("POST", "/api/ad-audiences/connections/tiktok/start", None).await;
    let url = url::Url::parse(start["url"].as_str().unwrap()).unwrap();
    let st: String = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();

    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@rival.test", "password": "correct horse", "company": "Rival"}))).await;
    let rival = h.cookie.clone().unwrap();
    for path in [format!("/api/ad-audiences/syncs/{id}"), format!("/api/ad-audiences/syncs/{id}/run")] {
        let method = if path.ends_with("/run") { "POST" } else { "GET" };
        let (s, _) = h.req(method, &path, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
    }
    let (s, _) = h.req("DELETE", &format!("/api/ad-audiences/syncs/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(h.ok("GET", "/api/ad-audiences/syncs", None).await["data"].as_array().unwrap().is_empty());
    assert_eq!(h.ok("GET", "/api/ad-audiences/connections", None).await["data"][0]["status"], "not_connected");
    // The rival can't sync the owner's list either.
    let (s, _) = h.req("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state={st}"), &[("cookie", &rival)], "").await;
    assert!(s.is_redirection());
    assert_eq!(headers["location"], "/settings/ad-audiences?platform=tiktok&error=wrong_account");
    // A used or unknown state doesn't work again, and signed-out callbacks are refused.
    let (_, headers, _) = h.raw("GET", &format!("/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state={st}"), &[("cookie", &owner)], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?platform=tiktok&error=expired");
    let (_, headers, _) = h.raw("GET", "/api/ad-audiences/oauth/tiktok/callback?auth_code=tt-code&state=nope", &[], "").await;
    assert_eq!(headers["location"], "/settings/ad-audiences?platform=tiktok&error=signed_out");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_connections WHERE platform = 'tiktok'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);
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
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "VIPs"}))).await["id"].as_str().unwrap().to_string();
    profile(&mut h, json!({"email": "ann@example.com", "list_ids": [list]})).await;

    let tools = {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
        let (_, _, out) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
        serde_json::from_str::<Value>(&out).unwrap()
    };
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for t in ["list_ad_connections", "list_ad_accounts", "list_audience_syncs", "create_audience_sync", "update_audience_sync", "run_audience_sync", "delete_audience_sync"] {
        assert!(names.contains(&t), "{t}");
    }
    let conns = mcp(&h, &key, "list_ad_connections", json!({})).await;
    assert_eq!(conns["connect_url"], "https://pl.test/settings/ad-audiences");
    let accts = mcp(&h, &key, "list_ad_accounts", json!({"platform": "meta"})).await;
    assert_eq!(accts["data"][0]["id"], "111");

    let args = json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list});
    let dry = mcp(&h, &key, "create_audience_sync", args.clone()).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["people_now"].as_i64()), (Some(true), Some(1)), "{dry}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_syncs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0, "the dry run creates nothing");
    let mut go = args.clone();
    go["confirm"] = json!(true);
    go["accept_terms"] = json!(true);
    let created = mcp(&h, &key, "create_audience_sync", go).await;
    let id = created["id"].as_str().unwrap().to_string();
    h.drain().await;
    assert_eq!(mcp(&h, &key, "get_audience_sync", json!({"id": id})).await["members_sent"], 1);
    let upd = mcp(&h, &key, "update_audience_sync", json!({"id": id, "interval_hours": 12})).await;
    assert_eq!(upd["interval_hours"], 12);
    let dry = mcp(&h, &key, "update_audience_sync", json!({"id": id, "include_unsubscribed": true})).await;
    assert_eq!(dry["dry_run"], true, "sending more needs confirming");
    assert_eq!(mcp(&h, &key, "get_audience_sync", json!({"id": id})).await["include_unsubscribed"], false);
    let upd = mcp(&h, &key, "update_audience_sync", json!({"id": id, "include_unsubscribed": true, "confirm": true})).await;
    assert_eq!(upd["include_unsubscribed"], true);
    mcp(&h, &key, "run_audience_sync", json!({"id": id})).await;
    assert_eq!(mcp(&h, &key, "list_audience_syncs", json!({"source_type": "list", "source_id": list})).await["data"].as_array().unwrap().len(), 1);

    let dry = mcp(&h, &key, "delete_audience_sync", json!({"id": id, "delete_on_platform": true})).await;
    assert_eq!(dry["dry_run"], true);
    assert!(m.calls("/meta/6001").iter().all(|c| c.method != "DELETE"));
    mcp(&h, &key, "delete_audience_sync", json!({"id": id, "delete_on_platform": true, "confirm": true})).await;
    assert_eq!(m.calls("/meta/6001").iter().filter(|c| c.method == "DELETE").count(), 1);
    let dry = mcp(&h, &key, "disconnect_ad_platform", json!({"platform": "meta"})).await;
    assert_eq!(dry["dry_run"], true);
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
async fn large_audiences_upload_in_batches_with_one_meta_session() {
    let (mut h, m) = mock_harness!();
    h.signup().await;
    connect(&mut h, "meta", "meta-code").await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Everyone"}))).await["id"].as_str().unwrap().to_string();
    let account: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let list_id: uuid::Uuid = list.parse().unwrap();
    sqlx::query(
        "WITH p AS (INSERT INTO profiles (id, account_id, email) SELECT gen_random_uuid(), $1, 'p' || g || '@bulk.test' FROM generate_series(1, 23000) g RETURNING id)
         INSERT INTO list_members (list_id, profile_id) SELECT $2, id FROM p",
    )
    .bind(account)
    .bind(list_id)
    .execute(&h.state.db)
    .await
    .unwrap();
    let id = h.ok("POST", "/api/ad-audiences/syncs", Some(json!({"platform": "meta", "ad_account_id": "111", "source_type": "list", "source_id": list, "accept_terms": true}))).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.drain().await;
    let adds = m.calls("/meta/6001/users");
    assert_eq!(adds.len(), 3, "10,000 per request");
    let sessions: Vec<Value> = adds.iter().map(|c| serde_json::from_str(&c.form()["session"]).unwrap()).collect();
    assert!(sessions.iter().all(|s| s["session_id"] == sessions[0]["session_id"]));
    assert_eq!(sessions.iter().map(|s| s["batch_seq"].as_i64().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(sessions.iter().map(|s| s["last_batch_flag"].as_bool().unwrap()).collect::<Vec<_>>(), vec![false, false, true]);
    assert_eq!(sessions[0]["estimated_num_total"], 23000);
    assert_eq!(meta_rows(&adds).len(), 23000);
    let st = sync_status(&mut h, &id).await;
    assert_eq!(st["members_sent"], 23000);
    h.cleanup().await;
}

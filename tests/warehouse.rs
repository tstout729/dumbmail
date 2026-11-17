//! Scheduled exports: datasets, incremental windows, manifests, multipart uploads, resuming after a
//! failure, account isolation and the MCP tools, against a local S3 stand-in.

#[macro_use]
mod common;
use common::*;

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{json, Value};

#[derive(Default)]
struct S3State {
    objects: HashMap<String, Vec<u8>>,
    uploads: HashMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
    /// (method, key, query) of every request.
    log: Vec<(String, String, String)>,
    /// Answer 500 to uploads whose key contains this.
    fail_keys: Option<String>,
    deny: bool,
    bad_auth: usize,
    /// Answer CompleteMultipartUpload with 200 and then an error document, as S3 can.
    complete_error: bool,
}

#[derive(Clone, Default)]
struct S3(Arc<Mutex<S3State>>);

impl S3 {
    fn objects(&self) -> HashMap<String, Vec<u8>> {
        self.0.lock().unwrap().objects.clone()
    }
    fn keys(&self, part: &str) -> Vec<String> {
        let mut k: Vec<String> = self.objects().into_keys().filter(|k| k.contains(part)).collect();
        k.sort();
        k
    }
    fn lines(&self, key: &str) -> Vec<String> {
        let gz = self.objects().remove(key).unwrap_or_else(|| panic!("no object {key}"));
        let mut s = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut s).unwrap();
        s.lines().map(String::from).collect()
    }
    fn json_rows(&self, dataset: &str) -> Vec<Value> {
        self.keys(&format!("/{dataset}/")).iter().flat_map(|k| self.lines(k)).map(|l| serde_json::from_str(&l).unwrap()).collect()
    }
    fn count(&self, method: &str, key_part: &str, query_part: &str) -> usize {
        self.0.lock().unwrap().log.iter().filter(|(m, k, q)| m == method && k.contains(key_part) && q.contains(query_part)).count()
    }
}

async fn s3_handler(State(s): State<S3>, method: Method, Path((bucket, key)): Path<(String, String)>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap, body: axum::body::Bytes) -> axum::response::Response {
    let mut st = s.0.lock().unwrap();
    let qs = {
        let mut k: Vec<String> = q.iter().map(|(a, b)| format!("{a}={b}")).collect();
        k.sort();
        k.join("&")
    };
    st.log.push((method.to_string(), key.clone(), qs));
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    if bucket != "exports" || !auth.starts_with("AWS4-HMAC-SHA256 Credential=AKTEST123456/") || !["host", "x-amz-content-sha256", "x-amz-date"].iter().all(|h| auth.split("SignedHeaders=").nth(1).is_some_and(|s| s.split(',').next().unwrap_or("").split(';').any(|x| x == *h))) || headers.get("x-amz-content-sha256").is_none() {
        st.bad_auth += 1;
        return (StatusCode::FORBIDDEN, "<Error><Code>SignatureDoesNotMatch</Code></Error>").into_response();
    }
    if st.deny {
        return (StatusCode::FORBIDDEN, "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>").into_response();
    }
    if st.fail_keys.as_deref().is_some_and(|f| key.contains(f)) && method != Method::DELETE {
        return (StatusCode::INTERNAL_SERVER_ERROR, "<Error><Code>InternalError</Code></Error>").into_response();
    }
    match (method.as_str(), q.get("uploadId"), q.get("partNumber"), q.contains_key("uploads")) {
        ("POST", None, None, true) => {
            let id = format!("up{}", st.uploads.len() + 1);
            st.uploads.insert(id.clone(), (key, BTreeMap::new()));
            format!("<InitiateMultipartUploadResult><Bucket>exports</Bucket><UploadId>{id}</UploadId></InitiateMultipartUploadResult>").into_response()
        }
        ("PUT", Some(id), Some(n), _) => {
            let n: u32 = n.parse().unwrap();
            st.uploads.get_mut(id).unwrap().1.insert(n, body.to_vec());
            ([("etag", format!("\"etag-{n}\""))], "").into_response()
        }
        ("POST", Some(_), None, _) if st.complete_error => {
            (StatusCode::OK, "  \n  \n<Error><Code>InternalError</Code><Message>We encountered an internal error. Please try again.</Message></Error>").into_response()
        }
        ("POST", Some(id), None, _) => {
            let (k, parts) = st.uploads.remove(id).unwrap();
            let xml = String::from_utf8_lossy(&body).to_string();
            assert_eq!(xml.matches("<Part>").count(), parts.len(), "{xml}");
            assert!(parts.iter().rev().skip(1).all(|(_, b)| b.len() >= 5 * 1024 * 1024), "parts but the last must be at least 5 MB");
            st.objects.insert(k, parts.into_values().flatten().collect());
            "<CompleteMultipartUploadResult><ETag>\"x\"</ETag></CompleteMultipartUploadResult>".into_response()
        }
        ("DELETE", Some(id), _, _) => {
            st.uploads.remove(id);
            StatusCode::NO_CONTENT.into_response()
        }
        ("PUT", None, None, _) => {
            st.objects.insert(key, body.to_vec());
            ([("etag", "\"e\"")], "").into_response()
        }
        ("DELETE", None, None, _) => {
            st.objects.remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "<Error><Code>NotImplemented</Code></Error>").into_response(),
    }
}

async fn mock_s3() -> (S3, String) {
    dumbmail::features::webhooks::net::allow_localhost_for_tests();
    let s = S3::default();
    let app = axum::Router::new().route("/{bucket}/{*key}", axum::routing::any(s3_handler)).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)).with_state(s.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (s, format!("http://{addr}"))
}

fn destination(endpoint: &str, extra: Value) -> Value {
    let mut d = json!({"name": "Warehouse", "endpoint": endpoint, "region": "us-east-1", "bucket": "exports", "prefix": "dm/",
        "access_key_id": "AKTEST123456", "secret_access_key": "s3cr3t-key-value", "schedule": "manual"});
    for (k, v) in extra.as_object().unwrap() {
        d[k] = v.clone();
    }
    d
}

async fn run_export(h: &mut Harness, dest: &str, kind: &str) -> Value {
    let run = h.ok("POST", &format!("/api/warehouse/destinations/{dest}/runs"), Some(json!({"kind": kind}))).await;
    // A new destination's first run waits a minute (see run.rs); fast-forward past it.
    h.fast_forward().await;
    h.ok("GET", &format!("/api/warehouse/runs/{}", run["id"].as_str().unwrap()), None).await
}

#[tokio::test]
async fn exports_rows_with_their_keys_and_a_manifest() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    h.signup().await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    for (email, consent) in [("p1@x.test", "subscribed"), ("p2@x.test", "subscribed"), ("p3@x.test", "unsubscribed")] {
        h.ok("POST", "/api/profiles", Some(json!({"email": email, "email_consent": consent, "properties": {"plan": "gold"}, "list_ids": [list["id"]]}))).await;
    }
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "value": 12.5, "profile": {"email": "p1@x.test"}, "properties": {"sku": "A, B"}}))).await;
    h.ok("POST", "/api/segments", Some(json!({"name": "Gold", "definition": {"conditions": [{"type": "property", "field": "plan", "op": "eq", "value": "gold"}]}}))).await;
    // Everything so far happened an hour ago (runs leave out the last few minutes).
    sqlx::query("UPDATE profiles SET updated_at = now() - interval '1 hour', created_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE events SET created_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();

    let (s, v) = h.req("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({})))).await;
    assert_eq!(s, 201, "{v}");
    let dest = v["id"].as_str().unwrap().to_string();
    // The keys are never shown again.
    assert!(!v.to_string().contains("s3cr3t") && !v.to_string().contains("AKTEST123456"), "{v}");
    assert_eq!(v["access_key_hint"], "AKTE…3456");
    // The destination was checked with a test file, which was then removed.
    assert_eq!(s3.count("PUT", "dm/_dumbmail_test/", ""), 1);
    assert_eq!(s3.count("DELETE", "dm/_dumbmail_test/", ""), 1);
    assert!(s3.keys("_dumbmail_test").is_empty());

    let run = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let profiles = s3.json_rows("profiles");
    assert_eq!(profiles.len(), 3);
    assert_eq!(profiles[0]["properties"]["plan"], "gold");
    assert!(profiles.iter().all(|p| p["email_consent"].is_string() && p["_updated_at"].is_string()));
    let events = s3.json_rows("events");
    assert_eq!((events.len(), events[0]["metric"].as_str(), events[0]["value"].as_f64()), (1, Some("Placed Order"), Some(12.5)));
    assert_eq!(s3.json_rows("list_members").len(), 3);
    assert_eq!(s3.json_rows("lists")[0]["member_count"], 3);
    assert_eq!(s3.json_rows("suppressions").len(), 1);
    assert_eq!(s3.json_rows("segments")[0]["name"], "Gold");
    let date = chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).format("%Y-%m-%d").to_string();
    let run_id = run["id"].as_str().unwrap();
    assert!(s3.keys("/profiles/").iter().all(|k| k.starts_with(&format!("dm/profiles/dt={date}/run={run_id}/part-")) && k.ends_with(".jsonl.gz")), "{:?}", s3.keys("/profiles/"));
    let manifest: Value = serde_json::from_slice(&s3.objects()[run["manifest_key"].as_str().unwrap()]).unwrap();
    assert_eq!(manifest["format"], "jsonl");
    let ds = |name: &str| manifest["datasets"].as_array().unwrap().iter().find(|d| d["name"] == name).unwrap().clone();
    assert_eq!(ds("profiles")["rows"], 3);
    assert_eq!(ds("profiles")["mode"], "incremental");
    assert_eq!(ds("profiles")["key"], json!(["id"]));
    assert_eq!(ds("list_members")["key"], json!(["list_id", "profile_id"]));
    assert_eq!(ds("lists")["mode"], "snapshot");
    assert_eq!(ds("profiles")["files"][0]["rows"], 3);
    assert_eq!(run["rows"].as_i64().unwrap(), manifest["datasets"].as_array().unwrap().iter().map(|d| d["rows"].as_i64().unwrap()).sum::<i64>());

    // The next run takes what changed since (and re-reads a 15-minute overlap): one profile, no events.
    let mark: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT exported_until FROM warehouse_watermarks WHERE dataset = 'profiles'").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET first_name = 'Pat', updated_at = $1 + interval '1 millisecond' WHERE email = 'p2@x.test'").bind(mark).execute(&h.state.db).await.unwrap();
    let before = s3.keys("/events/").len();
    let run2 = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run2["status"], "succeeded", "{run2}");
    let run2_id = run2["id"].as_str().unwrap();
    let new_profiles: Vec<Value> = s3.keys(&format!("run={run2_id}")).iter().filter(|k| k.contains("/profiles/")).flat_map(|k| s3.lines(k)).map(|l| serde_json::from_str(&l).unwrap()).collect();
    assert_eq!(new_profiles.len(), 1);
    assert_eq!(new_profiles[0]["first_name"], "Pat");
    assert_eq!(s3.keys("/events/").len(), before, "no new events, no new file");

    // CSV, full export: a header row, then every row.
    h.ok("PATCH", &format!("/api/warehouse/destinations/{dest}"), Some(json!({"format": "csv"}))).await;
    let full = run_export(&mut h, &dest, "full").await;
    assert_eq!(full["status"], "succeeded", "{full}");
    let key = s3.keys(&format!("run={}", full["id"].as_str().unwrap())).into_iter().find(|k| k.contains("/events/")).unwrap();
    assert!(key.ends_with(".csv.gz"));
    let lines = s3.lines(&key);
    assert_eq!(lines[0], "id,profile_id,metric,properties,value,occurred_at,created_at,unique_id,message_id,from_browser,_updated_at");
    assert!(lines[1].contains("\"{\"\"sku\"\": \"\"A, B\"\"}\"") || lines[1].contains("\"{\"\"sku\"\":\"\"A, B\"\"}\""), "{}", lines[1]);
    assert_eq!(s3.bad_auth_count(), 0);
    h.cleanup().await;
}

impl S3 {
    fn bad_auth_count(&self) -> usize {
        self.0.lock().unwrap().bad_auth
    }
}

#[tokio::test]
async fn big_files_go_up_in_parts_and_a_failed_run_resumes() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    h.signup().await;
    let account: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    // 12,000 profiles with ~2 KB of random text each: more than one 8 MB part after gzip.
    sqlx::query(
        "INSERT INTO profiles (id, account_id, email, properties, created_at, updated_at)
         SELECT gen_random_uuid(), $1, 'bulk' || i || '@x.test', jsonb_build_object('blob', (SELECT string_agg(md5(random()::text || i || j), '') FROM generate_series(1, 64) j)),
                now() - interval '1 hour', now() - interval '1 hour'
         FROM generate_series(1, 12000) i",
    )
    .bind(account)
    .execute(&h.state.db)
    .await
    .unwrap();
    h.ok("POST", "/api/events", Some(json!({"metric": "Viewed Product", "profile": {"email": "bulk1@x.test"}}))).await;
    // Tracking touched that profile too; runs leave out the last few minutes, so backdate both.
    sqlx::query("UPDATE events SET created_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET updated_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();
    let d = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"datasets": ["profiles", "events"]})))).await;
    let dest = d["id"].as_str().unwrap().to_string();

    // Events uploads fail: profiles are written, then the run waits to retry.
    s3.0.lock().unwrap().fail_keys = Some("/events/".into());
    let run = h.ok("POST", &format!("/api/warehouse/destinations/{dest}/runs"), Some(json!({"kind": "full"}))).await;
    let run_id = run["id"].as_str().unwrap().to_string();
    h.fast_forward().await;
    let r = h.ok("GET", &format!("/api/warehouse/runs/{run_id}"), None).await;
    assert_eq!((r["status"].as_str(), r["attempts"].as_i64()), (Some("running"), Some(1)), "{r}");
    assert_eq!(r["error"], "upload failed: HTTP 500 InternalError", "{r}");
    assert!(s3.count("POST", "/profiles/", "uploads=") >= 1, "a multipart upload was used");
    assert_eq!(s3.json_rows("profiles").len(), 12_000);
    // Another run can't start while this one is going.
    let (s, _) = h.req("POST", &format!("/api/warehouse/destinations/{dest}/runs"), Some(json!({"kind": "incremental"}))).await;
    assert_eq!(s, 409);

    // Storage recovers: the run picks up at events without writing profiles again.
    s3.0.lock().unwrap().fail_keys = None;
    let uploads_before = s3.count("POST", "/profiles/", "uploads=");
    h.fast_forward().await;
    let r = h.ok("GET", &format!("/api/warehouse/runs/{run_id}"), None).await;
    assert_eq!(r["status"], "succeeded", "{r}");
    assert_eq!(s3.count("POST", "/profiles/", "uploads="), uploads_before);
    assert_eq!(s3.json_rows("events").len(), 1);
    assert_eq!(r["rows"], 12_001);

    // Failing five times in a row fails the run and emails the account.
    s3.0.lock().unwrap().fail_keys = Some("dm/".into());
    let run = h.ok("POST", &format!("/api/warehouse/destinations/{dest}/runs"), Some(json!({"kind": "full"}))).await;
    for _ in 0..6 {
        h.fast_forward().await;
    }
    let r = h.ok("GET", &format!("/api/warehouse/runs/{}", run["id"].as_str().unwrap()), None).await;
    assert_eq!(r["status"], "failed", "{r}");
    assert!(h.state.email.sent().iter().any(|m| m.subject == "A data export failed" && m.text.contains("Warehouse")));
    h.cleanup().await;
}

#[tokio::test]
async fn destinations_are_checked_and_kept_to_their_account() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    h.signup().await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "a-only@x.test"}))).await;
    let a_cookie = h.cookie.clone();
    let d = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({})))).await;

    // Bad keys, unknown datasets, plain-http endpoints and private addresses are refused.
    s3.0.lock().unwrap().deny = true;
    let (s, v) = h.req("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({})))).await;
    assert_eq!(s, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("AccessDenied"), "{v}");
    s3.0.lock().unwrap().deny = false;
    let (s, _) = h.req("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"datasets": ["passwords"]})))).await;
    assert_eq!(s, 400);
    let (s, v) = h.req("POST", "/api/warehouse/destinations", Some(destination("http://example.com", json!({})))).await;
    assert_eq!(s, 400, "{v}");
    let (s, _) = h.req("POST", "/api/warehouse/destinations", Some(destination("https://10.0.0.5", json!({})))).await;
    assert_eq!(s, 400);
    let (s, v) = h.req("POST", &format!("/api/warehouse/destinations/{}/test", d["id"].as_str().unwrap()), None).await;
    assert_eq!((s.as_u16(), v["ok"].as_bool()), (200, Some(true)), "{v}");

    // Another account sees none of it, and its export has only its own rows.
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "b@b.test", "password": "correct horse", "company": "B Co"}))).await;
    let (s, _) = h.req("GET", &format!("/api/warehouse/destinations/{}", d["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 404);
    let (s, _) = h.req("POST", &format!("/api/warehouse/destinations/{}/runs", d["id"].as_str().unwrap()), Some(json!({"kind": "full"}))).await;
    assert_eq!(s, 404);
    assert_eq!(h.ok("GET", "/api/warehouse/destinations", None).await["total"], 0);
    h.ok("POST", "/api/profiles", Some(json!({"email": "b-only@x.test"}))).await;
    sqlx::query("UPDATE profiles SET updated_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();
    let db = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"prefix": "b/"})))).await;
    let run = run_export(&mut h, db["id"].as_str().unwrap(), "full").await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let b_rows: Vec<String> = s3.keys("b/profiles/").iter().flat_map(|k| s3.lines(k)).collect();
    assert_eq!(b_rows.len(), 1);
    assert!(b_rows[0].contains("b-only@x.test"));
    h.cookie = a_cookie;
    assert_eq!(h.ok("GET", "/api/warehouse/destinations", None).await["total"], 1);
    h.cleanup().await;
}

#[tokio::test]
async fn scheduled_runs_and_mcp_tools() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    h.signup().await;
    let d = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"schedule": "hourly", "datasets": ["lists"]})))).await;
    assert!(d["next_run_at"].is_string(), "{d}");
    // The scheduler starts the first run right away (it waits a minute for the baseline), then at
    // the top of each hour.
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    h.fast_forward().await;
    let runs = h.ok("GET", &format!("/api/warehouse/destinations/{}/runs", d["id"].as_str().unwrap()), None).await;
    assert_eq!(runs["total"], 1, "{runs}");
    assert_eq!(runs["data"][0]["status"], "succeeded");
    let d2 = h.ok("GET", &format!("/api/warehouse/destinations/{}", d["id"].as_str().unwrap()), None).await;
    let next: chrono::DateTime<chrono::Utc> = d2["next_run_at"].as_str().unwrap().parse().unwrap();
    assert!(next > chrono::Utc::now() && next <= chrono::Utc::now() + chrono::Duration::hours(1));
    assert!(s3.keys("_manifests/").len() == 1);

    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let hdrs = [("authorization", auth.as_str()), ("content-type", "application/json")];
    let call = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string()).await;
    for t in ["create_export_destination", "start_warehouse_export", "list_export_runs", "list_export_datasets"] {
        assert!(body.contains(t), "{t}");
    }
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &call("start_warehouse_export", json!({"id": d["id"], "kind": "full"}))).await;
    assert!(body.contains("dry_run"), "{body}");
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &call("start_warehouse_export", json!({"id": d["id"], "kind": "full", "confirm": true}))).await;
    let res: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(res["result"]["structuredContent"]["kind"], "full", "{res}");
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &call("delete_export_destination", json!({"id": d["id"]}))).await;
    assert!(body.contains("dry_run"));
    let (_, _, body) = h.raw("POST", "/mcp", &hdrs, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "dumbmail://docs/data-export"}}).to_string()).await;
    assert!(body.contains("Snowflake") && body.contains("BigQuery") && body.contains("Redshift"));
    h.cleanup().await;
}

/// Against a real S3-compatible server, when one is given:
/// `WAREHOUSE_TEST_S3=http://127.0.0.1:9000|bucket|access key|secret key` (the bucket must exist).
/// It checks request signing, multipart uploads and the test write for real.
#[tokio::test]
async fn real_s3_compatible_storage() {
    let Ok(spec) = std::env::var("WAREHOUSE_TEST_S3") else {
        eprintln!("WAREHOUSE_TEST_S3 not set; skipping");
        return;
    };
    let parts: Vec<&str> = spec.split('|').collect();
    let mut h = harness!();
    dumbmail::features::webhooks::net::allow_localhost_for_tests();
    h.signup().await;
    let account: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    sqlx::query(
        "INSERT INTO profiles (id, account_id, email, properties, created_at, updated_at)
         SELECT gen_random_uuid(), $1, 'real' || i || '@x.test', jsonb_build_object('blob', (SELECT string_agg(md5(random()::text || i || j), '') FROM generate_series(1, 64) j)),
                now() - interval '1 hour', now() - interval '1 hour'
         FROM generate_series(1, 12000) i",
    )
    .bind(account)
    .execute(&h.state.db)
    .await
    .unwrap();
    let prefix = format!("real-{}/", uuid::Uuid::new_v4().simple());
    let body = json!({"name": "Real", "endpoint": parts[0], "region": "us-east-1", "bucket": parts[1], "prefix": prefix, "access_key_id": parts[2], "secret_access_key": parts[3], "schedule": "manual"});
    let mut bad = body.clone();
    bad["secret_access_key"] = json!("not-the-secret");
    let (s, v) = h.req("POST", "/api/warehouse/destinations", Some(bad)).await;
    assert_eq!(s, 400, "a wrong secret is refused: {v}");
    let d = h.ok("POST", "/api/warehouse/destinations", Some(body)).await;
    let run = run_export(&mut h, d["id"].as_str().unwrap(), "full").await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert!(run["bytes"].as_i64().unwrap() > 8 * 1024 * 1024, "big enough for a multipart upload: {run}");
    eprintln!("wrote {} rows under {prefix}; manifest {}", run["rows"], run["manifest_key"]);
    h.cleanup().await;
}

#[tokio::test]
async fn a_message_exported_before_its_delivery_comes_back_with_its_final_status() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    h.signup().await;
    h.credit(10.0).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "late@x.test", "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    let d = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"datasets": ["messages", "profiles"]})))).await;
    let dest = d["id"].as_str().unwrap().to_string();
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Launch", "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;

    // The first export has the message as sent.
    let run = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let first = s3.json_rows("messages");
    assert_eq!((first.len(), first[0]["status"].as_str()), (1, Some("sent")));
    // Hours later, after the export, the provider confirms delivery. Nothing about the row's own
    // time columns moved past the watermark except delivered_at.
    let pid: String = sqlx::query_scalar("SELECT provider_message_id FROM messages WHERE NOT is_test").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE messages SET created_at = now() - interval '3 hours', sent_at = now() - interval '3 hours'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE warehouse_watermarks SET exported_until = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    let sns = json!({"Type": "Notification", "Message": json!({"eventType": "Delivery", "mail": {"messageId": pid}}).to_string()});
    let (s, _, _) = h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &sns.to_string()).await;
    assert_eq!(s, 200);

    // The next export has it again, with its final status, newer than the first copy.
    let run2 = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run2["status"], "succeeded", "{run2}");
    let run2_id = run2["id"].as_str().unwrap();
    let again: Vec<Value> = s3.keys(&format!("run={run2_id}")).iter().filter(|k| k.contains("/messages/")).flat_map(|k| s3.lines(k)).map(|l| serde_json::from_str(&l).unwrap()).collect();
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0]["id"], first[0]["id"]);
    assert_eq!(again[0]["status"], "delivered");
    assert!(again[0]["delivered_at"].is_string());
    let (a, b): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = (first[0]["_updated_at"].as_str().unwrap().parse().unwrap(), again[0]["_updated_at"].as_str().unwrap().parse().unwrap());
    assert!(b > a, "keep the copy with the latest _updated_at");
    h.cleanup().await;
}

#[tokio::test]
async fn an_upload_is_done_only_when_the_storage_service_says_so() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    h.signup().await;
    let account: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    // Big enough for a multipart upload.
    sqlx::query(
        "INSERT INTO profiles (id, account_id, email, properties, created_at, updated_at)
         SELECT gen_random_uuid(), $1, 'mp' || i || '@x.test', jsonb_build_object('blob', (SELECT string_agg(md5(random()::text || i || j), '') FROM generate_series(1, 64) j)),
                now() - interval '1 hour', now() - interval '1 hour'
         FROM generate_series(1, 12000) i",
    )
    .bind(account)
    .execute(&h.state.db)
    .await
    .unwrap();
    let d = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"datasets": ["profiles"]})))).await;
    let dest = d["id"].as_str().unwrap().to_string();

    // CompleteMultipartUpload answers 200, then an error document: that's a failure.
    s3.0.lock().unwrap().complete_error = true;
    let run = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run["status"], "running", "{run}");
    assert_eq!(run["error"], "finishing a multipart upload failed: HTTP 200 InternalError", "{run}");
    assert_eq!(run["files"], 0);
    assert!(run["manifest_key"].is_null());
    assert!(s3.keys("_manifests/").is_empty(), "no manifest for a file that never completed");
    assert_eq!(s3.count("DELETE", "/profiles/", "uploadId="), 1, "the upload was aborted");
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM warehouse_watermarks").fetch_one(&h.state.db).await.unwrap(), 0);
    // Nothing the storage service said is kept beyond its code.
    let err = sqlx::query_scalar::<_, Option<String>>("SELECT error FROM warehouse_runs").fetch_one(&h.state.db).await.unwrap().unwrap();
    assert!(!err.contains("encountered"), "{err}");

    // When it works again the run finishes, and only then do the manifest and watermark appear.
    s3.0.lock().unwrap().complete_error = false;
    h.fast_forward().await;
    let r = h.ok("GET", &format!("/api/warehouse/runs/{}", run["id"].as_str().unwrap()), None).await;
    assert_eq!(r["status"], "succeeded", "{r}");
    assert_eq!(s3.json_rows("profiles").len(), 12_000);
    assert_eq!(s3.keys("_manifests/").len(), 1);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM warehouse_watermarks").fetch_one(&h.state.db).await.unwrap(), 1);
    h.cleanup().await;
}

#[tokio::test]
async fn moving_a_destination_cancels_its_run_and_needs_its_keys_again() {
    let mut h = harness!();
    let (s3, endpoint) = mock_s3().await;
    let (s3b, endpoint_b) = mock_s3().await;
    h.signup().await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "m1@x.test"}))).await;
    let d = h.ok("POST", "/api/warehouse/destinations", Some(destination(&endpoint, json!({"datasets": ["profiles", "lists"]})))).await;
    let dest = d["id"].as_str().unwrap().to_string();
    let run = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM warehouse_watermarks").fetch_one(&h.state.db).await.unwrap(), 1);

    // A run is waiting when the destination moves to another host.
    let waiting = h.ok("POST", &format!("/api/warehouse/destinations/{dest}/runs"), Some(json!({"kind": "incremental"}))).await;
    // The saved keys are never sent to a new host: they must be given again.
    let (s, v) = h.req("PATCH", &format!("/api/warehouse/destinations/{dest}"), Some(json!({"endpoint": endpoint_b}))).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("access_key_id"), "{v}");
    assert_eq!(s3b.0.lock().unwrap().log.len(), 0, "nothing was signed for the new host with the old keys");
    let moved = h.ok("PATCH", &format!("/api/warehouse/destinations/{dest}"), Some(json!({"endpoint": endpoint_b, "access_key_id": "AKTEST123456", "secret_access_key": "another-secret"}))).await;
    assert!(moved["id"].is_string());
    let w = h.ok("GET", &format!("/api/warehouse/runs/{}", waiting["id"].as_str().unwrap()), None).await;
    assert_eq!((w["status"].as_str(), w["error"].as_str()), (Some("cancelled"), Some("destination_changed")), "{w}");
    // The next run starts a fresh baseline at the new place: every row, not just changes.
    h.fast_forward().await;
    let before = s3.objects().len();
    let run2 = run_export(&mut h, &dest, "incremental").await;
    assert_eq!(run2["status"], "succeeded", "{run2}");
    assert!(run2["datasets"][0]["after"].is_null(), "{run2}");
    assert_eq!(s3b.json_rows("profiles").len(), 1);
    assert_eq!(s3.objects().len(), before, "nothing more went to the old bucket");

    // A prefix change is a move too; a run reads its settings once, when it starts.
    let r3 = h.ok("POST", &format!("/api/warehouse/destinations/{dest}/runs"), Some(json!({"kind": "full"}))).await;
    h.ok("PATCH", &format!("/api/warehouse/destinations/{dest}"), Some(json!({"name": "Renamed"}))).await;
    h.fast_forward().await;
    let r3 = h.ok("GET", &format!("/api/warehouse/runs/{}", r3["id"].as_str().unwrap()), None).await;
    assert_eq!(r3["status"], "succeeded", "a rename doesn't stop a run: {r3}");
    let (s, _) = h.req("PATCH", &format!("/api/warehouse/destinations/{dest}"), Some(json!({"prefix": "dm/./x"}))).await;
    assert_eq!(s, 400, "dot segments would change the signed path");
    h.cleanup().await;
}

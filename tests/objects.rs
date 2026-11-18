//! Custom objects: types, upserts with history, permissions and isolation, flows off object
//! changes and dates, segments, condition splits and integration actions.

#[macro_use]
mod common;
use common::*;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

macro_rules! integ_harness {
    () => {{
        dumbmail::features::integrations::sandbox::set_program(env!("CARGO_BIN_EXE_dumbmail"));
        dumbmail::features::integrations::sandbox::set_global_cap(64);
        harness!()
    }};
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

fn today() -> chrono::NaiveDate {
    chrono::Utc::now().with_timezone(&chrono_tz::America::Los_Angeles).date_naive()
}

fn day(d: i64) -> String {
    (today() + chrono::Duration::days(d)).format("%Y-%m-%d").to_string()
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

async fn mcp(h: &Harness, key: &str, tool: &str, args: Value) -> (bool, Value, String) {
    let (st, v) = bearer(h, key, "POST", "/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let r = &v["result"];
    (r["isError"].as_bool().unwrap_or(false), r["structuredContent"].clone(), r["content"][0]["text"].as_str().unwrap_or("").to_string())
}

async fn set_role(h: &Harness, role: &str) {
    sqlx::query("UPDATE memberships SET role = $1").bind(role).execute(&h.state.db).await.unwrap();
}

fn reservation(id: &str, email: &str, fields: Value) -> Value {
    json!({"type": "reservation", "external_id": id, "fields": fields, "profiles": [{"email": email, "relationship": "guest"}]})
}

#[tokio::test]
async fn upserts_merge_record_history_and_stay_in_their_account() {
    let mut h = harness!();
    h.signup().await;
    // First upsert makes the type, guessing field types from the values.
    let r = h.ok("POST", "/api/objects", Some(reservation("R1", "ana@example.com", json!({"status": "confirmed", "check_in": day(10), "nights": 3, "pets": false, "name": "Beach house"})))).await;
    assert_eq!(r["result"], "created", "{r}");
    let t = h.ok("GET", "/api/object-types/reservation", None).await;
    let kind = |k: &str| t["fields"].as_array().unwrap().iter().find(|f| f["key"] == k).unwrap()["type"].clone();
    assert_eq!((kind("check_in"), kind("nights"), kind("pets"), kind("status")), (json!("date"), json!("number"), json!("boolean"), json!("text")));
    assert_eq!(t["title_field"], "name");
    // The guest was created, never subscribed, and not changed by later links.
    let (consent, held): (String, bool) = sqlx::query_as("SELECT email_consent, marketing_hold FROM profiles WHERE email = 'ana@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((consent.as_str(), held), ("never_subscribed", false));
    let id = r["id"].as_str().unwrap().to_string();

    // Merge: a changed field, a removed one; history keeps what changed.
    let r = h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": day(12), "pets": null}, "as_of": "2026-11-17T10:00:00Z"}))).await;
    assert_eq!(r["result"], "updated");
    assert_eq!(r["changed"], json!(["check_in", "pets"]));
    // The same again changes nothing and records nothing.
    let r = h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": day(12)}}))).await;
    assert_eq!(r["result"], "unchanged");
    // An older state (by as_of) never overwrites a newer one.
    let r = h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"status": "cancelled"}, "as_of": "2026-11-17T09:00:00Z"}))).await;
    assert_eq!(r["result"], "older");
    let o = h.ok("GET", &format!("/api/objects/{id}"), None).await;
    assert_eq!(o["fields"]["status"], "confirmed");
    assert_eq!(o["fields"]["check_in"], day(12));
    assert!(o["fields"].get("pets").is_none());
    let hist = o["history"].as_array().unwrap();
    assert_eq!(hist.len(), 2, "{o}");
    assert_eq!(hist[0]["before"]["check_in"], day(10));
    assert_eq!(hist[0]["after"]["check_in"], day(12));
    assert_eq!(o["profiles"][0]["email"], "ana@example.com");
    assert_eq!(o["profiles"][0]["relationship"], "guest");

    // Values are checked against the field's type.
    let (s, e) = h.req("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R2", "fields": {"check_in": "next tuesday"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(count(&h, "SELECT count(*) FROM objects WHERE external_id = 'R2'").await, 0, "nothing half-written");
    let (s, _) = h.req("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R2", "fields": {"nested": {"a": 1}}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Sensitive fields: never in lists; only for people who can read profiles.
    let mut fields = t["fields"].as_array().unwrap().clone();
    fields.push(json!({"key": "door_code", "type": "text", "sensitive": true}));
    h.ok("PATCH", "/api/object-types/reservation", Some(json!({"fields": fields}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"door_code": "4417"}}))).await;
    let list = h.ok("GET", "/api/object-types/reservation/objects?q=beach", None).await;
    assert_eq!(list["total"], 1);
    assert!(!list.to_string().contains("4417"), "{list}");
    let o = h.ok("GET", &format!("/api/objects/{id}"), None).await;
    assert_eq!(o["fields"]["door_code"], "4417", "the owner can read profiles");
    // Rules can't test a sensitive field (a segment count or a flow start would tell who has which code).
    let (st, e) = h.req("POST", "/api/segments/preview", Some(json!({"definition": {"conditions": [{"type": "object", "object_type": "reservation", "where": [{"field": "door_code", "op": "eq", "value": "4417"}]}]}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
    let (st, e) = h.req("POST", "/api/flows", Some(json!({"name": "Probe", "trigger": {"type": "object", "object_type": "reservation", "filters": [{"field": "door_code", "op": "starts_with", "value": "4"}]}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
    // A field taken off the type keeps its values, but they're held back like sensitive ones.
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"wifi": "pw-77"}}))).await;
    let defs: Vec<Value> = h.ok("GET", "/api/object-types/reservation", None).await["fields"].as_array().unwrap().iter().filter(|f| f["key"] != "wifi").cloned().collect();
    h.ok("PATCH", "/api/object-types/reservation", Some(json!({"fields": defs}))).await;
    assert!(!h.ok("GET", "/api/object-types/reservation/objects", None).await.to_string().contains("pw-77"));
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'ana@example.com'").fetch_one(&h.state.db).await.unwrap();
    let mine = h.ok("GET", &format!("/api/profiles/{pid}/objects"), None).await;
    assert_eq!(mine["types"][0]["objects"][0]["external_id"], "R1");
    assert!(!mine.to_string().contains("4417"));

    // A content member: can see objects, not who they belong to or their sensitive fields; can't write.
    set_role(&h, "content").await;
    let o = h.ok("GET", &format!("/api/objects/{id}"), None).await;
    assert!(o.get("profiles").is_none() && !o.to_string().contains("4417") && !o.to_string().contains("ana@"), "{o}");
    assert_eq!(o["hidden_fields"], json!(["door_code", "wifi"]));
    assert!(!o.to_string().contains("pw-77"), "{o}");
    assert_eq!(h.req("POST", "/api/objects", Some(reservation("R3", "x@example.com", json!({})))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(h.req("GET", &format!("/api/profiles/{pid}/objects"), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(h.req("DELETE", &format!("/api/objects/{id}"), None).await.0, StatusCode::FORBIDDEN);
    set_role(&h, "manager").await;
    h.ok("PATCH", "/api/object-types/reservation", Some(json!({"name": "Stay"}))).await;
    set_role(&h, "analyst").await;
    assert_eq!(h.req("PATCH", "/api/object-types/reservation", Some(json!({"name": "Booking"}))).await.0, StatusCode::FORBIDDEN, "changing types needs settings write");
    set_role(&h, "owner").await;

    // Another account sees none of it and can't link to this account's people.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@rival.test", "password": "correct horse", "company": "Rival"}))).await;
    assert_eq!(h.req("GET", &format!("/api/objects/{id}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.req("GET", "/api/object-types/reservation", None).await.0, StatusCode::NOT_FOUND);
    let (s, _) = h.req("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "profiles": [{"id": pid}]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM object_links WHERE profile_id = '{pid}'")).await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn types_deletes_bulk_and_mcp() {
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent", "scopes": ["full"]}))).await["key"].as_str().unwrap().to_string();
    let (err, v, text) = mcp(&h, &key, "upsert_object_type", json!({"key": "subscription", "fields": [{"key": "plan", "type": "enum", "options": ["basic", "pro"]}, {"key": "renews_on", "type": "date"}], "title_field": "plan"})).await;
    assert!(!err, "{text}");
    assert_eq!(v["key"], "subscription");
    let (err, v, text) = mcp(&h, &key, "upsert_objects", json!({"objects": [
        {"type": "subscription", "external_id": "S1", "fields": {"plan": "PRO", "renews_on": day(5)}, "profiles": [{"email": "a@example.com"}]},
        {"type": "subscription", "external_id": "S2", "fields": {"plan": "gold"}}]})).await;
    assert!(!err, "{text}");
    assert_eq!(v["results"][0]["result"], "created");
    assert!(v["results"][1]["error"].as_str().unwrap().contains("basic, pro"), "{v}");
    let (_, o, _) = mcp(&h, &key, "get_object", json!({"type": "subscription", "external_id": "S1"})).await;
    assert_eq!(o["fields"]["plan"], "pro", "enum values take the option's own spelling");

    // Bulk: queued, written by a background job in chunks, counted.
    let many: Vec<Value> = (0..230).map(|i| json!({"type": "subscription", "external_id": format!("B{i}"), "fields": {"plan": "basic"}})).collect();
    let (s, b) = h.req("POST", "/api/objects/bulk", Some(json!({"objects": many}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    h.drain().await;
    let st = h.ok("GET", &format!("/api/objects/bulk/{}", b["id"].as_str().unwrap()), None).await;
    assert_eq!((st["status"].clone(), st["created"].clone(), st["done"].clone()), (json!("done"), json!(230), json!(230)), "{st}");
    assert_eq!(count(&h, "SELECT count(*) FROM object_bulk_upserts WHERE items IS NOT NULL").await, 0, "the batch isn't kept once written");
    // A replayed job for a finished batch writes nothing more.
    dumbmail::jobs::enqueue_background(&h.state.db, "objects_bulk", json!({"bulk_id": b["id"]}), None).await.unwrap();
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM object_changes").await, 231);
    let too_many: Vec<Value> = (0..1001).map(|i| json!({"type": "subscription", "external_id": format!("X{i}")})).collect();
    assert_eq!(h.req("POST", "/api/objects/bulk", Some(json!({"objects": too_many}))).await.0, StatusCode::BAD_REQUEST);

    // Deletes need confirm, and are audited with counts only.
    let (err, dry, _) = mcp(&h, &key, "delete_object", json!({"type": "subscription", "external_ids": ["B1", "B2"]})).await;
    assert!(!err && dry["dry_run"] == true && dry["objects"] == 2, "{dry}");
    assert_eq!(count(&h, "SELECT count(*) FROM objects WHERE external_id IN ('B1', 'B2')").await, 2);
    let (_, done, _) = mcp(&h, &key, "delete_object", json!({"type": "subscription", "external_ids": ["B1", "B2"], "confirm": true})).await;
    assert_eq!(done["deleted"], 2);
    let audit: Value = sqlx::query_scalar("SELECT details FROM audit_log WHERE action = 'objects.delete'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(audit, json!({"count": 2, "types": ["subscription"]}));

    // A type in use by a flow can't be deleted; otherwise it needs confirm.
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "Renewal", "trigger": {"type": "object_date", "object_type": "subscription", "field": "renews_on", "offset_days": -3}}))).await;
    let (err, _, text) = mcp(&h, &key, "delete_object_type", json!({"key": "subscription", "confirm": true})).await;
    assert!(err && text.contains("Renewal"), "{text}");
    let (s, e) = h.req("POST", "/api/flows", Some(json!({"name": "Bad", "trigger": {"type": "object_date", "object_type": "subscription", "field": "plan"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "only a date field can drive a date trigger: {e}");
    assert_eq!(h.req("POST", "/api/flows", Some(json!({"name": "Bad", "trigger": {"type": "object", "object_type": "nope"}}))).await.0, StatusCode::NOT_FOUND);
    h.ok("DELETE", &format!("/api/flows/{}", f["id"].as_str().unwrap()), None).await;
    let (_, dry, _) = mcp(&h, &key, "delete_object_type", json!({"key": "subscription"})).await;
    assert_eq!(dry["dry_run"], true);
    let (_, gone, _) = mcp(&h, &key, "delete_object_type", json!({"key": "subscription", "confirm": true})).await;
    assert_eq!(gone["deleted"], true);
    assert_eq!(count(&h, "SELECT count(*) FROM objects").await, 0);
    h.cleanup().await;
}

fn email_flow(name: &str, trigger: Value, subject: &str) -> Value {
    json!({"name": name, "trigger": trigger, "definition": {"entry": "e1", "nodes": [
        {"id": "e1", "type": "email", "next": null, "content": {"subject": subject, "blocks": [{"type": "text", "html": "<p>Check-in {{ object.check_in|date }} at {{ object.listing|default('our place') }}</p>"}]}}]}})
}

async fn live(h: &mut Harness, body: Value) -> String {
    let f = h.ok("POST", "/api/flows", Some(body)).await;
    let id = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
    id
}

#[tokio::test]
async fn object_date_flows_follow_dates_skip_cancelled_and_send_once() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [
        {"key": "status", "type": "enum", "options": ["confirmed", "cancelled"]}, {"key": "check_in", "type": "date"}, {"key": "listing", "type": "text"}]}))).await;
    // One reservation exists before the flow (scheduled by the seeding); the rest after.
    h.ok("POST", "/api/objects", Some(reservation("R1", "ana@example.com", json!({"status": "confirmed", "check_in": day(2), "listing": "Dune Cottage"})))).await;
    let trigger = json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_days": -2, "hour": 0, "relationship": "guest",
        "filters": [{"field": "status", "op": "eq", "value": "confirmed"}]});
    let fid = live(&mut h, email_flow("Pre-arrival", trigger, "See you {{ object.external_id }}")).await;
    // Ana's second reservation: a second run.
    h.ok("POST", "/api/objects", Some(reservation("R2", "ana@example.com", json!({"status": "confirmed", "check_in": day(2)})))).await;
    // Ben's is cancelled before it's due: filtered.
    h.ok("POST", "/api/objects", Some(reservation("R3", "ben@example.com", json!({"status": "confirmed", "check_in": day(2)})))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R3", "fields": {"status": "cancelled"}}))).await;
    // Cara's moves out to day 10: no send now, one pending for the new date.
    h.ok("POST", "/api/objects", Some(reservation("R4", "cara@example.com", json!({"status": "confirmed", "check_in": day(2)})))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R4", "fields": {"check_in": day(10)}}))).await;
    // Dan unsubscribed: the run starts, the send path skips him.
    h.ok("POST", "/api/objects", Some(reservation("R5", "dan@example.com", json!({"status": "confirmed", "check_in": day(2)})))).await;
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE email = 'dan@example.com'").execute(&h.state.db).await.unwrap();

    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    let sent = |h: &Harness| {
        let mut v: Vec<(String, String)> = h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("See you")).map(|m| (m.to, m.subject)).collect();
        v.sort();
        v
    };
    assert_eq!(sent(&h), vec![("ana@example.com".into(), "See you R1".into()), ("ana@example.com".into(), "See you R2".into())]);
    let body = h.state.email.sent().into_iter().find(|m| m.subject == "See you R1").unwrap().html;
    let pretty = (today() + chrono::Duration::days(2)).format("%B %-d, %Y").to_string();
    assert!(body.contains(&pretty) && body.contains("Dune Cottage"), "object fields and the date filter: {pretty}");
    let report = h.ok("GET", &format!("/api/flows/{fid}/report"), None).await;
    let c = &report["object_dates"]["counts"];
    assert_eq!((c["started"].clone(), c["filtered"].clone(), c["pending"].clone()), (json!(3), json!(1), json!(1)), "{report}");
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs").await, 3, "one run per object and person");

    // Running the scheduler again sends nothing more; a fire is claimed once.
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h).len(), 2);
    // Moving a date back to a moment that already fired never sends it again.
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": day(5)}}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": day(2)}}))).await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h).len(), 2);

    // A fire more than a day late is missed, not sent late.
    sqlx::query("UPDATE flow_object_date_fires SET due_at = now() - interval '30 hours' WHERE outcome = 'pending'").execute(&h.state.db).await.unwrap();
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h).len(), 2);
    // Only Cara's moved date was pending (R1's in-between date was dropped when it moved back).
    assert_eq!(count(&h, "SELECT count(*) FROM flow_object_date_fires WHERE outcome = 'missed'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM flow_object_date_fires WHERE outcome = 'pending'").await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn runs_leave_when_the_object_moves_is_cancelled_or_deleted() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "status", "type": "text"}, {"key": "check_in", "type": "date"}, {"key": "nights", "type": "number"}]}))).await;
    let trigger = json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_days": -3, "hour": 0, "filters": [{"field": "status", "op": "neq", "value": "cancelled"}]});
    // Wait a day, then a split on the object: long stays get one email, short stays another.
    live(&mut h, json!({"name": "Prep", "trigger": trigger, "definition": {"entry": "d1", "nodes": [
        {"id": "d1", "type": "delay", "amount": 1, "unit": "days", "next": "s1"},
        {"id": "s1", "type": "condition", "condition": {"conditions": []}, "object_filters": [{"field": "nights", "op": "gte", "value": 7}], "yes": "long", "no": "short"},
        {"id": "long", "type": "email", "next": null, "content": {"subject": "Long stay {{ object.external_id }}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}},
        {"id": "short", "type": "email", "next": null, "content": {"subject": "Short stay {{ object.external_id }}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}]}})).await;
    for (id, who, nights) in [("A", "a@example.com", 9), ("B", "b@example.com", 2), ("C", "c@example.com", 2), ("D", "d@example.com", 2), ("E", "e@example.com", 2)] {
        h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": id, "fields": {"status": "confirmed", "check_in": day(3), "nights": nights}, "profiles": [{"email": who}]}))).await;
    }
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs WHERE status = 'active'").await, 5);
    // While they wait: C's date moves, D is cancelled, E is deleted.
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "C", "fields": {"check_in": day(9)}}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "D", "fields": {"status": "cancelled"}}))).await;
    let e: uuid::Uuid = sqlx::query_scalar("SELECT id FROM objects WHERE external_id = 'E'").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", "/api/objects/delete", Some(json!({"ids": [e], "confirm": true}))).await;
    h.fast_forward().await;
    let mut subjects: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject.contains("stay")).map(|m| m.subject).collect();
    subjects.sort();
    assert_eq!(subjects, vec!["Long stay A", "Short stay B"]);
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs WHERE status = 'exited'").await, 3);
    h.cleanup().await;
}

#[tokio::test]
async fn object_change_flows_start_once_per_object_and_change() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "status", "type": "text"}, {"key": "check_in", "type": "date"}]}))).await;
    let trigger = json!({"type": "object", "object_type": "reservation", "on": "updated", "field": "status", "to": "cancelled"});
    live(&mut h, json!({"name": "Sorry", "trigger": trigger, "definition": {"entry": "e1", "settings": {"reentry": "never"}, "nodes": [
        {"id": "e1", "type": "email", "next": null, "content": {"subject": "Cancelled {{ object.external_id }}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}]}})).await;
    for id in ["R1", "R2"] {
        h.ok("POST", "/api/objects", Some(reservation(id, "ana@example.com", json!({"status": "confirmed", "check_in": day(20)})))).await;
    }
    h.drain().await;
    // A change to another field starts nothing; both cancellations do (one per reservation).
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"check_in": day(21)}}))).await;
    for id in ["R1", "R2"] {
        h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": id, "fields": {"status": "cancelled"}}))).await;
    }
    h.fast_forward().await;
    let subjects = |h: &Harness| {
        let mut v: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("Cancelled")).map(|m| m.subject).collect();
        v.sort();
        v
    };
    assert_eq!(subjects(&h), vec!["Cancelled R1", "Cancelled R2"]);
    // The same change's job run again (a retry after a crash) starts nothing twice.
    let changes: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM object_changes WHERE 'status' = ANY(changed) AND kind = 'updated'").fetch_all(&h.state.db).await.unwrap();
    for c in &changes {
        dumbmail::jobs::enqueue(&h.state.db, "objects_change", json!({"change_id": c}), None).await.unwrap();
    }
    // Reinstated and cancelled again: re-entry "never" counts this object's runs.
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"status": "confirmed"}}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"status": "cancelled"}}))).await;
    h.fast_forward().await;
    assert_eq!(subjects(&h).len(), 2);
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs").await, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn segments_ask_about_objects() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "status", "type": "text"}, {"key": "check_in", "type": "date"}, {"key": "check_out", "type": "date"}]}))).await;
    h.ok("POST", "/api/objects", Some(reservation("R1", "soon@example.com", json!({"status": "confirmed", "check_in": day(5), "check_out": day(8)})))).await;
    h.ok("POST", "/api/objects", Some(reservation("R2", "later@example.com", json!({"status": "confirmed", "check_in": day(40), "check_out": day(45)})))).await;
    h.ok("POST", "/api/objects", Some(reservation("R3", "past@example.com", json!({"status": "confirmed", "check_in": day(-10), "check_out": day(-7)})))).await;
    h.ok("POST", "/api/objects", Some(reservation("R4", "today@example.com", json!({"status": "confirmed", "check_in": day(0), "check_out": day(2)})))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "none@example.com"}))).await;
    let who = |v: &Value| {
        let mut e: Vec<String> = v["sample"].as_array().unwrap().iter().map(|p| p["email"].as_str().unwrap().to_string()).collect();
        e.sort();
        e
    };
    let soon = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"conditions": [{"type": "object", "object_type": "reservation", "relationship": "guest",
        "where": [{"field": "status", "op": "eq", "value": "confirmed"}, {"field": "check_in", "op": "within_next_days", "value": 14}]}]}}))).await;
    assert_eq!(who(&soon), vec!["soon@example.com", "today@example.com"], "{soon}");
    let no_active = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"conditions": [{"type": "object", "object_type": "reservation", "op": "zero",
        "where": [{"field": "check_out", "op": "in_future"}]}]}}))).await;
    assert_eq!(who(&no_active), vec!["none@example.com", "past@example.com"], "{no_active}");
    // Another account's objects never count, even for the same type key.
    let wrong_rel = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"conditions": [{"type": "object", "object_type": "reservation", "relationship": "owner"}]}}))).await;
    assert_eq!(wrong_rel["count"], 0);
    h.cleanup().await;
}

#[tokio::test]
async fn integrations_write_objects_once_without_codes_and_hold_new_guests() {
    let mut h = integ_harness!();
    h.signup().await;
    let script = r#"function handle(input) {
      var p = input.payload;
      if (p.gone) return [{type: 'delete_object', object_type: 'stay', external_id: p.id}];
      return [{type: 'upsert_object', object_type: 'stay', external_id: p.id, as_of: p.at,
               fields: {check_in: p.check_in, doorCode: p.code, notes: {accessCode: p.code}},
               profiles: [{email: p.email, relationship: 'guest'}]}];
    }"#;
    let i = h.ok("POST", "/api/integrations", Some(json!({"name": "PMS", "script": script, "redact": ["**.doorCode", "**.notes"]}))).await;
    let iid = i["id"].as_str().unwrap().to_string();
    let url = i["inbound_url"].as_str().unwrap();
    let path = url[url.find("/webhooks/in/").unwrap()..].to_string();
    // A test run writes nothing and says what would happen.
    let t = h.ok("POST", &format!("/api/integrations/{iid}/test"), Some(json!({"payload": {"id": "S1", "check_in": day(4), "code": "8812", "email": "guest@example.com", "at": "2026-11-17T10:00:00Z"}}))).await;
    assert!(t.to_string().contains("would create stay S1"), "{t}");
    assert_eq!(count(&h, "SELECT count(*) FROM objects").await, 0);
    let body = json!({"id": "S1", "check_in": day(4), "code": "8812", "email": "guest@example.com", "at": "2026-11-17T10:00:00Z"}).to_string();
    let (s, _, r) = h.raw("POST", &path, &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    h.drain().await;
    let (fields, source): (Value, String) = sqlx::query_as("SELECT fields, source FROM objects WHERE external_id = 'S1'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(fields, json!({"check_in": day(4)}), "redacted before it was written");
    assert_eq!(source, format!("integration:{iid}"));
    let (held, email_source): (bool, Option<String>) = sqlx::query_as("SELECT marketing_hold, email_source FROM profiles WHERE email = 'guest@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(held, "a guest the integration added without an opt-in is held back from marketing");
    assert_eq!(email_source, Some(format!("integration:{iid}")));
    let everything: String = sqlx::query_scalar("SELECT concat_ws(' ', (SELECT string_agg(fields::text, ' ') FROM objects), (SELECT string_agg(before::text || after::text, ' ') FROM object_changes), (SELECT string_agg(result::text, ' ') FROM integration_deliveries))").fetch_one(&h.state.db).await.unwrap();
    assert!(!everything.contains("8812"), "{everything}");
    let dl: Value = sqlx::query_scalar("SELECT result FROM integration_deliveries WHERE source = 'webhook'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(dl["counts"]["objects_upserted"], 1, "{dl}");
    // A replay applies it again and changes nothing; an older message never wins.
    let d: uuid::Uuid = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE source = 'webhook'").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/integrations/{iid}/replay"), Some(json!({"delivery_ids": [d]}))).await;
    let older = json!({"id": "S1", "check_in": day(9), "email": "guest@example.com", "at": "2026-11-16T10:00:00Z"}).to_string();
    h.raw("POST", &path, &[("content-type", "application/json")], &older).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM object_changes").await, 1);
    let (s, _, _) = h.raw("POST", &path, &[("content-type", "application/json")], &json!({"id": "S1", "gone": true}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM objects").await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn guesty_keeps_reservations_as_objects_without_door_codes() {
    let mut h = integ_harness!();
    h.signup().await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    let id = created["integration"]["id"].as_str().unwrap().to_string();
    assert!(created["flows"].as_array().unwrap().iter().any(|f| f["name"].as_str().unwrap().contains("from the reservation")), "{created}");
    let t = h.ok("GET", "/api/object-types/reservation", None).await;
    assert_eq!(t["fields"].as_array().unwrap().iter().find(|f| f["key"] == "check_in").unwrap()["type"], "date");
    // Signatures are tested elsewhere; this one checks what's kept.
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"signature": {"type": "none"}}))).await;
    let i = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    let url = i["inbound_url"].as_str().unwrap();
    let path = url[url.find("/webhooks/in/").unwrap()..].to_string();
    let p = format!("{}/recipes/guesty/fixtures/webhook-updated-with-door-code.json", env!("CARGO_MANIFEST_DIR"));
    let body = serde_json::from_str::<Value>(&std::fs::read_to_string(p).unwrap()).unwrap()["input"]["payload"].to_string();
    let (s, _, r) = h.raw("POST", &path, &[("content-type", "application/json"), ("svix-id", "msg-door")], &body).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    h.drain().await;
    let (fields, links): (Value, i64) = sqlx::query_as("SELECT fields, (SELECT count(*) FROM object_links l WHERE l.object_id = o.id AND l.relationship = 'guest') FROM objects o").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(fields["check_in"], "2026-12-19", "{fields}");
    assert_eq!(links, 1);
    for code in ["5512", "1234", "9876", "keyCode", "Lockbox", "guestContact", "notes"] {
        assert!(!fields.to_string().contains(code), "{code:?} reached the reservation object: {fields}");
    }
    h.cleanup().await;
}

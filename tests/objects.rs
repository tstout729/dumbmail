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
    assert_eq!(hist.len(), 3, "created, Ana linked, updated: {o}");
    assert_eq!(hist[1]["kind"], "links");
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
    assert_eq!(count(&h, "SELECT count(*) FROM object_changes WHERE kind <> 'links'").await, 231);
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
    let everything: String = sqlx::query_scalar("SELECT concat_ws(' ', (SELECT string_agg(fields::text, ' ') FROM objects), (SELECT string_agg(before::text || after::text, ' ') FROM object_changes), (SELECT string_agg(concat_ws(' ', result::text, actions::text), ' ') FROM integration_deliveries))").fetch_one(&h.state.db).await.unwrap();
    assert!(!everything.contains("8812"), "{everything}");
    let dl: Value = sqlx::query_scalar("SELECT result FROM integration_deliveries WHERE source = 'webhook'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(dl["counts"]["objects_upserted"], 1, "{dl}");
    // A replay applies it again and changes nothing; an older message never wins.
    let d: uuid::Uuid = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE source = 'webhook'").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/integrations/{iid}/replay"), Some(json!({"delivery_ids": [d]}))).await;
    let older = json!({"id": "S1", "check_in": day(9), "email": "guest@example.com", "at": "2026-11-16T10:00:00Z"}).to_string();
    h.raw("POST", &path, &[("content-type", "application/json")], &older).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM object_changes WHERE kind <> 'links'").await, 1);
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

#[tokio::test]
async fn two_change_jobs_for_one_object_start_one_run() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "status", "type": "text"}, {"key": "nights", "type": "number"}]}))).await;
    let fid = live(&mut h, json!({"name": "Changed", "trigger": {"type": "object", "object_type": "reservation", "on": "updated"}, "definition": {"entry": "d1", "settings": {"reentry": "never"}, "nodes": [
        {"id": "d1", "type": "delay", "amount": 1, "unit": "days", "next": null}]}})).await;
    let o = h.ok("POST", "/api/objects", Some(reservation("R1", "ana@example.com", json!({"status": "confirmed", "nights": 2})))).await;
    h.drain().await;
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "fields": {"nights": 3}}))).await;
    let change: uuid::Uuid = sqlx::query_scalar("SELECT id FROM object_changes WHERE kind = 'updated'").fetch_one(&h.state.db).await.unwrap();
    // Another change job for the same object is entering the flow and hasn't committed yet.
    let flow: dumbmail::models::FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1::uuid").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    let def = dumbmail::flows::parse_definition(&flow.definition).unwrap();
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'ana@example.com'").fetch_one(&h.state.db).await.unwrap();
    let ctx = json!({"object": {"id": o["id"], "type": "reservation", "external_id": "R1"}});
    let mut tx = h.state.db.begin().await.unwrap();
    assert!(dumbmail::flows::enter(&mut tx, &flow, &def, pid, "d1", &ctx, None).await.unwrap().is_some());
    let state = h.state.clone();
    let job = tokio::spawn(async move { dumbmail::features::objects::flow::run_change(&state, change).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!job.is_finished(), "the second start waits for the first one's lock");
    tx.commit().await.unwrap();
    job.await.unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs").await, 1, "re-entry 'never' for this object: one run");
    h.cleanup().await;
}

async fn account_of(h: &Harness) -> uuid::Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// Make `path` POST a JSON webhook body to an integration with no signature.
async fn integration(h: &mut Harness, script: &str, redact: Value) -> (String, String) {
    let i = h.ok("POST", "/api/integrations", Some(json!({"name": "PMS", "script": script, "redact": redact}))).await;
    let url = i["inbound_url"].as_str().unwrap();
    (i["id"].as_str().unwrap().to_string(), url[url.find("/webhooks/in/").unwrap()..].to_string())
}

#[tokio::test]
async fn an_integration_never_inserts_a_profile_unheld() {
    let mut h = integ_harness!();
    h.signup().await;
    let acct = account_of(&h).await;
    // Every profile insert is recorded with its hold as inserted.
    for sql in [
        "CREATE TABLE test_inserts (email text, held boolean)",
        "CREATE FUNCTION test_record_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO test_inserts VALUES (NEW.email, NEW.marketing_hold); RETURN NEW; END $$",
        "CREATE TRIGGER test_record_insert AFTER INSERT ON profiles FOR EACH ROW EXECUTE FUNCTION test_record_insert()",
    ] {
        sqlx::query(sql).execute(&h.state.db).await.unwrap();
    }
    let (_, path) = integration(&mut h, "function handle(i) { return [{type: 'upsert_profile', email: 'guest@example.com', first_name: 'G'}] }", json!([])).await;
    let (mut points, go) = dumbmail::features::objects::hooks::install(acct);
    let (s, _, r) = h.raw("POST", &path, &[("content-type", "application/json")], "{}").await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let state = h.state.clone();
    let job = tokio::spawn(async move { dumbmail::jobs::drain_lane(&state, dumbmail::jobs::BACKGROUND).await.unwrap() });
    // Someone else creates the same person just before the integration's insert ...
    assert_eq!(points.recv().await, Some("before_create"));
    sqlx::query("INSERT INTO profiles (id, account_id, email) VALUES ($1, $2, 'guest@example.com')").bind(uuid::Uuid::now_v7()).bind(acct).execute(&h.state.db).await.unwrap();
    go.notify_one();
    // ... and deletes it just before the integration updates the match.
    assert_eq!(points.recv().await, Some("before_update"));
    sqlx::query("DELETE FROM profiles WHERE email = 'guest@example.com'").execute(&h.state.db).await.unwrap();
    go.notify_one();
    assert_eq!(points.recv().await, Some("before_create"));
    go.notify_one();
    assert_eq!(points.recv().await, Some("after_action"));
    go.notify_one();
    job.await.unwrap();
    dumbmail::features::objects::hooks::remove(acct);
    let inserts: Vec<(String, bool)> = sqlx::query_as("SELECT email, held FROM test_inserts").fetch_all(&h.state.db).await.unwrap();
    // The other writer's own insert, then the integration's: held from its first insert.
    assert_eq!(inserts, vec![("guest@example.com".to_string(), false), ("guest@example.com".to_string(), true)]);
    let (held, src): (bool, Option<String>) = sqlx::query_as("SELECT marketing_hold, email_source FROM profiles WHERE email = 'guest@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert!(held && src.is_some_and(|s| s.starts_with("integration:")));
    h.cleanup().await;
}

#[tokio::test]
async fn a_reassigned_booking_unlinks_the_old_guest_at_once() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_of(&h).await;
    for (email, key) in [("ana@example.com", "gA"), ("ben@example.com", "gB")] {
        sqlx::query("INSERT INTO profiles (id, account_id, email, integration_keys) VALUES ($1, $2, $3, jsonb_build_object('pms_guest', $4::text))")
            .bind(uuid::Uuid::now_v7())
            .bind(acct)
            .bind(email)
            .bind(key)
            .execute(&h.state.db)
            .await
            .unwrap();
    }
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "status", "type": "text"}, {"key": "nights", "type": "number"}]}))).await;
    live(&mut h, json!({"name": "Welcome", "trigger": {"type": "object", "object_type": "reservation", "relationship": "guest"}, "definition": {"entry": "d1", "nodes": [
        {"id": "d1", "type": "delay", "amount": 1, "unit": "days", "next": "e1"},
        {"id": "e1", "type": "email", "next": null, "content": {"subject": "Your stay {{ object.external_id }}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}]}})).await;
    let script = "function handle(i) { var p = i.payload; return [{type: 'upsert_object', object_type: 'reservation', external_id: 'R1', as_of: p.v, fields: {status: 'confirmed', nights: p.nights}, replace_links: ['guest'], profiles: p.guest ? [{property: 'pms_guest', value: p.guest, relationship: 'guest'}] : []}] }";
    let (_, path) = integration(&mut h, script, json!([])).await;
    let send = |body: Value| {
        let path = path.clone();
        let h = &h;
        async move {
            let (s, _, r) = h.raw("POST", &path, &[("content-type", "application/json")], &body.to_string()).await;
            assert_eq!(s, StatusCode::OK, "{r}");
            h.drain().await;
        }
    };
    send(json!({"guest": "gA", "v": 1, "nights": 2})).await;
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs WHERE status = 'active'").await, 1, "Ana is waiting in the flow");
    // The booking moves to a guest DumbMail doesn't know yet: Ana is unlinked now anyway.
    send(json!({"guest": "gNew", "v": 2, "nights": 3})).await;
    let links = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT p.email FROM object_links l JOIN profiles p ON p.id = l.profile_id ORDER BY 1").fetch_all(&db).await.unwrap() }
    };
    assert!(links(&h).await.is_empty(), "the old guest is unlinked at once");
    h.fast_forward().await;
    assert!(h.state.email.sent().iter().all(|m| m.to != "ana@example.com"), "Ana's run leaves before its email");
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs WHERE status = 'exited'").await, 1);
    // Once the new guest is known (here: Ben), they're linked.
    sqlx::query("UPDATE profiles SET integration_keys = '{\"pms_guest\": \"gNew\"}' WHERE email = 'ben@example.com'").execute(&h.state.db).await.unwrap();
    send(json!({"guest": "gNew", "v": 2, "nights": 3})).await;
    assert_eq!(links(&h).await, vec!["ben@example.com"], "the same version resent links the guest now known");
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM object_changes ORDER BY created_at, id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(kinds, vec!["created", "links", "updated", "links", "links"], "links and unlinks are in the history");
    // The API can replace a relationship's links too.
    h.ok("POST", "/api/objects", Some(json!({"type": "reservation", "external_id": "R1", "replace_links": ["guest"], "profiles": [{"email": "cara@example.com", "relationship": "guest"}]}))).await;
    assert_eq!(links(&h).await, vec!["cara@example.com"]);
    h.cleanup().await;
}

#[tokio::test]
async fn versions_order_writes_exactly_and_ignored_writes_create_nobody() {
    let mut h = harness!();
    h.signup().await;
    let up = |v: Value| {
        let mut b = json!({"type": "sub", "external_id": "S1", "fields": v.get("fields").cloned().unwrap_or(json!({})), "as_of": v["as_of"].clone()});
        if let Some(p) = v.get("profiles") {
            b["profiles"] = p.clone();
        }
        b
    };
    let big = "90071992547409930000";
    let r = h.ok("POST", "/api/objects", Some(up(json!({"fields": {"plan": "pro"}, "as_of": big, "profiles": [{"email": "a@example.com"}]})))).await;
    assert_eq!(r["result"], "created");
    // One less than a number a float can't tell apart from it: older, ignored, and its person isn't created.
    let r = h.ok("POST", "/api/objects", Some(up(json!({"fields": {"plan": "basic"}, "as_of": "90071992547409929999", "profiles": [{"email": "late@example.com"}]})))).await;
    assert_eq!(r["result"], "older");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles WHERE email = 'late@example.com'").await, 0, "an ignored write creates nobody");
    // The same version again is a duplicate.
    let r = h.ok("POST", "/api/objects", Some(up(json!({"fields": {"plan": "basic"}, "as_of": big})))).await;
    assert_eq!(r["result"], "duplicate");
    // No version: applies, but never lowers the stored one.
    let r = h.ok("POST", "/api/objects", Some(up(json!({"fields": {"seats": 3}})))).await;
    assert_eq!(r["result"], "updated");
    let (plan, as_of): (Option<String>, Option<String>) = sqlx::query_as("SELECT fields->>'plan', as_of FROM objects").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((plan.as_deref(), as_of.as_deref()), (Some("pro"), Some(big)));
    let r = h.ok("POST", "/api/objects", Some(up(json!({"fields": {"plan": "team"}, "as_of": "90071992547409930001"})))).await;
    assert_eq!(r["result"], "updated");
    // A type's versions are one kind: a time after numbers is refused.
    let (s, e) = h.req("POST", "/api/objects", Some(up(json!({"fields": {"plan": "x"}, "as_of": "2026-11-17T10:00:00Z"})))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(h.req("POST", "/api/objects", Some(up(json!({"as_of": -4})))).await.0, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn a_bulk_upsert_that_dies_resumes_after_its_last_write() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_of(&h).await;
    let items = json!([
        {"type": "sub", "external_id": "S1", "fields": {"plan": "a"}},
        {"type": "sub", "external_id": "S1", "fields": {"plan": "b"}},
        {"type": "sub", "external_id": "S2", "fields": {"plan": "c"}}]);
    let (s, b) = h.req("POST", "/api/objects/bulk", Some(json!({"objects": items}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let id: uuid::Uuid = b["id"].as_str().unwrap().parse().unwrap();
    // The job dies just as it reaches the third object (A and B are written).
    let (mut points, go) = dumbmail::features::objects::hooks::install(acct);
    let state = h.state.clone();
    let job = tokio::spawn(async move { dumbmail::features::objects::bulk::run(&state, id).await });
    for _ in 0..2 {
        assert_eq!(points.recv().await, Some("bulk_item"));
        go.notify_one();
    }
    assert_eq!(points.recv().await, Some("bulk_item"));
    job.abort();
    let _ = job.await;
    dumbmail::features::objects::hooks::remove(acct);
    // Run again: it carries on at the third object; A and B aren't written again (no B → A → B).
    dumbmail::features::objects::bulk::run(&h.state, id).await.unwrap();
    let plans: Vec<String> = sqlx::query_scalar("SELECT c.after->>'plan' FROM object_changes c JOIN objects o ON o.id = c.object_id WHERE o.external_id = 'S1' ORDER BY c.created_at, c.id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(plans, vec!["a", "b"]);
    let st = h.ok("GET", &format!("/api/objects/bulk/{id}"), None).await;
    assert_eq!((st["status"].clone(), st["done"].clone(), st["created"].clone(), st["updated"].clone()), (json!("done"), json!(3), json!(2), json!(1)), "{st}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_retried_delete_never_removes_a_replacement() {
    let mut h = integ_harness!();
    h.signup().await;
    let acct = account_of(&h).await;
    let script = "function handle(i) { var p = i.payload; return p.del ? [{type: 'delete_object', object_type: 'stay', external_id: 'S1'}] : [{type: 'upsert_object', object_type: 'stay', external_id: 'S1', fields: {n: p.n}}] }";
    let (_, path) = integration(&mut h, script, json!([])).await;
    h.raw("POST", &path, &[("content-type", "application/json")], &json!({"n": 1}).to_string()).await;
    h.drain().await;
    // The delete's delivery dies right after its action is written.
    h.raw("POST", &path, &[("content-type", "application/json")], &json!({"del": true}).to_string()).await;
    let d: uuid::Uuid = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE body LIKE '%del%'").fetch_one(&h.state.db).await.unwrap();
    let (mut points, go) = dumbmail::features::objects::hooks::install(acct);
    let state = h.state.clone();
    let job = tokio::spawn(async move { dumbmail::features::integrations::engine::process_delivery(&state, d).await });
    assert_eq!(points.recv().await, Some("before_delete"));
    go.notify_one();
    assert_eq!(points.recv().await, Some("after_action"));
    job.abort();
    let _ = job.await;
    dumbmail::features::objects::hooks::remove(acct);
    assert_eq!(count(&h, "SELECT count(*) FROM objects").await, 0);
    // A replacement arrives under the same id, then the delete's delivery is retried.
    sqlx::query("DELETE FROM jobs WHERE payload->>'delivery_id' = $1").bind(d.to_string()).execute(&h.state.db).await.unwrap();
    h.raw("POST", &path, &[("content-type", "application/json")], &json!({"n": 2}).to_string()).await;
    h.drain().await;
    dumbmail::features::integrations::engine::process_delivery(&h.state, d).await.unwrap();
    let n: Option<String> = sqlx::query_scalar("SELECT fields->>'n' FROM objects WHERE external_id = 'S1'").fetch_optional(&h.state.db).await.unwrap();
    assert_eq!(n.as_deref(), Some("2"), "the retry saw its delete already applied and left the replacement");
    h.cleanup().await;
}

#[tokio::test]
async fn a_date_fire_is_checked_again_at_the_moment_it_is_claimed() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_of(&h).await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "status", "type": "text"}, {"key": "check_in", "type": "date"}]}))).await;
    let trigger = json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_days": -2, "hour": 0, "relationship": "guest"});
    let fid = live(&mut h, email_flow("Pre-arrival", trigger.clone(), "See you {{ object.external_id }}")).await;
    // Three reservations due now; in the gap after each is claimed, something changes.
    for (id, who) in [("T", "t@example.com"), ("U", "u@example.com"), ("D", "d@example.com")] {
        h.ok("POST", "/api/objects", Some(reservation(id, who, json!({"status": "confirmed", "check_in": day(2)})))).await;
    }
    let stored: Value = sqlx::query_scalar("SELECT trigger FROM flows WHERE id = $1::uuid").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    let (mut points, go) = dumbmail::features::objects::hooks::install(acct);
    let state = h.state.clone();
    let tick = tokio::spawn(async move { dumbmail::flows::event_date_tick(&state, 0).await });
    for _ in 0..3 {
        assert_eq!(points.recv().await, Some("fire_claimed"));
        // Each fire is judged on its own change: the trigger is back to normal before the next.
        sqlx::query("UPDATE flows SET trigger = $2 WHERE id = $1::uuid").bind(&fid).bind(&stored).execute(&h.state.db).await.unwrap();
        // Which fire was claimed: the pending one that is locked now.
        let q = "SELECT o.external_id FROM flow_object_date_fires f JOIN objects o ON o.id = f.object_id WHERE f.outcome = 'pending'";
        let pending: Vec<String> = sqlx::query_scalar(q).fetch_all(&h.state.db).await.unwrap();
        let free: Vec<String> = sqlx::query_scalar(&format!("{q} FOR UPDATE OF f SKIP LOCKED")).fetch_all(&h.state.db).await.unwrap();
        let claimed = pending.into_iter().find(|e| !free.contains(e)).unwrap();
        match claimed.as_str() {
            // The flow's trigger changes (a fire made for the old one never starts).
            "T" => {
                let mut t = stored.clone();
                t["hour"] = json!(1);
                sqlx::query("UPDATE flows SET trigger = $2 WHERE id = $1::uuid").bind(&fid).bind(&t).execute(&h.state.db).await.unwrap();
            }
            // The guest is unlinked.
            "U" => {
                sqlx::query("DELETE FROM object_links l USING objects o WHERE o.id = l.object_id AND o.external_id = 'U'").execute(&h.state.db).await.unwrap();
            }
            // The date changes without the scheduler hearing about it.
            _ => {
                sqlx::query("UPDATE objects SET fields = jsonb_set(fields, '{check_in}', to_jsonb($1::text)) WHERE external_id = 'D'").bind(day(9)).execute(&h.state.db).await.unwrap();
            }
        }
        go.notify_one();
    }
    tick.await.unwrap().unwrap();
    dumbmail::features::objects::hooks::remove(acct);
    let outcomes: Vec<(String, String)> = sqlx::query_as("SELECT o.external_id, f.outcome FROM flow_object_date_fires f JOIN objects o ON o.id = f.object_id ORDER BY 1").fetch_all(&h.state.db).await.unwrap();
    let get = |e: &str| outcomes.iter().find(|(x, _)| x == e).map(|(_, o)| o.clone()).unwrap();
    assert_eq!(get("T"), "obsolete", "made for another trigger: {outcomes:?}");
    assert_eq!(get("U"), "not_started", "nobody has the guest relationship any more: {outcomes:?}");
    assert_eq!(get("D"), "obsolete", "its date moved: {outcomes:?}");
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs").await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn a_time_zone_change_never_leaves_a_fire_at_its_old_moment() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "reservation", "fields": [{"key": "check_in", "type": "date"}]}))).await;
    h.ok("POST", "/api/flows", Some(email_flow("Pre", json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_days": -2, "hour": 10}), "x"))).await;
    h.ok("POST", "/api/objects", Some(reservation("R1", "a@example.com", json!({"check_in": day(20)})))).await;
    let la: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT due_at FROM flow_object_date_fires").fetch_one(&h.state.db).await.unwrap();
    // The New York moment already has a fire (it ran): the Los Angeles one must not stay pending.
    let ny = la - chrono::Duration::hours(3);
    sqlx::query("INSERT INTO flow_object_date_fires (flow_id, object_id, account_id, due_at, trigger_key, outcome) SELECT flow_id, object_id, account_id, $1, trigger_key, 'started' FROM flow_object_date_fires")
        .bind(ny)
        .execute(&h.state.db)
        .await
        .unwrap();
    h.ok("PATCH", "/api/account", Some(json!({"timezone": "America/New_York"}))).await;
    let rows: Vec<(chrono::DateTime<chrono::Utc>, String)> = sqlx::query_as("SELECT due_at, outcome FROM flow_object_date_fires ORDER BY due_at").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(rows, vec![(ny, "started".to_string()), (la, "obsolete".to_string())]);
    h.cleanup().await;
}

#[tokio::test]
async fn guesty_pre_arrival_waits_until_the_booking_is_paid_in_full() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    let created = h.ok("POST", "/api/integrations/from-recipe", Some(json!({"recipe": "guesty"}))).await;
    let id = created["integration"]["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/integrations/{id}"), Some(json!({"signature": {"type": "none"}}))).await;
    let i = h.ok("GET", &format!("/api/integrations/{id}"), None).await;
    let url = i["inbound_url"].as_str().unwrap();
    let path = url[url.find("/webhooks/in/").unwrap()..].to_string();
    // Both suggested pre-arrival flows (event-based and object-based), firing at midnight.
    for f in created["flows"].as_array().unwrap().iter().filter(|f| f["name"].as_str().unwrap().contains("pre-arrival")) {
        let fid = f["id"].as_str().unwrap();
        let mut trigger = h.ok("GET", &format!("/api/flows/{fid}"), None).await["trigger"].clone();
        trigger["hour"] = json!(0);
        h.ok("PATCH", &format!("/api/flows/{fid}"), Some(json!({"trigger": trigger}))).await;
        h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    }
    let legacy = |ev: &str, due: f64, at: &str, msg: &str| {
        let r = json!({"_id": "R1", "status": "confirmed", "guestId": "gA", "lastUpdatedAt": at, "confirmedAt": "2026-11-17T09:00:00.000Z",
            "checkInDateLocalized": day(2), "checkOutDateLocalized": day(4), "listing": {"nickname": "Dune Cottage"},
            "guest": {"_id": "gA", "email": "ana@example.com", "firstName": "Ana"},
            "money": {"totalPaid": 600.0 - due, "balanceDue": due, "currency": "USD"}});
        (json!({"event": ev, "reservation": r}).to_string(), msg.to_string())
    };
    let post = |(body, msg): (String, String)| {
        let path = path.clone();
        let h = &h;
        async move {
            let (s, _, r) = h.raw("POST", &path, &[("content-type", "application/json"), ("svix-id", &msg)], &body).await;
            assert_eq!(s, StatusCode::OK, "{r}");
            h.drain().await;
        }
    };
    let sent = |h: &Harness| h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("See you soon")).map(|m| m.to).collect::<Vec<_>>();
    // Confirmed, money still due: nothing.
    post(legacy("reservation.new", 120.0, "2026-11-17T10:00:00.000Z", "m1")).await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert!(sent(&h).is_empty(), "confirmed but not paid: no pre-arrival");
    let paid: Option<bool> = sqlx::query_scalar("SELECT (fields->>'paid_in_full')::boolean FROM objects WHERE external_id = 'R1'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(paid, Some(false));
    // Paid in full later: each pre-arrival flow sends once.
    post(legacy("reservation.updated", 0.0, "2026-11-17T11:00:00.000Z", "m2")).await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h), vec!["ana@example.com", "ana@example.com"], "the event-based and the object-based flow");
    // A booking whose payment Guesty never reported (v2 webhooks carry no money): never.
    sqlx::query("INSERT INTO profiles (id, account_id, email, integration_keys) SELECT $1, account_id, 'ben@example.com', '{\"guesty_guest_id\": \"gB\"}' FROM integrations")
        .bind(uuid::Uuid::now_v7())
        .execute(&h.state.db)
        .await
        .unwrap();
    let v2 = json!({"event": "reservation.created.v2", "meta": {"eventId": "m3", "publishedAt": "2026-11-17T12:00:00.000Z"},
        "data": {"reservationId": "R2", "guestId": "gB", "status": "confirmed", "checkInDateLocalized": day(2), "checkOutDateLocalized": day(4), "guestsCount": 2}});
    post((v2.to_string(), "m3".into())).await;
    dumbmail::flows::date_triggers(&h.state).await.unwrap();
    h.fast_forward().await;
    assert_eq!(sent(&h).len(), 2, "unknown payment never sends");
    assert_eq!(count(&h, "SELECT count(*) FROM objects WHERE external_id = 'R2' AND fields ? 'paid_in_full'").await, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn sensitive_fields_never_reach_messages_or_dry_runs() {
    let mut h = integ_harness!();
    h.signup().await;
    h.credit(5.0).await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "stay", "fields": [{"key": "check_in", "type": "date"}, {"key": "pin", "type": "text", "sensitive": true}]}))).await;
    live(&mut h, json!({"name": "Codes", "trigger": {"type": "object", "object_type": "stay", "on": "created"}, "definition": {"entry": "e1", "nodes": [
        {"id": "e1", "type": "email", "next": null, "content": {"subject": "Code [{{ object.pin }}] for {{ object.check_in }}", "blocks": [{"type": "text", "html": "<p>Your code: {{ object.pin }}</p>"}]}}]}})).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S1", "fields": {"check_in": day(3), "pin": "4417"}, "profiles": [{"email": "a@example.com"}]}))).await;
    h.fast_forward().await;
    let m = h.state.email.sent().into_iter().find(|m| m.subject.starts_with("Code")).expect("the flow sent");
    assert_eq!(m.subject, format!("Code [] for {}", day(3)));
    assert!(!m.html.contains("4417") && !m.text.contains("4417"));
    let stored: String = sqlx::query_scalar("SELECT string_agg(concat_ws(' ', subject, to_address), ' ') FROM messages").fetch_one(&h.state.db).await.unwrap();
    assert!(!stored.contains("4417"));
    // An integration's test run hides the private value and never says whether it would change.
    let script = "function handle(i) { return [{type: 'upsert_object', object_type: 'stay', external_id: 'S1', fields: {pin: i.payload.pin, check_in: i.payload.d}}] }";
    let (iid, _) = integration(&mut h, script, json!([])).await;
    for pin in ["4417", "9999"] {
        let t = h.ok("POST", &format!("/api/integrations/{iid}/test"), Some(json!({"payload": {"pin": pin, "d": day(3)}}))).await;
        let shown = t.to_string();
        assert!(!shown.contains("4417") && !shown.contains("9999"), "{shown}");
        assert!(shown.contains("unchanged") && shown.contains("pin (private)"), "the same answer whatever the private value: {shown}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn sensitivity_survives_edits_and_rules_fail_closed_at_run_time() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "stay", "fields": [
        {"key": "status", "type": "text"}, {"key": "check_in", "type": "date"},
        {"key": "pin", "type": "text", "sensitive": true}, {"key": "lock", "type": "enum", "options": ["front", "back"], "sensitive": true}]}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S1", "fields": {"status": "confirmed", "check_in": day(5), "pin": "4417", "lock": "front"}, "profiles": [{"email": "a@example.com"}]}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S1", "fields": {"pin": "5512"}}))).await;
    // A sensitive enum's options are never listed, in the type or in an error.
    let t = h.ok("GET", "/api/object-types/stay", None).await;
    assert!(!t.to_string().contains("front"), "{t}");
    let (s, e) = h.req("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S2", "fields": {"lock": "side"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!e.to_string().contains("front"), "{e}");
    // Saving the type without the hidden options keeps them.
    let mut fields: Vec<Value> = t["fields"].as_array().unwrap().clone();
    h.ok("PATCH", "/api/object-types/stay", Some(json!({"fields": fields.clone()}))).await;
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S3", "fields": {"lock": "back"}}))).await;
    // Rules saved while status was ordinary; then status becomes sensitive.
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "No confirmed stay", "definition": {"conditions": [{"type": "object", "object_type": "stay", "op": "zero", "where": [{"field": "status", "op": "eq", "value": "confirmed"}]}]}}))).await;
    let seg_id: uuid::Uuid = seg["id"].as_str().unwrap().parse().unwrap();
    live(&mut h, json!({"name": "Not cancelled", "trigger": {"type": "object", "object_type": "stay", "on": "updated", "filters": [{"field": "status", "op": "neq", "value": "cancelled"}]},
        "definition": {"entry": "d1", "nodes": [{"id": "d1", "type": "delay", "amount": 1, "unit": "days", "next": null}]}})).await;
    let date_flow = h.ok("POST", "/api/flows", Some(email_flow("Dated", json!({"type": "object_date", "object_type": "stay", "field": "check_in", "offset_days": -1}), "x"))).await;
    for f in fields.iter_mut() {
        if f["key"] == "status" || f["key"] == "check_in" {
            f["sensitive"] = json!(true);
        }
    }
    h.ok("PATCH", "/api/object-types/stay", Some(json!({"fields": fields.clone()}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "nobody@example.com"}))).await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    assert_eq!(count(&h, &format!("SELECT count(*) FROM segment_members WHERE segment_id = '{seg_id}'")).await, 0, "a zero-count rule on a sensitive field matches nobody");
    assert_eq!(h.req("POST", "/api/segments/preview", Some(json!({"definition": seg["definition"].clone()}))).await.0, StatusCode::BAD_REQUEST, "saving or previewing says why");
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S1", "fields": {"pin": "7777"}}))).await;
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs").await, 0, "a `neq` filter on a sensitive field starts nothing");
    let report = h.ok("GET", &format!("/api/flows/{}/report", date_flow["id"].as_str().unwrap()), None).await;
    assert_eq!(report["object_dates"]["hidden"], true, "{report}");
    // Taking pin off the type: its values and history go, and a later pin is sensitive again.
    fields.retain(|f| f["key"] != "pin");
    h.ok("PATCH", "/api/object-types/stay", Some(json!({"fields": fields}))).await;
    h.drain().await;
    let left: String = sqlx::query_scalar("SELECT concat_ws(' ', (SELECT string_agg(fields::text, ' ') FROM objects), (SELECT string_agg(concat_ws(' ', before::text, after::text, changed::text), ' ') FROM object_changes))").fetch_one(&h.state.db).await.unwrap();
    for v in ["4417", "5512", "7777", "\"pin\""] {
        assert!(!left.contains(v), "{v} survived: {left}");
    }
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "S1", "fields": {"pin": "1234"}}))).await;
    let t = h.ok("GET", "/api/object-types/stay", None).await;
    assert_eq!(t["fields"].as_array().unwrap().iter().find(|f| f["key"] == "pin").unwrap()["sensitive"], true);
    assert!(!h.ok("GET", "/api/object-types/stay/objects", None).await.to_string().contains("1234"));
    h.cleanup().await;
}

#[tokio::test]
async fn the_scheduler_and_retention_stay_bounded() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_of(&h).await;
    h.ok("POST", "/api/object-types", Some(json!({"key": "stay", "fields": [{"key": "check_in", "type": "date"}]}))).await;
    // Objects and events that existed before their date flows went live.
    for i in 0..6 {
        h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": format!("S{i}"), "fields": {"check_in": day(30)}}))).await;
        h.ok("POST", "/api/events", Some(json!({"metric": "Booked", "profile": {"email": format!("e{i}@example.com")}, "properties": {"on": day(30)}}))).await;
    }
    h.drain().await;
    live(&mut h, email_flow("Obj", json!({"type": "object_date", "object_type": "stay", "field": "check_in", "offset_days": -1}), "x")).await;
    live(&mut h, json!({"name": "Ev", "trigger": {"type": "event_date", "metric": "Booked", "field": "on", "offset_days": -1},
        "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "next": null, "content": {"subject": "x", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}]}})).await;
    // A tight budget is shared: object dates get their part even when there are more events than budget.
    dumbmail::flows::event_date_tick(&h.state, 4).await.unwrap();
    assert!(count(&h, "SELECT count(*) FROM flow_object_date_fires").await >= 2, "object dates got their share of the budget");
    assert!(count(&h, "SELECT count(*) FROM flow_event_date_fires").await >= 2);
    // The claim reads each live flow's fires by (flow, due): a paused flow's fires are never scanned.
    let mut conn = h.state.db.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off").execute(&mut *conn).await.unwrap();
    let plan: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {}", dumbmail::features::objects::flow::CLAIM_SQL.replace("$1", "24"))).fetch_all(&mut *conn).await.unwrap();
    let plan = plan.join("\n");
    assert!(plan.contains("flow_object_date_fires_flow_due_idx"), "{plan}");
    drop(conn);
    // History trimming walks objects with a cursor, a bounded number per run.
    let ids: Vec<uuid::Uuid> = (0..dumbmail::features::objects::store::TRIM_OBJECTS + 2).map(|_| uuid::Uuid::now_v7()).collect();
    let tid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM object_types").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO objects (id, account_id, type_id, external_id) SELECT x, $2, $3, x::text FROM unnest($1::uuid[]) x").bind(&ids).bind(acct).bind(tid).execute(&h.state.db).await.unwrap();
    let last = *ids.last().unwrap();
    sqlx::query("INSERT INTO object_changes (id, object_id, account_id, kind, version, created_at) SELECT gen_random_uuid(), $1, $2, 'updated', g, now() - make_interval(hours => g + 1) FROM generate_series(1, 105) g")
        .bind(last)
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    let (_, next) = dumbmail::features::objects::store::trim_history(&h.state.db, 5000, None).await.unwrap();
    assert!(next.is_some(), "more objects than one run looks at: it hands over a cursor");
    assert_eq!(count(&h, &format!("SELECT count(*) FROM object_changes WHERE object_id = '{last}'")).await, 105, "the last object is beyond the first run");
    let (_, next) = dumbmail::features::objects::store::trim_history(&h.state.db, 5000, next).await.unwrap();
    assert_eq!(next, None);
    assert_eq!(count(&h, &format!("SELECT count(*) FROM object_changes WHERE object_id = '{last}'")).await, 100, "the newest 100 are kept");
    h.cleanup().await;
}

#[tokio::test]
async fn single_writes_are_audited_and_type_edits_keep_stored_values_valid() {
    let mut h = harness!();
    h.signup().await;
    // An empty object is in its history from the start.
    let r = h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "E1"}))).await;
    let o = h.ok("GET", &format!("/api/objects/{}", r["id"].as_str().unwrap()), None).await;
    assert_eq!(o["history"][0]["kind"], "created", "{o}");
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent", "scopes": ["full"]}))).await["key"].as_str().unwrap().to_string();
    let (err, _, text) = mcp(&h, &key, "upsert_objects", json!({"objects": [{"type": "stay", "external_id": "E2", "fields": {"nights": "3"}}]})).await;
    assert!(!err, "{text}");
    h.ok("DELETE", &format!("/api/objects/{}", r["id"].as_str().unwrap()), None).await;
    let audits: Vec<(String, Value)> = sqlx::query_as("SELECT action, details FROM audit_log WHERE action LIKE 'objects.%' ORDER BY created_at").fetch_all(&h.state.db).await.unwrap();
    let actions: Vec<&str> = audits.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["objects.upsert", "objects.upsert", "objects.delete"], "{audits:?}");
    assert!(!format!("{audits:?}").contains("nights"), "no fields in the audit: {audits:?}");
    // nights is text now ("3"); making it a number changes what's stored: refused with a count ...
    h.ok("POST", "/api/objects", Some(json!({"type": "stay", "external_id": "E3", "fields": {"nights": "several"}}))).await;
    let mut fields: Vec<Value> = h.ok("GET", "/api/object-types/stay", None).await["fields"].as_array().unwrap().clone();
    for f in fields.iter_mut() {
        if f["key"] == "nights" {
            f["type"] = json!("number");
        }
    }
    let (s, e) = h.req("PATCH", "/api/object-types/stay", Some(json!({"fields": fields.clone()}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["error"]["message"].as_str().unwrap().starts_with("2 stays have a value in nights"), "{e}");
    assert_eq!(count(&h, "SELECT count(*) FROM object_types WHERE fields @> '[{\"key\": \"nights\", \"type\": \"text\"}]'").await, 1, "nothing changed");
    // ... and with convert, values that fit are converted and the rest removed.
    h.ok("PATCH", "/api/object-types/stay", Some(json!({"fields": fields, "convert": true}))).await;
    let nights: Vec<(String, Option<Value>)> = sqlx::query_as("SELECT external_id, fields->'nights' FROM objects ORDER BY external_id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(nights, vec![("E2".to_string(), Some(json!(3))), ("E3".to_string(), None)]);
    h.cleanup().await;
}

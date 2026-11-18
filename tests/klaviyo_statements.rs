//! An account that never imported from Klaviyo pays nothing for the importer: the send, schedule,
//! test-send, go-live, live-edit, flow-send and segment-refresh paths run exactly as many SQL statements
//! as on `main` before the importer. The counts below were measured by running this same file against
//! `origin/main` (35d7abc); a change here means one of those paths picked up a query.
//!
//! Statements are counted from sqlx's own query log (`sqlx::query` tracing events), one per statement
//! executed, on this test's thread only.

#[macro_use]
mod common;

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;
use uuid::Uuid;

#[derive(Clone, Default)]
struct Counter {
    n: Arc<AtomicUsize>,
    sql: Arc<Mutex<Vec<String>>>,
}

struct Field<'a>(&'a mut String);
impl tracing::field::Visit for Field<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "summary" || field.name() == "db.statement" {
            self.0.push_str(&format!("{value:?} "));
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "summary" || field.name() == "db.statement" {
            self.0.push_str(value);
            self.0.push(' ');
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Counter {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.n.fetch_add(1, Ordering::SeqCst);
            let mut s = String::new();
            event.record(&mut Field(&mut s));
            self.sql.lock().unwrap().push(s.chars().take(160).collect());
        }
    }
}

/// Run `f`, counting the SQL statements it executes.
async fn counted<T>(f: impl Future<Output = T>) -> (T, usize, Vec<String>) {
    let c = Counter::default();
    let sub = tracing_subscriber::registry().with(c.clone());
    let guard = tracing::subscriber::set_default(sub);
    let out = f.await;
    drop(guard);
    let n = c.n.load(Ordering::SeqCst);
    let sql = c.sql.lock().unwrap().clone();
    (out, n, sql)
}

fn show(label: &str, n: usize, sql: &[String]) -> String {
    format!("{label}: {n} statements\n  {}", sql.join("\n  "))
}

#[tokio::test]
async fn an_account_that_never_imported_runs_the_same_sql_as_before() {
    let mut h = harness!();
    h.signup().await;
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE accounts SET address = '1 Main St, Portland OR', from_email = 'hi@shop.test', from_name = 'Shop' WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await["id"].as_str().unwrap().to_string();
    for i in 0..3 {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("p{i}@shop.test"), "email_consent": "subscribed", "list_ids": [list]}))).await;
    }
    let campaign = |name: &str| json!({"name": name, "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": {"subject": name, "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}});
    let mut report = Vec::new();

    // Campaign start (the send job), on a campaign already moved to sending.
    let c1: Uuid = h.ok("POST", "/api/campaigns", Some(campaign("Start"))).await["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(c1).execute(&h.state.db).await.unwrap();
    let (r, n, sql) = counted(dumbmail::campaigns::prepare_send(&h.state, c1)).await;
    r.unwrap();
    report.push(("campaign start", n, sql));

    // Schedule, and send now.
    let c2: Uuid = h.ok("POST", "/api/campaigns", Some(campaign("Later"))).await["id"].as_str().unwrap().parse().unwrap();
    let at = chrono::Utc::now() + chrono::Duration::days(2);
    let (r, n, sql) = counted(dumbmail::api::messaging::launch(&h.state, acct, c2, Some(at), None)).await;
    r.unwrap();
    report.push(("schedule", n, sql));
    let c3: Uuid = h.ok("POST", "/api/campaigns", Some(campaign("Now"))).await["id"].as_str().unwrap().parse().unwrap();
    let (r, n, sql) = counted(dumbmail::api::messaging::launch(&h.state, acct, c3, None, None)).await;
    r.unwrap();
    report.push(("send now", n, sql));

    // Test send.
    let c4: Uuid = h.ok("POST", "/api/campaigns", Some(campaign("Test"))).await["id"].as_str().unwrap().parse().unwrap();
    let row = dumbmail::campaigns::load(&h.state.db, acct, c4).await.unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let (r, n, sql) = counted(dumbmail::campaigns::send_test(&h.state, &account, &row, &["p0@shop.test".to_string()], None, true)).await;
    r.unwrap();
    report.push(("test send", n, sql));

    // A flow: go live, edit while live, and send its email.
    let def = json!({"entry": "e", "nodes": [{"id": "e", "type": "email", "content": {"subject": "Thanks", "blocks": [{"type": "text", "html": "<p>Thanks</p>"}]}, "next": null}]});
    let fid: Uuid = h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": def}))).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (r, n, sql) = counted(dumbmail::api::messaging::set_flow_status(&h.state, acct, fid, "live")).await;
    r.unwrap();
    report.push(("flow go-live", n, sql));
    let body: dumbmail::api::messaging::FlowBody = serde_json::from_value(json!({"definition": def})).unwrap();
    let (r, n, sql) = counted(dumbmail::api::messaging::update_flow(&h.state, acct, fid, body)).await;
    r.unwrap();
    report.push(("live flow edit", n, sql));
    let flow: dumbmail::models::FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(fid).fetch_one(&h.state.db).await.unwrap();
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'p1@shop.test'").fetch_one(&h.state.db).await.unwrap();
    let run = dumbmail::flows::start(&h.state, &flow, pid, json!({})).await.unwrap().unwrap();
    let (r, n, sql) = counted(dumbmail::flows::step(&h.state, run)).await;
    r.unwrap();
    report.push(("flow email send", n, sql));

    // Segment refresh (a second one, so it's past the silent first refresh).
    let seg: Uuid = h.ok("POST", "/api/segments", Some(json!({"name": "All", "definition": {"conditions": [{"type": "property", "field": "vip", "op": "eq", "value": true}]}}))).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    dumbmail::flows::refresh_segment(&h.state, seg).await.unwrap();
    let (r, n, sql) = counted(dumbmail::flows::refresh_segment(&h.state, seg)).await;
    r.unwrap();
    report.push(("segment refresh", n, sql));

    let got: Vec<(&str, usize)> = report.iter().map(|(l, n, _)| (*l, *n)).collect();
    let detail: Vec<String> = report.iter().map(|(l, n, s)| show(l, *n, s)).collect();
    println!("{}", detail.join("\n"));
    // Measured on origin/main (ea51090) with this same test: the import adds no statement to these
    // paths. If a later change to main adds one on purpose, run this file on main and update the
    // numbers; an accidental extra query on a send path is what this is here to catch.
    let expected: Vec<(&str, usize)> = EXPECTED.to_vec();
    assert_eq!(got, expected, "{}", detail.join("\n"));
    h.cleanup().await;
}

const EXPECTED: &[(&str, usize)] = &[
    ("campaign start", 13),
    ("schedule", 14),
    ("send now", 15),
    ("test send", 12),
    ("flow go-live", 5),
    ("live flow edit", 2),
    ("flow email send", 26),
    ("segment refresh", 7),
];

//! An account with no webhook endpoint and no data export pays nothing for them: a campaign send
//! and a segment refresh run exactly the SQL they would run with webhooks and exports switched off
//! (the same statements as on main, apart from the transaction around each recorded event).
//! Its own test binary: it counts every SQL statement the process runs.

#[macro_use]
mod common;
use common::*;

use std::sync::{Arc, LazyLock, Mutex};

use serde_json::json;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

#[derive(Clone, Default)]
struct Statements(Arc<Mutex<Option<Vec<String>>>>);

struct Summary(String);

impl Visit for Summary {
    fn record_str(&mut self, f: &Field, v: &str) {
        // `db.statement` is the full text when sqlx gives it; short statements only have `summary`.
        if (f.name() == "db.statement" && !v.is_empty()) || (f.name() == "summary" && self.0.is_empty()) {
            self.0 = v.to_string();
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> Layer<S> for Statements {
    fn on_event(&self, e: &tracing::Event<'_>, _: Context<'_, S>) {
        if e.metadata().target() != "sqlx::query" {
            return;
        }
        let mut s = Summary(String::new());
        e.record(&mut s);
        if let Some(v) = self.0.lock().unwrap().as_mut() {
            v.push(s.0);
        }
    }
}

static SEEN: LazyLock<Statements> = LazyLock::new(|| {
    let s = Statements::default();
    let sub = tracing_subscriber::registry().with(s.clone().with_filter(tracing_subscriber::filter::LevelFilter::TRACE));
    tracing::subscriber::set_global_default(sub).unwrap();
    s
});

fn start() {
    *SEEN.0.lock().unwrap() = Some(Vec::new());
}

/// The statements run since [`start`], leaving out transaction control (BEGIN, COMMIT ...) and the
/// test's own polling of the job queue (`jobs::drain` claims from each lane until both are empty).
fn stop() -> Vec<String> {
    let all = SEEN.0.lock().unwrap().take().unwrap_or_default();
    all.into_iter()
        .filter(|s| {
            let s = s.trim_start().to_ascii_uppercase();
            !["BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT", "RELEASE", "UPDATE JOBS SET LOCKED_AT"].iter().any(|k| s.starts_with(k))
        })
        .collect()
}

/// A campaign to 3 people, then a segment refresh that someone enters. Returns the statements of each.
async fn measure(h: &mut Harness) -> (Vec<String>, Vec<String>) {
    h.signup().await;
    h.credit(10.0).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    for i in 0..3 {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("p{i}@x.test"), "email_consent": "subscribed", "list_ids": [list["id"]]}))).await;
    }
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Launch", "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hello</p>"}]}}))).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "VIPs", "definition": {"match": "all", "conditions": [{"type": "property", "field": "vip", "op": "eq", "value": true}]}}))).await;
    let seg_id: uuid::Uuid = seg["id"].as_str().unwrap().parse().unwrap();
    h.drain().await;
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    h.ok("POST", "/api/profiles", Some(json!({"email": "vip@x.test", "properties": {"vip": true}}))).await;
    h.drain().await;

    start();
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let send = stop();
    start();
    dumbmail::flows::refresh_segment(&h.state, seg_id).await.unwrap();
    let refresh = stop();
    (send, refresh)
}

#[tokio::test]
async fn an_account_with_nothing_set_up_runs_no_extra_sql() {
    LazyLock::force(&SEEN);
    dumbmail::features::observe::set_ttl_for_tests(std::time::Duration::from_secs(3600));

    // Webhooks and exports present, nothing set up (the snapshot is loaded and says so).
    let mut idle = harness!();
    dumbmail::features::observe::refresh(&idle.state.db).await.unwrap();
    let (send, refresh) = measure(&mut idle).await;
    idle.cleanup().await;

    // The same with every observer switched off.
    let Some(mut off) = Harness::new().await else { return };
    dumbmail::features::observe::set_disabled_for_tests(true);
    let (send_off, refresh_off) = measure(&mut off).await;
    dumbmail::features::observe::set_disabled_for_tests(false);
    off.cleanup().await;

    println!("campaign send: {} statements; segment refresh: {} statements", send.len(), refresh.len());
    if let Ok(p) = std::env::var("DUMP") {
        std::fs::write(p, format!("{send:#?}\n----\n{refresh:#?}")).unwrap();
    }
    assert!(send.len() > 10 && !refresh.is_empty(), "the counter sees statements: {send:?}");
    assert_eq!(send.len(), send_off.len(), "campaign send\nidle: {send:#?}\noff: {send_off:#?}");
    assert_eq!(refresh.len(), refresh_off.len(), "segment refresh\nidle: {refresh:#?}\noff: {refresh_off:#?}");
    for s in send.iter().chain(&refresh) {
        assert!(!s.contains("webhook_") && !s.contains("warehouse_"), "observer SQL ran: {s}");
    }
}

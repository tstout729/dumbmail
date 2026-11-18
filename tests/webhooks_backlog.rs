//! Webhook backlogs are bounded. In its own test binary because it lowers process-wide limits.

#[macro_use]
mod common;
use common::*;

use serde_json::json;

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn a_dead_endpoint_keeps_only_so_many_undelivered_events_and_the_owner_is_told_once() {
    let mut h = harness!();
    dumbmail::features::webhooks::net::allow_localhost_for_tests();
    dumbmail::features::webhooks::set_limits_for_tests(5, 1000);
    // Nothing listens here: every delivery fails to connect.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", dead.local_addr().unwrap());
    drop(dead);
    h.signup().await;
    let ep = h.ok("POST", "/api/webhooks", Some(json!({"url": url, "topics": ["profile.created"], "description": "CRM"}))).await;
    let id = ep["id"].as_str().unwrap().to_string();
    for i in 0..8 {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("b{i}@x.test")}))).await;
    }
    h.drain().await;

    // 8 queued, the limit is 5: the 3 oldest were dropped, and the owner heard about it once.
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_deliveries WHERE status = 'pending'").await, 5);
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_deliveries WHERE status = 'dropped'").await, 3);
    let e = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    assert_eq!((e["stats"]["pending"].as_i64(), e["stats"]["dropped_total"].as_i64()), (Some(5), Some(3)), "{e}");
    assert!(e["stats"]["last_dropped_at"].is_string());
    let dropped = h.ok("GET", &format!("/api/webhooks/{id}/deliveries?status=dropped"), None).await;
    assert_eq!(dropped["data"][0]["error"], "backlog_full");
    for i in 8..11 {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("b{i}@x.test")}))).await;
    }
    h.drain().await;
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_deliveries WHERE status = 'pending'").await, 5);
    let alerts: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.subject == "A webhook endpoint is falling behind").collect();
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].text.contains("\"CRM\" (127.0.0.1)") && !alerts[0].text.contains("/hook"), "{}", alerts[0].text);

    // An account's outbox is bounded too, where rows are written: against the sweep's count, a
    // batch past the limit keeps what fits and the rest is counted as dropped.
    dumbmail::features::webhooks::set_limits_for_tests(5, 3);
    h.ok("PATCH", &format!("/api/webhooks/{id}"), Some(json!({"topics": ["profile.created", "list.member_added"]}))).await;
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    h.drain().await;
    dumbmail::features::observe::refresh(&h.state.db).await.unwrap();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await;
    let ids: Vec<String> = sqlx::query_scalar("SELECT id::text FROM profiles LIMIT 6").fetch_all(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": ids}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox WHERE topic = 'list.member_added'").await, 3);
    // A second batch finds no room left until the sweep counts again.
    let more: Vec<String> = sqlx::query_scalar("SELECT id::text FROM profiles ORDER BY id DESC LIMIT 2").fetch_all(&h.state.db).await.unwrap();
    let list2 = h.ok("POST", "/api/lists", Some(json!({"name": "L2"}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list2["id"].as_str().unwrap()), Some(json!({"profile_ids": more}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox WHERE topic = 'list.member_added'").await, 3);
    // Drops are counted on the endpoint a moment later, off the caller's path.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let e = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    // 3 + 3 from the endpoint's own backlog above, then 3 of the 6 and both of the 2 here.
    assert_eq!(e["stats"]["dropped_total"].as_i64(), Some(3 + 3 + 3 + 2), "{e}");
    // The sweep trims an outbox that got past the limit anyway (here: rows written while the limit
    // was higher), oldest first.
    h.drain().await;
    let account: uuid::Uuid = sqlx::query_scalar("SELECT account_id FROM webhook_endpoints").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO webhook_outbox (account_id, event_id, topic, data, created_at) SELECT $1, gen_random_uuid(), 'profile.created', '{}', now() - interval '2 minutes' FROM generate_series(1, 10)")
        .bind(account)
        .execute(&h.state.db)
        .await
        .unwrap();
    dumbmail::features::webhooks::set_limits_for_tests(5, 4);
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    assert!(dumbmail::jobs::run_one_in(&h.state, dumbmail::jobs::BACKGROUND).await.unwrap());
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox").await, 4);
    assert_eq!(count(&h, &format!("SELECT rows FROM webhook_outbox_counts WHERE account_id = '{account}'")).await, 4);
    dumbmail::features::webhooks::set_limits_for_tests(100_000, 200_000);
    h.cleanup().await;
}

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

    // An account's outbox is bounded too: a batch past the limit keeps what fits.
    dumbmail::features::webhooks::set_limits_for_tests(5, 3);
    h.ok("PATCH", &format!("/api/webhooks/{id}"), Some(json!({"topics": ["profile.created", "list.member_added"]}))).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "L"}))).await;
    let ids: Vec<String> = sqlx::query_scalar("SELECT id::text FROM profiles LIMIT 6").fetch_all(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": ids}))).await;
    assert_eq!(count(&h, "SELECT count(*) FROM webhook_outbox WHERE topic = 'list.member_added'").await, 3);
    let e = h.ok("GET", &format!("/api/webhooks/{id}"), None).await;
    assert!(e["stats"]["dropped_total"].as_i64().unwrap() >= 6, "{e}");
    dumbmail::features::webhooks::set_limits_for_tests(100_000, 200_000);
    h.cleanup().await;
}

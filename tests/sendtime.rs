//! Send-time options end to end: local-time releases by zone, cancel and restart safety, A/B tests,
//! smart sending, gradual sends, SMS quiet hours in the recipient's zone, validation and the API.

#[macro_use]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use dumbmail::features::sendtime::clock;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::*;

async fn person(h: &mut Harness, list: &str, email: &str, extra: Value) -> String {
    let mut body = json!({"email": email, "email_consent": "subscribed", "list_ids": [list]});
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    h.ok("POST", "/api/profiles", Some(body)).await["id"].as_str().unwrap().to_string()
}

async fn list(h: &mut Harness, name: &str) -> String {
    h.ok("POST", "/api/lists", Some(json!({"name": name}))).await["id"].as_str().unwrap().to_string()
}

fn campaign(list: &str, subject: &str, send_options: Value) -> Value {
    json!({"name": subject, "channel": "email", "audience": {"include": [{"type": "list", "id": list}]},
        "content": {"subject": subject, "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}, "send_options": send_options})
}

fn received(h: &Harness, to: &str, subject: &str) -> usize {
    h.state.email.sent().iter().filter(|m| m.to == to && m.subject == subject).count()
}

/// (to_address, send_after, send_tz, status, error) for a campaign's messages.
async fn rows(h: &Harness, cid: &str) -> Vec<(String, Option<DateTime<Utc>>, Option<String>, String, Option<String>)> {
    sqlx::query_as("SELECT to_address, send_after, send_tz, status, error FROM messages WHERE campaign_id = $1::uuid AND NOT is_test ORDER BY to_address")
        .bind(cid)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

/// Make one zone's release time come now, and run every due batch (batches that run early re-queue
/// the messages that aren't due yet).
async fn release(h: &Harness, cid: &str, zone: Option<&str>) {
    sqlx::query("UPDATE messages SET send_after = now() - interval '1 second' WHERE campaign_id = $1::uuid AND status = 'queued' AND ($2::text IS NULL OR send_tz = $2)")
        .bind(cid)
        .bind(zone)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'campaign_batch' AND run_at <> 'infinity'").execute(&h.state.db).await.unwrap();
    h.drain().await;
}

#[tokio::test]
async fn local_time_send_releases_each_zone_once_and_stops_on_cancel() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "World").await;
    person(&mut h, &l, "kolkata@x.test", json!({"properties": {"timezone": "Asia/Kolkata"}})).await;
    person(&mut h, &l, "kathmandu@x.test", json!({"properties": {"timezone": "Asia/Kathmandu"}})).await;
    person(&mut h, &l, "ny@x.test", json!({"properties": {"country": "US", "region": "NY"}})).await;
    person(&mut h, &l, "hawaii@x.test", json!({"phone_number": "+18085550100"})).await;
    person(&mut h, &l, "la@x.test", json!({})).await;
    // Never sent marketing by any path: unsubscribed, and hard-bounced.
    person(&mut h, &l, "gone@x.test", json!({"email_consent": "unsubscribed"})).await;
    let bounced = person(&mut h, &l, "bounced@x.test", json!({})).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'hard_bounce' WHERE id = $1::uuid").bind(&bounced).execute(&h.state.db).await.unwrap();

    let c = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Local hello", json!({"timing": "local_time", "local_time": "10:00"})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    assert_eq!(c["send_options"], json!({"timing": "local_time", "local_time": "10:00", "late": "next_day", "smart_sending": false}));
    let sent = h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(sent["status"], "sending", "today's earliest zone already passed 10:00, so it starts now: {sent}");
    h.drain().await;

    // Each recipient planned once, at the first 10:00 in their zone on or after today's date in the
    // account's zone, and nobody has it yet.
    let la: Tz = "America/Los_Angeles".parse().unwrap();
    let started: DateTime<Utc> = sqlx::query_scalar("SELECT sent_at FROM campaigns WHERE id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    let today = started.with_timezone(&la).date_naive();
    let planned = rows(&h, &cid).await;
    let zones: Vec<(&str, &str)> = planned.iter().map(|r| (r.0.as_str(), r.2.as_deref().unwrap())).collect();
    assert_eq!(
        zones,
        vec![("hawaii@x.test", "Pacific/Honolulu"), ("kathmandu@x.test", "Asia/Kathmandu"), ("kolkata@x.test", "Asia/Kolkata"), ("la@x.test", "America/Los_Angeles"), ("ny@x.test", "America/New_York")]
    );
    for (to, at, tz, status, _) in &planned {
        let tz: Tz = tz.as_deref().unwrap().parse().unwrap();
        let at = at.unwrap();
        assert_eq!(status, "queued", "{to}");
        let expected = clock::next_local(tz, started, Some(today), 600);
        assert!((at - expected).num_seconds().abs() <= 5, "{to}: {at} vs {expected}");
        assert_eq!(at.with_timezone(&tz).format("%H:%M").to_string(), "10:00", "{to}");
    }
    assert_eq!(h.state.email.sent().iter().filter(|m| m.subject == "Local hello").count(), 0);

    // Kathmandu's hour comes: only Kathmandu gets it.
    release(&h, &cid, Some("Asia/Kathmandu")).await;
    assert_eq!(received(&h, "kathmandu@x.test", "Local hello"), 1);
    assert_eq!(h.state.email.sent().iter().filter(|m| m.subject == "Local hello").count(), 1);

    // A restarted send job neither re-plans nor resends.
    let before: Vec<_> = rows(&h, &cid).await.into_iter().map(|r| (r.0, r.1)).collect();
    sqlx::query("INSERT INTO jobs (kind, payload) VALUES ('campaign_send', jsonb_build_object('campaign_id', $1::text))").bind(&cid).execute(&h.state.db).await.unwrap();
    h.drain().await;
    let after: Vec<_> = rows(&h, &cid).await.into_iter().map(|r| (r.0, r.1)).collect();
    assert_eq!(before, after);
    release(&h, &cid, Some("America/New_York")).await;
    release(&h, &cid, Some("Asia/Kathmandu")).await;
    assert_eq!(received(&h, "kathmandu@x.test", "Local hello"), 1, "never twice across buckets");
    assert_eq!(received(&h, "ny@x.test", "Local hello"), 1);

    // Progress by zone while it's under way.
    let p = h.ok("GET", &format!("/api/campaigns/{cid}/send-progress"), None).await;
    assert_eq!(p["totals"], json!({"waiting": 3, "sent": 2, "skipped_smart_sending": 0, "skipped_other": 0, "failed": 0}), "{p}");
    assert!(p["next_release"].is_string());
    assert!(p["buckets"].as_array().unwrap().iter().any(|b| b["zones"] == json!(["Pacific/Honolulu"]) && b["waiting"] == 1));

    // Cancelled mid-way: nobody else gets it, even when their hour comes.
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    release(&h, &cid, None).await;
    h.fast_forward().await;
    for to in ["kolkata@x.test", "hawaii@x.test", "la@x.test", "gone@x.test", "bounced@x.test"] {
        assert_eq!(received(&h, to, "Local hello"), 0, "{to}");
    }
    let p = h.ok("GET", &format!("/api/campaigns/{cid}/send-progress"), None).await;
    assert_eq!(p["totals"]["sent"], 2);
    assert_eq!(p["totals"]["skipped_other"], 3);
    assert_eq!(p["skip_reasons"], json!([{"reason": "campaign cancelled", "count": 3}]));
    h.cleanup().await;
}

#[tokio::test]
async fn local_time_ab_test_slice_and_winner_follow_the_zone_rule() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "AB").await;
    let zones = ["Asia/Kolkata", "Asia/Kathmandu", "America/New_York", "Europe/London", "Australia/Adelaide"];
    for i in 0..10 {
        person(&mut h, &l, &format!("ab{i}@x.test"), json!({"properties": {"timezone": zones[i % zones.len()]}})).await;
    }
    let mut body = campaign(&l, "Subject A", json!({"timing": "local_time", "local_time": "09:30", "late": "immediately"}));
    body["ab_test"] = json!({"variants": [{"id": "b", "content": {"subject": "Subject B", "blocks": [{"type": "text", "html": "<p>B</p>"}]}}], "test_percent": 20, "wait_hours": 1, "metric": "open_rate", "pick": "manual"});
    let cid = h.ok("POST", "/api/campaigns", Some(body)).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let slice = rows(&h, &cid).await;
    assert_eq!(slice.len(), 2, "20% of 10");
    for (to, at, tz, _, _) in &slice {
        let tz: Tz = tz.as_deref().unwrap().parse().unwrap();
        let at = at.unwrap();
        // "immediately": the slot is 09:30 local today, or right away when that already passed.
        assert!(at.with_timezone(&tz).format("%H:%M").to_string() == "09:30" || (at - Utc::now()).num_seconds() <= 5, "{to} {at}");
    }
    release(&h, &cid, None).await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(c["status"], "testing", "{c}");
    h.ok("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "b"}))).await;
    h.drain().await;
    let all = rows(&h, &cid).await;
    assert_eq!(all.len(), 10);
    assert!(all.iter().all(|r| r.2.is_some() && r.1.is_some()), "the rest are planned by zone too");
    release(&h, &cid, None).await;
    for i in 0..10 {
        let to = format!("ab{i}@x.test");
        let n = h.state.email.sent().iter().filter(|m| m.to == to && m.subject.starts_with("Subject ")).count();
        assert_eq!(n, 1, "{to}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn smart_sending_skips_recent_recipients_and_gradual_spreads_batches() {
    let mut h = harness!();
    h.signup().await;
    let first = list(&mut h, "First").await;
    let all = list(&mut h, "All").await;
    let a = person(&mut h, &first, "a@x.test", json!({})).await;
    h.ok("POST", &format!("/api/lists/{all}/members"), Some(json!({"profile_ids": [a]}))).await;
    person(&mut h, &all, "b@x.test", json!({})).await;
    person(&mut h, &all, "c@x.test", json!({})).await;

    let c1 = h.ok("POST", "/api/campaigns", Some(campaign(&first, "First one", Value::Null))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{c1}/send"), None).await;
    h.drain().await;
    assert_eq!(received(&h, "a@x.test", "First one"), 1);
    // A receipt is not marketing: it doesn't make b look recently messaged.
    h.ok("POST", "/api/transactional/send", Some(json!({"to": "b@x.test", "content": {"subject": "Your receipt", "text": "Thanks for your order."}}))).await;
    assert_eq!(received(&h, "b@x.test", "Your receipt"), 1);

    let opts = json!({"smart_sending": true, "smart_sending_hours": 16});
    let c2 = h.ok("POST", "/api/campaigns", Some(campaign(&all, "Second one", opts.clone()))).await["id"].as_str().unwrap().to_string();
    let preview = h.ok("POST", "/api/sendtime/preview", Some(json!({"campaign_id": c2}))).await;
    assert_eq!(preview["total"], 3);
    assert_eq!(preview["smart_sending_skips"], 1, "{preview}");
    h.ok("POST", &format!("/api/campaigns/{c2}/send"), None).await;
    h.drain().await;
    assert_eq!(received(&h, "a@x.test", "Second one"), 0);
    assert_eq!(received(&h, "b@x.test", "Second one"), 1);
    assert_eq!(received(&h, "c@x.test", "Second one"), 1);
    let report = h.ok("GET", &format!("/api/campaigns/{c2}/report"), None).await;
    assert_eq!(report["send_progress"]["totals"]["skipped_smart_sending"], 1, "{report}");
    assert_eq!(report["stats"]["recipients"], 2, "skipped people aren't recipients");
    let c = h.ok("GET", &format!("/api/campaigns/{c2}"), None).await;
    assert_eq!(c["status"], "sent");

    // Gradual over 3 hours: three hourly batches; the first goes now.
    let c3 = h.ok("POST", "/api/campaigns", Some(campaign(&all, "Third one", json!({"timing": "gradual", "spread_hours": 3})))).await["id"].as_str().unwrap().to_string();
    let preview = h.ok("POST", "/api/sendtime/preview", Some(json!({"campaign_id": c3}))).await;
    assert_eq!(preview["buckets"].as_array().unwrap().len(), 3, "{preview}");
    h.ok("POST", &format!("/api/campaigns/{c3}/send"), None).await;
    h.drain().await;
    let r = rows(&h, &c3).await;
    let start = r.iter().filter_map(|x| x.1).min().unwrap();
    let mut offsets: Vec<i64> = r.iter().map(|x| (x.1.unwrap() - start).num_minutes()).collect();
    offsets.sort();
    assert_eq!(offsets, vec![0, 60, 120]);
    assert_eq!(h.state.email.sent().iter().filter(|m| m.subject == "Third one").count(), 1);
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{c3}"), None).await["status"], "sending");
    release(&h, &c3, None).await;
    assert_eq!(h.state.email.sent().iter().filter(|m| m.subject == "Third one").count(), 3);
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{c3}"), None).await["status"], "sent");
    h.cleanup().await;
}

#[tokio::test]
async fn sms_waits_for_the_recipients_own_hours() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await;

    // Quiet-hour settings: only narrower than 08:00-21:00.
    let s = h.ok("GET", "/api/sendtime/settings", None).await;
    assert_eq!((s["sms_window_start"].as_str(), s["sms_window_end"].as_str()), (Some("09:00"), Some("20:00")));
    for bad in [json!({"sms_window_start": "07:30"}), json!({"sms_window_end": "21:30"}), json!({"sms_window_start": "12:00", "sms_window_end": "12:30"}), json!({"sms_window_start": "nine"})] {
        let (st, v) = h.req("PATCH", "/api/sendtime/settings", Some(bad.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    let s = h.ok("PATCH", "/api/sendtime/settings", Some(json!({"sms_window_start": "10:00", "sms_window_end": "19:00"}))).await;
    assert_eq!(s["sms_window_start"], "10:00");

    let l = list(&mut h, "Texts").await;
    // Zones from the phone number alone: Hawaii, New York, India, Nepal; and one in the account's zone.
    let phones = [("+18085550100", "Pacific/Honolulu"), ("+12125550100", "America/New_York"), ("+919812345678", "Asia/Kolkata"), ("+9779812345678", "Asia/Kathmandu"), ("+13125550100", "America/Chicago")];
    for (i, (phone, _)) in phones.iter().enumerate() {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("t{i}@x.test"), "phone_number": phone, "sms_consent": "subscribed", "list_ids": [l]}))).await;
    }
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Text", "channel": "sms", "audience": {"include": [{"type": "list", "id": l}]}, "content": {"body": "Hi from Acme. Reply STOP to opt out."}}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let now = Utc::now();
    h.drain().await;
    let sent: Vec<String> = h.state.sms.sent().iter().map(|m| m.to.clone()).collect();
    for (phone, zone) in phones {
        let tz: Tz = zone.parse().unwrap();
        let minute = clock::local_minute(tz, now);
        // Skip a recipient sitting on the window's edge while the test runs.
        if [600, 1140].iter().any(|edge| (minute as i64 - edge).abs() <= 1) {
            continue;
        }
        let row: (String, Option<DateTime<Utc>>) = sqlx::query_as("SELECT status, send_after FROM messages WHERE campaign_id = $1::uuid AND to_address = $2")
            .bind(&cid)
            .bind(phone)
            .fetch_one(&h.state.db)
            .await
            .unwrap();
        match clock::window_wait(tz, now, 600, 1140) {
            None => {
                assert!(sent.contains(&phone.to_string()), "{phone} ({zone}) is inside 10:00-19:00 and should be sent");
                assert_eq!(row.0, "sent");
            }
            Some(until) => {
                assert!(!sent.contains(&phone.to_string()), "{phone} ({zone}) is in quiet hours");
                assert_eq!(row.0, "queued");
                let at = row.1.unwrap();
                assert!((at - until).num_seconds().abs() <= 120, "{phone}: held until {at}, expected {until}");
                assert_eq!(at.with_timezone(&tz).format("%H:%M").to_string(), "10:00");
            }
        }
    }
    h.cleanup().await;
}

#[tokio::test]
async fn send_options_validation_api_and_mcp() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "L").await;
    person(&mut h, &l, "ann@x.test", json!({"properties": {"timezone": "Asia/Kolkata"}})).await;
    for bad in [json!({"timing": "whenever"}), json!({"timing": "gradual", "spread_hours": 500}), json!({"timing": "local_time", "local_time": "26:00"}), json!({"smart_sending": true, "smart_sending_hours": 0}), json!({"surprise": 1})] {
        let (s, v) = h.req("POST", "/api/campaigns", Some(campaign(&l, "x", bad.clone()))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    let c = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Opts", json!({"timing": "best_time"})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    assert_eq!(c["send_options"]["window_start"], "08:00");
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"name": "Renamed"}))).await;
    assert_eq!(c["send_options"]["timing"], "best_time", "a patch without send_options keeps them");
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"send_options": null}))).await;
    assert!(c["send_options"].is_null());
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"send_options": {"timing": "local_time", "local_time": "08:15"}}))).await;
    let dup = h.ok("POST", &format!("/api/campaigns/{cid}/duplicate"), None).await;
    assert_eq!(dup["send_options"], c["send_options"]);

    // A scheduled local-time campaign starts when the first zone reaches the hour on that day.
    let day = Utc::now() + chrono::Duration::days(3);
    let sched = h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": day}))).await;
    assert_eq!(sched["status"], "scheduled");
    let la: Tz = "America/Los_Angeles".parse().unwrap();
    let local_day = day.with_timezone(&la).date_naive();
    assert_eq!(sched["send_options"]["local_date"], json!(local_day.to_string()));
    let start: DateTime<Utc> = sched["send_at"].as_str().unwrap().parse().unwrap();
    assert_eq!(start, clock::local_instant(chrono_tz::Pacific::Kiritimati, local_day, 8 * 60 + 15));
    let p = h.ok("POST", "/api/sendtime/preview", Some(json!({"campaign_id": cid}))).await;
    let at: DateTime<Utc> = p["buckets"][0]["at"].as_str().unwrap().parse().unwrap();
    assert_eq!(at, clock::local_instant(chrono_tz::Asia::Kolkata, local_day, 8 * 60 + 15));
    assert_eq!(p["buckets"][0]["zones"], json!(["Asia/Kolkata"]));

    // Profile time zone lookup.
    let ann = h.ok("GET", "/api/profiles?q=ann", None).await["data"][0]["id"].as_str().unwrap().to_string();
    let z = h.ok("GET", &format!("/api/profiles/{ann}/timezone"), None).await;
    assert_eq!((z["timezone"].as_str(), z["source"].as_str()), (Some("Asia/Kolkata"), Some("profile")));

    // Another account can't see this campaign's progress or this profile.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "another pass", "company": "Other"}))).await;
    assert_eq!(other.req("GET", &format!("/api/campaigns/{cid}/send-progress"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.req("GET", &format!("/api/profiles/{ann}/timezone"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.req("POST", "/api/sendtime/preview", Some(json!({"campaign_id": cid}))).await.0, StatusCode::NOT_FOUND);

    // MCP: schemas carry send_options; send_campaign's dry run says when it would go.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "agent"}))).await["key"].as_str().unwrap().to_string();
    let mcp_router = h.router.clone();
    let mcp = |method: &str, params: Value| {
        let req = Request::builder().method("POST").uri("/mcp").header("authorization", format!("Bearer {key}")).header("content-type", "application/json")
            .body(Body::from(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string())).unwrap();
        let router = mcp_router.clone();
        async move {
            let res = router.oneshot(req).await.unwrap();
            serde_json::from_slice::<Value>(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
        }
    };
    let tools = mcp("tools/list", json!({})).await;
    let tools = tools["result"]["tools"].as_array().unwrap().clone();
    let schema = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name}"))["inputSchema"].clone();
    for name in ["create_campaign", "update_campaign", "send_campaign"] {
        assert!(schema(name)["properties"]["send_options"]["properties"]["timing"].is_object(), "{name}");
    }
    for name in ["preview_send_plan", "get_send_progress", "get_send_settings", "update_send_settings", "get_profile_timezone"] {
        assert!(tools.iter().any(|t| t["name"] == name), "{name}");
    }
    let c2 = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Via MCP", Value::Null))).await["id"].as_str().unwrap().to_string();
    let dry = mcp("tools/call", json!({"name": "send_campaign", "arguments": {"campaign_id": c2, "send_options": {"timing": "gradual", "spread_hours": 4}}})).await;
    let dry = &dry["result"]["structuredContent"];
    assert_eq!(dry["dry_run"], true, "{dry}");
    assert!(dry["send_plan"]["description"].as_str().unwrap().contains("4 hours"), "{dry}");
    let c2v = h.ok("GET", &format!("/api/campaigns/{c2}"), None).await;
    assert!(c2v["send_options"].is_null(), "a dry run saves nothing");
    let docs = mcp("resources/read", json!({"uri": "dumbmail://docs/sendtime"})).await;
    assert!(docs["result"]["contents"][0]["text"].as_str().unwrap().contains("local_time"));
    h.cleanup().await;
}

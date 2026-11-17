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

/// A half hour from 10:00 on that no zone in `zones` reached in the last 20 minutes or reaches in the next 2.
fn quiet_minute(zones: &[&str]) -> u32 {
    let now = Utc::now();
    (0..48u32)
        .map(|i| (600 + i * 30) % 1440)
        .find(|m| {
            zones.iter().all(|z| {
                let lm = clock::local_minute(z.parse().unwrap(), now);
                (lm + 1440 - m) % 1440 > 20 && (m + 1440 - lm) % 1440 > 2
            })
        })
        .unwrap()
}

/// When a local-time send asked for at `asked` reaches someone in `tz` (late: next day): at the hour
/// on `day`, right away when it passed less than 15 minutes before, otherwise the next day's.
fn expected_local(tz: Tz, asked: DateTime<Utc>, day: chrono::NaiveDate, minute: u32) -> DateTime<Utc> {
    let slot = clock::local_instant(tz, day, minute);
    if slot + chrono::Duration::minutes(15) >= asked { slot.max(asked) } else { clock::next_local(tz, asked, Some(day), minute) }
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

    // A local time that isn't about to come, or just gone, in any of these zones while the test runs
    // (a zone whose time passed less than 15 minutes before the send gets it right away).
    let minute = quiet_minute(&["Asia/Kolkata", "Asia/Kathmandu", "America/New_York", "Pacific/Honolulu", "America/Los_Angeles"]);
    let hm = clock::hhmm(minute);
    let c = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Local hello", json!({"timing": "local_time", "local_time": hm})))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    assert_eq!(c["send_options"], json!({"timing": "local_time", "local_time": hm, "late": "next_day", "smart_sending": false}));
    let sent = h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(sent["status"], "sending", "today's earliest zone already passed 10:00, so it starts now: {sent}");
    h.drain().await;

    // Each recipient planned once, at the first 10:00 in their zone on or after today's date in the
    // account's zone (or right away when it passed less than 15 minutes before the send), and nobody
    // has it yet.
    let la: Tz = "America/Los_Angeles".parse().unwrap();
    let started: DateTime<Utc> = sqlx::query_scalar("SELECT send_at FROM campaigns WHERE id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
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
        let expected = expected_local(tz, started, today, minute);
        assert!((at - expected).num_seconds().abs() <= 5, "{to}: {at} vs {expected}");
        if at > started {
            assert_eq!(at.with_timezone(&tz).format("%H:%M").to_string(), hm, "{to}");
        }
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
    h.ok("PATCH", "/api/transactional/settings", Some(json!({"enabled": true, "acknowledge": true}))).await;
    let me = h.ok("GET", "/api/me", None).await;
    let (s, _, body) = h.raw("POST", "/admin/transactional/mode", &[("authorization", "Bearer test-admin-token-0123456789abcdef0123"), ("content-type", "application/json")], &json!({"account_id": me["account"]["id"], "mode": "full"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
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

/// Add `n` subscribed people to a list straight in the database (much faster than the API).
async fn people(h: &Harness, list: &str, prefix: &str, n: i64) {
    sqlx::query(
        "WITH p AS (INSERT INTO profiles (id, account_id, email, email_consent)
                    SELECT gen_random_uuid(), l.account_id, $2 || g || '@x.test', 'subscribed' FROM lists l, generate_series(1, $3) g WHERE l.id = $1::uuid
                    RETURNING id)
         INSERT INTO list_members (list_id, profile_id) SELECT $1::uuid, id FROM p",
    )
    .bind(list)
    .bind(prefix)
    .bind(n)
    .execute(&h.state.db)
    .await
    .unwrap();
}

/// Run the campaign's send job now (and only that job).
async fn prepare(h: &Harness, cid: &str) {
    let n = sqlx::query("DELETE FROM jobs WHERE kind = 'campaign_send' AND payload->>'campaign_id' = $1").bind(cid).execute(&h.state.db).await.unwrap().rows_affected();
    assert_eq!(n, 1, "one send job");
    dumbmail::campaigns::prepare_send(&h.state, cid.parse().unwrap()).await.unwrap();
}

async fn campaign_row(h: &Harness, cid: &str) -> dumbmail::models::CampaignRow {
    sqlx::query_as("SELECT * FROM campaigns WHERE id = $1::uuid").bind(cid).fetch_one(&h.state.db).await.unwrap()
}

async fn ids(h: &Harness, cid: &str) -> Vec<uuid::Uuid> {
    sqlx::query_scalar("SELECT id FROM messages WHERE campaign_id = $1::uuid AND NOT is_test ORDER BY id").bind(cid).fetch_all(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn a_plain_send_now_campaign_goes_out_exactly_as_before() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let l = list(&mut h, "Everyone").await;
    people(&h, &l, "plain", 250).await;
    let cid = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Plain one", Value::Null))).await["id"].as_str().unwrap().to_string();
    let asked = Utc::now();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    // The send job: every recipient queued, nothing planned, then batches of 100 in id order, due now.
    prepare(&h, &cid).await;
    let all = ids(&h, &cid).await;
    assert_eq!(all.len(), 250);
    let unplanned: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND status = 'queued' AND send_after IS NULL AND send_tz IS NULL AND planned_from IS NULL")
        .bind(&cid)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(unplanned, 250);
    let batches: Vec<(Value, DateTime<Utc>)> = sqlx::query_as("SELECT payload, run_at FROM jobs WHERE kind = 'campaign_batch' ORDER BY id").fetch_all(&h.state.db).await.unwrap();
    let expected: Vec<Vec<String>> = all.chunks(100).map(|c| c.iter().map(|u| u.to_string()).collect()).collect();
    let got: Vec<Vec<String>> = batches.iter().map(|(p, _)| serde_json::from_value(p["message_ids"].clone()).unwrap()).collect();
    assert_eq!(got, expected);
    assert!(batches.iter().all(|(_, at)| *at >= asked - chrono::Duration::seconds(1) && *at <= Utc::now()), "due right away: {batches:?}");
    // Everyone gets it once, in that order.
    h.drain().await;
    let to: Vec<String> = sqlx::query_scalar("SELECT to_address FROM messages WHERE campaign_id = $1::uuid ORDER BY id").bind(&cid).fetch_all(&h.state.db).await.unwrap();
    let sent: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject == "Plain one").map(|m| m.to).collect();
    assert_eq!(sent.len(), 250);
    assert_eq!(sent, to[to.len() - sent.len()..].to_vec(), "sent in id order");
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");
    h.cleanup().await;
}

#[tokio::test]
async fn the_audience_is_fixed_when_the_send_starts() {
    let mut h = harness!();
    h.signup().await;
    for opts in [Value::Null, json!({"timing": "gradual", "spread_hours": 3})] {
        let l = list(&mut h, "Fixed").await;
        let tag = if opts.is_null() { "now" } else { "slow" };
        person(&mut h, &l, &format!("first-{tag}@x.test"), json!({})).await;
        person(&mut h, &l, &format!("second-{tag}@x.test"), json!({})).await;
        let cid = h.ok("POST", "/api/campaigns", Some(campaign(&l, &format!("Fixed {tag}"), opts.clone()))).await["id"].as_str().unwrap().to_string();
        h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
        prepare(&h, &cid).await;
        // Someone joins while it's going out, then the send job runs again (a retry or a duplicate).
        let joiner = format!("joiner-{tag}@x.test");
        person(&mut h, &l, &joiner, json!({})).await;
        sqlx::query("INSERT INTO jobs (kind, payload) VALUES ('campaign_send', jsonb_build_object('campaign_id', $1::text))").bind(&cid).execute(&h.state.db).await.unwrap();
        h.drain().await;
        release(&h, &cid, None).await;
        assert_eq!(rows(&h, &cid).await.len(), 2, "{tag}: nobody added after the start");
        assert_eq!(received(&h, &joiner, &format!("Fixed {tag}")), 0, "{tag}");
        assert_eq!(received(&h, &format!("second-{tag}@x.test"), &format!("Fixed {tag}")), 1, "{tag}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn an_ab_holdout_who_leaves_the_list_still_gets_the_winner() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "AB fixed").await;
    for i in 0..10 {
        person(&mut h, &l, &format!("h{i}@x.test"), json!({})).await;
    }
    let mut body = campaign(&l, "Version A", Value::Null);
    body["ab_test"] = json!({"variants": [{"id": "b", "content": {"subject": "Version B", "blocks": [{"type": "text", "html": "<p>B</p>"}]}}], "test_percent": 20, "wait_hours": 1, "metric": "open_rate", "pick": "manual"});
    let cid = h.ok("POST", "/api/campaigns", Some(body)).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let slice: Vec<String> = rows(&h, &cid).await.into_iter().map(|r| r.0).collect();
    assert_eq!(slice.len(), 2);
    let holdouts: Vec<String> = (0..10).map(|i| format!("h{i}@x.test")).filter(|e| !slice.contains(e)).collect();
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM campaign_holdouts WHERE campaign_id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, 8);
    // One holdout leaves the list (still subscribed); another unsubscribes; someone new joins.
    sqlx::query("DELETE FROM list_members WHERE profile_id = (SELECT id FROM profiles WHERE email = $1)").bind(&holdouts[0]).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE email = $1").bind(&holdouts[1]).execute(&h.state.db).await.unwrap();
    person(&mut h, &l, "late@x.test", json!({})).await;
    h.ok("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "b"}))).await;
    h.drain().await;
    assert_eq!(received(&h, &holdouts[0], "Version B"), 1, "left the list but was in the audience when it started");
    assert_eq!(received(&h, &holdouts[1], "Version B"), 0, "consent is still checked");
    assert_eq!(received(&h, "late@x.test", "Version B"), 0, "joined after the start");
    for e in &holdouts[2..] {
        assert_eq!(received(&h, e, "Version B"), 1, "{e}");
    }
    for e in &slice {
        assert_eq!(received(&h, e, "Version B") + received(&h, e, "Version A"), 1, "{e}: test slice only once");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn interrupted_planning_gives_everyone_the_same_time_on_restart() {
    let mut h = harness!();
    h.signup().await;
    let zones = ["Asia/Kolkata", "Asia/Kathmandu", "America/New_York", "Europe/London", "Pacific/Honolulu", "Australia/Adelaide"];
    let minute = quiet_minute(&zones);
    for opts in [json!({"timing": "gradual", "spread_hours": 4}), json!({"timing": "local_time", "local_time": clock::hhmm(minute)}), json!({"timing": "best_time", "within_hours": 24})] {
        let l = list(&mut h, "Restart").await;
        for (i, z) in zones.iter().enumerate() {
            person(&mut h, &l, &format!("r{i}-{}@x.test", opts["timing"].as_str().unwrap()), json!({"properties": {"timezone": z}})).await;
        }
        let cid = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Restart", opts.clone()))).await["id"].as_str().unwrap().to_string();
        h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
        prepare(&h, &cid).await;
        let before = rows(&h, &cid).await;
        assert!(before.iter().all(|r| r.1.is_some() && r.2.is_some()), "{opts}: all planned");
        // The worker died part way through the second step: half the messages have no release time yet
        // (the first step, a single statement, either happened or didn't). The restart is later.
        let all = ids(&h, &cid).await;
        let half: Vec<uuid::Uuid> = all[all.len() / 2..].to_vec();
        let gradual = opts["timing"] == "gradual";
        sqlx::query("UPDATE messages SET send_tz = NULL, send_after = CASE WHEN $2 THEN send_after END WHERE id = ANY($1)").bind(&half).bind(gradual).execute(&h.state.db).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let n = dumbmail::features::sendtime::plan::plan_campaign(&h.state, &campaign_row(&h, &cid).await).await.unwrap();
        assert_eq!(n as usize, half.len(), "{opts}");
        assert_eq!(rows(&h, &cid).await, before, "{opts}: the same times as before the crash");
        if gradual {
            let start = before.iter().filter_map(|r| r.1).min().unwrap();
            let mut hours: Vec<i64> = before.iter().map(|r| (r.1.unwrap() - start).num_minutes() / 60).collect();
            hours.sort();
            assert_eq!(hours, vec![0, 0, 1, 2, 2, 3], "{opts}: equal hourly batches");
        }
    }
    h.cleanup().await;
}

#[tokio::test]
async fn a_batch_never_leaves_a_message_behind_between_its_reads() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let l = list(&mut h, "Dense").await;
    people(&h, &l, "dense", 400).await;
    let cid = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Dense", json!({"timing": "gradual", "spread_hours": 2})))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    prepare(&h, &cid).await;
    sqlx::query("DELETE FROM jobs WHERE kind = 'campaign_batch'").execute(&h.state.db).await.unwrap();
    // Release times a quarter of a millisecond apart, starting now: some come due while the batch reads.
    sqlx::query(
        "UPDATE messages m SET send_after = now() + x.rn * interval '250 microseconds'
         FROM (SELECT id, row_number() OVER (ORDER BY id) AS rn FROM messages WHERE campaign_id = $1::uuid) x WHERE m.id = x.id",
    )
    .bind(&cid)
    .execute(&h.state.db)
    .await
    .unwrap();
    let all = ids(&h, &cid).await;
    dumbmail::campaigns::send_batch(&h.state, cid.parse().unwrap(), &all).await.unwrap();
    let orphans: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT m.id FROM messages m WHERE m.campaign_id = $1::uuid AND m.status = 'queued'
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'campaign_batch' AND j.run_at <> 'infinity' AND j.payload->'message_ids' ? m.id::text)",
    )
    .bind(&cid)
    .fetch_all(&h.state.db)
    .await
    .unwrap();
    assert!(orphans.is_empty(), "queued with no batch to send them: {orphans:?}");
    h.fast_forward().await;
    assert_eq!(h.state.email.sent().iter().filter(|m| m.subject == "Dense").count(), 400);
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");
    h.cleanup().await;
}

/// A one-person campaign that is sending, with its message claimed by a worker.
async fn claimed_race(h: &mut Harness, l: &str) -> (uuid::Uuid, uuid::Uuid) {
    let cid = h.ok("POST", "/api/campaigns", Some(campaign(l, "Race", Value::Null))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    prepare(h, &cid).await;
    sqlx::query("DELETE FROM jobs WHERE kind = 'campaign_batch'").execute(&h.state.db).await.unwrap();
    let mid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM messages WHERE campaign_id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    let cid: uuid::Uuid = cid.parse().unwrap();
    assert!(dumbmail::campaigns::claim(&h.state.db, cid, mid, "email", Utc::now()).await.unwrap().is_some());
    (cid, mid)
}

#[tokio::test]
async fn a_cancel_landing_between_claim_and_requeue_leaves_nothing_queued() {
    let mut h = harness!();
    h.signup().await;
    let acct: uuid::Uuid = h.ok("GET", "/api/me", None).await["account"]["id"].as_str().unwrap().parse().unwrap();
    let l = list(&mut h, "Race").await;
    person(&mut h, &l, "race@x.test", json!({})).await;
    let db = h.state.db.clone();
    let status = |mid: uuid::Uuid| {
        let db = db.clone();
        async move { sqlx::query_as::<_, (String, Option<String>)>("SELECT status, error FROM messages WHERE id = $1").bind(mid).fetch_one(&db).await.unwrap() }
    };
    // Claimed, then cancelled, then the worker puts it back (a warm-up or quiet-hours wait): skipped.
    let (cid, mid) = claimed_race(&mut h, &l).await;
    dumbmail::api::messaging::cancel(&h.state, acct, cid).await.unwrap();
    assert!(!dumbmail::campaigns::requeue_claimed(&h.state.db, cid, mid, "email", None).await.unwrap());
    assert_eq!(status(mid).await, ("skipped".to_string(), Some("campaign cancelled".to_string())));
    // Put back first, then cancelled: skipped by the cancel. A claim after a cancel gets nothing.
    let (cid, mid) = claimed_race(&mut h, &l).await;
    assert!(dumbmail::campaigns::requeue_claimed(&h.state.db, cid, mid, "email", None).await.unwrap());
    dumbmail::api::messaging::cancel(&h.state, acct, cid).await.unwrap();
    assert_eq!(status(mid).await.0, "skipped");
    assert!(dumbmail::campaigns::claim(&h.state.db, cid, mid, "email", Utc::now()).await.unwrap().is_none());
    // Both at once, many times: never left queued behind a cancelled campaign.
    for round in 0..25 {
        let (cid, mid) = claimed_race(&mut h, &l).await;
        let state = h.state.clone();
        let (c, r) = tokio::join!(dumbmail::api::messaging::cancel(&state, acct, cid), dumbmail::campaigns::requeue_claimed(&state.db, cid, mid, "email", None));
        c.unwrap();
        r.unwrap();
        assert_eq!(status(mid).await.0, "skipped", "round {round}");
    }
    // A pause stops the campaign the same way.
    let (cid, mid) = claimed_race(&mut h, &l).await;
    dumbmail::deliverability::pause(&h.state, acct, "test pause").await.unwrap();
    assert!(!dumbmail::campaigns::requeue_claimed(&h.state.db, cid, mid, "email", None).await.unwrap());
    assert_eq!(status(mid).await.0, "skipped");
    h.cleanup().await;
}

#[tokio::test]
async fn sms_uses_the_phone_numbers_clock_and_notes_when_zones_disagree() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await;
    let l = list(&mut h, "Far").await;
    // A New York number on a profile that says Manila: their texting hours never overlap.
    let pid = h.ok("POST", "/api/profiles", Some(json!({"email": "far@x.test", "phone_number": "+12125550100", "sms_consent": "subscribed", "properties": {"timezone": "Asia/Manila"}, "list_ids": [l]}))).await["id"].as_str().unwrap().to_string();
    let text = |name: &str| json!({"name": name, "channel": "sms", "audience": {"include": [{"type": "list", "id": l}]}, "content": {"body": "Hi from Acme. Reply STOP to opt out."}});
    let cid = h.ok("POST", "/api/campaigns", Some(text("Far 1"))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let now = Utc::now();
    h.drain().await;
    let ny: Tz = "America/New_York".parse().unwrap();
    let (status, send_after, note): (String, Option<DateTime<Utc>>, Option<String>) =
        sqlx::query_as("SELECT status, send_after, send_note FROM messages WHERE campaign_id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    match clock::window_wait(ny, now, 540, 1200) {
        // New York's hours decide, whatever the time in Manila.
        None => {
            assert_eq!(status, "sent");
            let note = note.expect("noted");
            assert!(note.contains("America/New_York") && note.contains("Asia/Manila"), "{note}");
            let detail = h.ok("GET", &format!("/api/profiles/{pid}"), None).await;
            assert_eq!(detail["messages"][0]["note"], json!(note), "on the profile timeline");
            assert!(detail["events"].as_array().unwrap().iter().any(|e| e["metric"] == "Received SMS" && e["properties"]["time_zone_note"] == json!(note)), "{detail}");
        }
        Some(until) => {
            assert_eq!(status, "queued");
            assert!((send_after.unwrap() - until).num_seconds().abs() <= 120, "waits for New York's 09:00, not Manila's");
        }
    }
    // The account can choose not to text such people: skipped, with the reason and the note.
    let s = h.ok("PATCH", "/api/sendtime/settings", Some(json!({"sms_zone_conflict": "skip"}))).await;
    assert_eq!(s["sms_zone_conflict"], "skip");
    assert_eq!(h.req("PATCH", "/api/sendtime/settings", Some(json!({"sms_zone_conflict": "manila"}))).await.0, StatusCode::BAD_REQUEST);
    let before = h.state.sms.sent().len();
    let cid = h.ok("POST", "/api/campaigns", Some(text("Far 2"))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let (status, error, note): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT status, error, send_note FROM messages WHERE campaign_id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), error.as_deref()), ("skipped", Some("time zones disagree")));
    assert!(note.unwrap().contains("weren't texted"));
    assert_eq!(h.state.sms.sent().len(), before);
    h.cleanup().await;
}

#[tokio::test]
async fn a_text_waiting_for_its_turn_checks_quiet_hours_again_before_it_goes() {
    // One text a second, so the second waits about a second for its turn.
    let mut h = match Harness::with(|c| c.send_rate_per_sec = 1).await {
        Some(h) => h,
        None => return,
    };
    h.signup().await;
    h.credit(5.0).await;
    h.claim_number().await;
    let acct: uuid::Uuid = h.ok("GET", "/api/me", None).await["account"]["id"].as_str().unwrap().parse().unwrap();
    // A number whose local time is inside 08:00-21:00 right now.
    let now = Utc::now();
    let (phone, tz) = [("+12125550100", "America/New_York"), ("+18085550100", "Pacific/Honolulu"), ("+919812345678", "Asia/Kolkata"), ("+13125550100", "America/Chicago")]
        .into_iter()
        .map(|(p, z)| (p, z.parse::<Tz>().unwrap()))
        .find(|(_, z)| (8 * 60 + 2..21 * 60 - 2).contains(&clock::local_minute(*z, now)))
        .unwrap();
    h.ok("PATCH", "/api/sendtime/settings", Some(json!({"sms_window_start": "08:00", "sms_window_end": "21:00"}))).await;
    let pid = h.ok("POST", "/api/profiles", Some(json!({"phone_number": phone, "sms_consent": "subscribed"}))).await["id"].as_str().unwrap().to_string();
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1::uuid", dumbmail::models::PROFILE_COLS);
    let profile: dumbmail::models::Profile = sqlx::query_as(&q).bind(&pid).fetch_one(&h.state.db).await.unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let content = json!({"body": "Your code is ready. Reply STOP to opt out."});
    let spec = || dumbmail::sending::SendSpec {
        account: &account,
        profile: &profile,
        channel: "sms",
        content: &content,
        campaign_id: None,
        flow_id: None,
        flow_node_id: None,
        event: None,
        is_test: false,
        message_id: None,
        to_override: None,
    };
    // Three texts at once take turns a second apart; the window closes while two of them wait.
    let close = async {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let window = if clock::local_minute(tz, Utc::now()) < 14 * 60 { ("19:00", "20:00") } else { ("08:00", "09:00") };
        sqlx::query("UPDATE send_settings SET sms_start_minute = $2, sms_end_minute = $3 WHERE account_id = $1")
            .bind(acct)
            .bind(clock::parse_hhmm(window.0).unwrap() as i32)
            .bind(clock::parse_hhmm(window.1).unwrap() as i32)
            .execute(&h.state.db)
            .await
            .unwrap();
    };
    let (a, b, c, ()) = tokio::join!(dumbmail::sending::send(&h.state, spec()), dumbmail::sending::send(&h.state, spec()), dumbmail::sending::send(&h.state, spec()), close);
    let outcomes: Vec<dumbmail::sending::Outcome> = [a, b, c].into_iter().map(Result::unwrap).collect();
    let sent = outcomes.iter().filter(|o| matches!(o, dumbmail::sending::Outcome::Sent(_))).count();
    let held: Vec<DateTime<Utc>> = outcomes.iter().filter_map(|o| if let dumbmail::sending::Outcome::Deferred(t) = o { Some(*t) } else { None }).collect();
    assert_eq!((sent, held.len()), (1, 2), "only the one whose turn came before the window closed goes: {outcomes:?}");
    assert!(held.iter().all(|t| *t > Utc::now()), "{held:?}");
    assert_eq!(h.state.sms.sent().iter().filter(|m| m.to == phone).count(), 1);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE profile_id = $1::uuid").bind(&pid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 1, "the held text leaves no message behind (a flow re-runs it)");
    h.cleanup().await;
}

#[tokio::test]
async fn rest_send_and_schedule_take_send_options() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "Rest").await;
    person(&mut h, &l, "rest@x.test", json!({})).await;
    let cid = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Rest", Value::Null))).await["id"].as_str().unwrap().to_string();
    let at = Utc::now() + chrono::Duration::days(2);
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at, "surprise": 1}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown fields aren't ignored: {v}");
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at, "send_options": {"timing": "whenever"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "draft");
    let c = h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at, "send_options": {"timing": "gradual", "spread_hours": 3}}))).await;
    assert_eq!((c["status"].as_str(), c["send_options"]["timing"].as_str(), c["send_options"]["spread_hours"].as_i64()), (Some("scheduled"), Some("gradual"), Some(3)), "{c}");
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/send"), Some(json!({"send_options": {"smart_sending": true, "smart_sending_hours": 0}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let c = h.ok("POST", &format!("/api/campaigns/{cid}/send"), Some(json!({"send_options": {"smart_sending": true}}))).await;
    assert_eq!((c["status"].as_str(), c["send_options"]["smart_sending"].as_bool()), (Some("sending"), Some(true)), "{c}");
    // Options can't change once it's sending.
    let (s, _) = h.req("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at, "send_options": null}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.cleanup().await;
}

#[tokio::test]
async fn a_relaunched_campaign_goes_to_its_edited_audience_and_nobody_twice() {
    let mut h = harness!();
    h.signup().await;
    let first = list(&mut h, "First").await;
    for i in 0..4 {
        person(&mut h, &first, &format!("a{i}@x.test"), json!({})).await;
    }
    // Spread over two hours: half go now, half in an hour. Cancel before the second half.
    let cid = h.ok("POST", "/api/campaigns", Some(campaign(&first, "Relaunch", json!({"timing": "gradual", "spread_hours": 2})))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let r = rows(&h, &cid).await;
    let sent: Vec<String> = r.iter().filter(|x| x.3 == "sent").map(|x| x.0.clone()).collect();
    let waiting: Vec<String> = r.iter().filter(|x| x.3 == "queued").map(|x| x.0.clone()).collect();
    assert_eq!((sent.len(), waiting.len()), (2, 2), "{r:?}");
    h.ok("POST", &format!("/api/campaigns/{cid}/cancel"), None).await;
    // Edit: a new list with one person who got it, one who didn't yet, and someone new; send now.
    let second = list(&mut h, "Second").await;
    for e in [&sent[0], &waiting[0]] {
        sqlx::query("INSERT INTO list_members (list_id, profile_id) SELECT $1::uuid, id FROM profiles WHERE email = $2").bind(&second).bind(e).execute(&h.state.db).await.unwrap();
    }
    person(&mut h, &second, "new@x.test", json!({})).await;
    let edited = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"audience": {"include": [{"type": "list", "id": second}]}, "send_options": null}))).await;
    assert_eq!(edited["status"], "draft");
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");
    assert_eq!(received(&h, "new@x.test", "Relaunch"), 1, "the edited audience gets it");
    assert_eq!(received(&h, &waiting[0], "Relaunch"), 1, "including someone the first attempt never reached");
    assert_eq!(received(&h, &sent[0], "Relaunch"), 1, "nobody gets a second copy");
    assert_eq!(received(&h, &sent[1], "Relaunch"), 1);
    assert_eq!(received(&h, &waiting[1], "Relaunch"), 0, "no longer in the audience");
    // The report counts each person who got it once, and nothing left over from the first attempt.
    let r = rows(&h, &cid).await;
    assert_eq!(r.len(), 4, "{r:?}");
    assert!(r.iter().all(|x| x.3 == "sent"), "{r:?}");
    let report = h.ok("GET", &format!("/api/campaigns/{cid}/report"), None).await;
    assert_eq!(report["stats"]["recipients"], 4, "{report}");
    h.cleanup().await;
}

#[tokio::test]
async fn send_jobs_hold_one_connection_each_even_on_a_tiny_pool() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let l = list(&mut h, "Pool").await;
    people(&h, &l, "pool", 40).await;
    // Several A/B campaigns (the test slice, its versions and the holdouts) and plain ones start at once.
    let mut cids = Vec::new();
    for i in 0..6 {
        let mut body = campaign(&l, &format!("Pool {i}"), Value::Null);
        if i % 2 == 0 {
            body["ab_test"] = json!({"variants": [{"id": "b", "content": {"subject": "B", "blocks": [{"type": "text", "html": "<p>B</p>"}]}}], "test_percent": 25, "wait_hours": 1, "metric": "open_rate", "pick": "manual"});
        }
        let cid = h.ok("POST", "/api/campaigns", Some(body)).await["id"].as_str().unwrap().to_string();
        h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
        sqlx::query("DELETE FROM jobs WHERE kind = 'campaign_send' AND payload->>'campaign_id' = $1").bind(&cid).execute(&h.state.db).await.unwrap();
        cids.push(cid);
    }
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(2).acquire_timeout(std::time::Duration::from_secs(5)).connect(&h.state.cfg.database_url).await.unwrap();
    let s = &h.state;
    let small = dumbmail::state::AppState(std::sync::Arc::new(dumbmail::state::Inner {
        db: pool,
        cfg: s.cfg.clone(),
        email: s.email.clone(),
        sms: s.sms.clone(),
        http: s.http.clone(),
        pending_checks: Default::default(),
        daily_limits: Default::default(),
    }));
    let runs = cids.iter().map(|c| dumbmail::campaigns::prepare_send(&small, c.parse().unwrap()));
    let results = tokio::time::timeout(std::time::Duration::from_secs(60), futures_util::future::join_all(runs)).await.expect("no stall");
    for r in results {
        r.unwrap();
    }
    // Each test slice and its holdouts come from one read: together exactly the audience, no overlap.
    for (i, cid) in cids.iter().enumerate() {
        let (msgs, holdouts, both): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM messages WHERE campaign_id = $1::uuid AND NOT is_test),
                    (SELECT count(*) FROM campaign_holdouts WHERE campaign_id = $1::uuid),
                    (SELECT count(*) FROM campaign_holdouts h JOIN messages m ON m.profile_id = h.profile_id AND m.campaign_id = h.campaign_id WHERE h.campaign_id = $1::uuid)",
        )
        .bind(cid)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
        if i % 2 == 0 {
            assert_eq!((msgs, holdouts, both), (10, 30, 0), "{cid}");
            let versions: Vec<(String, i64)> = sqlx::query_as("SELECT variant, count(*) FROM messages WHERE campaign_id = $1::uuid GROUP BY 1 ORDER BY 1").bind(cid).fetch_all(&h.state.db).await.unwrap();
            assert_eq!(versions, vec![("a".to_string(), 5), ("b".to_string(), 5)]);
        } else {
            assert_eq!((msgs, holdouts), (40, 0), "{cid}");
        }
    }
    small.db.close().await;
    h.cleanup().await;
}

#[tokio::test]
async fn an_unsubscribe_or_cancel_during_the_wait_for_a_send_slot_stops_the_email() {
    // One email a second, so the second waits about a second for its turn.
    let mut h = match Harness::with(|c| c.send_rate_per_sec = 1).await {
        Some(h) => h,
        None => return,
    };
    h.signup().await;
    h.credit(5.0).await;
    let acct: uuid::Uuid = h.ok("GET", "/api/me", None).await["account"]["id"].as_str().unwrap().parse().unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();

    // Two flow emails to one person; they unsubscribe while the second waits for its slot.
    let pid = h.ok("POST", "/api/profiles", Some(json!({"email": "quit@x.test", "email_consent": "subscribed"}))).await["id"].as_str().unwrap().to_string();
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1::uuid", dumbmail::models::PROFILE_COLS);
    let profile: dumbmail::models::Profile = sqlx::query_as(&q).bind(&pid).fetch_one(&h.state.db).await.unwrap();
    let content = json!({"subject": "Flow mail", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]});
    let spec = || dumbmail::sending::SendSpec {
        account: &account,
        profile: &profile,
        channel: "email",
        content: &content,
        campaign_id: None,
        flow_id: None,
        flow_node_id: None,
        event: None,
        is_test: false,
        message_id: None,
        to_override: None,
    };
    let unsubscribe = async {
        // Both checked consent on the profile as read before sending. Once the first has gone, the
        // second waits about a second for its slot: they unsubscribe then.
        for _ in 0..1000 {
            let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE profile_id = $1::uuid AND status = 'sent'").bind(&pid).fetch_one(&h.state.db).await.unwrap();
            if sent == 1 {
                sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE id = $1::uuid").bind(&pid).execute(&h.state.db).await.unwrap();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the first email never went");
    };
    let (a, b, ()) = tokio::join!(dumbmail::sending::send(&h.state, spec()), dumbmail::sending::send(&h.state, spec()), unsubscribe);
    let outcomes: Vec<dumbmail::sending::Outcome> = [a, b].into_iter().map(Result::unwrap).collect();
    assert_eq!(outcomes.iter().filter(|o| matches!(o, dumbmail::sending::Outcome::Sent(_))).count(), 1, "{outcomes:?}");
    assert!(outcomes.iter().any(|o| matches!(o, dumbmail::sending::Outcome::Skipped("not emailable"))), "{outcomes:?}");
    assert_eq!(received(&h, "quit@x.test", "Flow mail"), 1);
    let skipped: Vec<(String, Option<String>)> = sqlx::query_as("SELECT status, error FROM messages WHERE profile_id = $1::uuid AND status <> 'sent'").bind(&pid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(skipped, vec![("skipped".to_string(), Some("not emailable".to_string()))], "stopped at the last check, and recorded");

    // A campaign to two people, cancelled while the second email waits for its slot.
    let l = list(&mut h, "Cancel mid-wait").await;
    person(&mut h, &l, "c1@x.test", json!({})).await;
    person(&mut h, &l, "c2@x.test", json!({})).await;
    let cid = h.ok("POST", "/api/campaigns", Some(campaign(&l, "Cancel me", Value::Null))).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    let cancel = async {
        // Wait until one has gone and the other is claimed (waiting for its slot), then cancel.
        for _ in 0..1000 {
            let (sent, sending): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE status = 'sent'), count(*) FILTER (WHERE status = 'sending') FROM messages WHERE campaign_id = $1::uuid")
                .bind(&cid)
                .fetch_one(&h.state.db)
                .await
                .unwrap();
            if sent == 1 && sending == 1 {
                dumbmail::api::messaging::cancel(&h.state, acct, cid.parse().unwrap()).await.unwrap();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("never saw one sent and one waiting");
    };
    let ((), ()) = tokio::join!(h.drain(), cancel);
    assert_eq!(received(&h, "c1@x.test", "Cancel me") + received(&h, "c2@x.test", "Cancel me"), 1, "the waiting email didn't go");
    let stopped: (String, Option<String>, bool) =
        sqlx::query_as("SELECT status, error, claimed_at IS NOT NULL FROM messages WHERE campaign_id = $1::uuid AND status <> 'sent'").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stopped, ("skipped".to_string(), Some("campaign cancelled".to_string()), true), "it was in flight when the cancel landed");
    h.cleanup().await;
}

#[tokio::test]
async fn the_winner_send_is_planned_from_when_the_winner_was_picked() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "Winner time").await;
    for i in 0..10 {
        person(&mut h, &l, &format!("w{i}@x.test"), json!({})).await;
    }
    let mut body = campaign(&l, "Version A", json!({"timing": "gradual", "spread_hours": 4}));
    body["ab_test"] = json!({"variants": [{"id": "b", "content": {"subject": "Version B", "blocks": [{"type": "text", "html": "<p>B</p>"}]}}], "test_percent": 20, "wait_hours": 1, "metric": "open_rate", "pick": "manual"});
    let cid = h.ok("POST", "/api/campaigns", Some(body)).await["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.fast_forward().await;
    release(&h, &cid, None).await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "testing");
    h.ok("POST", &format!("/api/campaigns/{cid}/winner"), Some(json!({"version": "b"}))).await;
    let picked: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT ab_winner_at FROM campaigns WHERE id = $1::uuid").bind(&cid).fetch_one(&h.state.db).await.unwrap();
    let picked = picked.expect("stored when picked");
    h.drain().await;
    let from: Vec<DateTime<Utc>> = sqlx::query_scalar("SELECT DISTINCT planned_from FROM messages WHERE campaign_id = $1::uuid AND variant IS NULL").bind(&cid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(from, vec![picked], "every winner message planned from the pick");
    h.cleanup().await;
}

#[tokio::test]
async fn send_options_are_saved_only_when_the_campaign_launches() {
    let mut h = harness!();
    h.signup().await;
    let l = list(&mut h, "Not ready").await;
    person(&mut h, &l, "nr@x.test", json!({})).await;
    let mut body = campaign(&l, "Not ready", Value::Null);
    body["content"]["subject"] = json!("");
    let cid = h.ok("POST", "/api/campaigns", Some(body)).await["id"].as_str().unwrap().to_string();
    for (path, b) in [("send", json!({"send_options": {"timing": "gradual", "spread_hours": 3}})), ("schedule", json!({"send_at": Utc::now() + chrono::Duration::days(1), "send_options": {"smart_sending": true}}))] {
        let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/{path}"), Some(b)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{path}: {v}");
        let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
        assert_eq!((c["status"].as_str(), &c["send_options"]), (Some("draft"), &Value::Null), "{path}: not saved when it didn't launch: {c}");
    }
    h.cleanup().await;
}

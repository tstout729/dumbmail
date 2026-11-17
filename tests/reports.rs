//! Reporting: attribution settings, custom and ready-made reports, cohorts, funnels,
//! deliverability, benchmarks and CSV exports.

#[macro_use]
mod common;

use axum::http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn profile(h: &Harness, acct: Uuid, email: &str, props: Value) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent, properties) VALUES ($1, $2, $3, 'subscribed', $4)")
        .bind(id)
        .bind(acct)
        .bind(email)
        .bind(props)
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

async fn campaign(h: &Harness, acct: Uuid, name: &str, audience: Value) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO campaigns (id, account_id, name, channel, status, sent_at, audience) VALUES ($1, $2, $3, 'email', 'sent', now() - interval '48 hours', $4)")
        .bind(id)
        .bind(acct)
        .bind(name)
        .bind(audience)
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

async fn flow(h: &Harness, acct: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO flows (id, account_id, name, status, trigger, definition) VALUES ($1, $2, $3, 'live', $4, $5)")
        .bind(id)
        .bind(acct)
        .bind(name)
        .bind(json!({"type": "list", "list_id": Uuid::nil()}))
        .bind(json!({"entry": "e1", "nodes": [{"id": "e1", "type": "email", "name": "Welcome email", "content": {"subject": "Hi"}}]}))
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

struct Msg {
    profile: Uuid,
    to: String,
    campaign: Option<Uuid>,
    flow: Option<Uuid>,
    status: &'static str,
    created_h: f64,
    opened_h: Option<f64>,
    clicked_h: Option<f64>,
    unsubscribed: bool,
}

async fn message(h: &Harness, acct: Uuid, m: Msg) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, status, bounce_type, created_at, sent_at,
                               opened_at, clicked_at, unsubscribed_at, from_domain)
         VALUES ($1, $2, $3, 'email', $4, $5, CASE WHEN $5::uuid IS NULL THEN NULL ELSE 'e1' END, $6, $7, CASE WHEN $7 = 'bounced' THEN 'Permanent' END,
                 now() - make_interval(secs => $8 * 3600), now() - make_interval(secs => $8 * 3600),
                 now() - make_interval(secs => $9 * 3600), now() - make_interval(secs => $10 * 3600),
                 CASE WHEN $11 THEN now() - make_interval(secs => $8 * 3600 - 60) END, 'acme.test')",
    )
    .bind(id)
    .bind(acct)
    .bind(m.profile)
    .bind(m.campaign)
    .bind(m.flow)
    .bind(&m.to)
    .bind(m.status)
    .bind(m.created_h)
    .bind(m.opened_h)
    .bind(m.clicked_h)
    .bind(m.unsubscribed)
    .execute(&h.state.db)
    .await
    .unwrap();
    id
}

async fn event(h: &Harness, acct: Uuid, profile: Uuid, metric: &str, value: Option<f64>, ago_h: f64, props: Value) {
    let metric_id = dumbmail::audience::metric_id(&h.state.db, acct, metric).await.unwrap();
    sqlx::query("INSERT INTO events (id, account_id, profile_id, metric_id, properties, value, occurred_at) VALUES ($1, $2, $3, $4, $5, $6, now() - make_interval(secs => $7 * 3600))")
        .bind(Uuid::now_v7())
        .bind(acct)
        .bind(profile)
        .bind(metric_id)
        .bind(props)
        .bind(value)
        .bind(ago_h)
        .execute(&h.state.db)
        .await
        .unwrap();
}

struct Data {
    acct: Uuid,
    campaign: Uuid,
    flow: Uuid,
    list: Uuid,
    profiles: Vec<Uuid>,
}

/// Ten people; one campaign to all of them two days ago (six opens, three clicks, one bounce, one
/// spam complaint after an open, one unsubscribe), a flow email to four, and five orders: $100 and $50 from campaign clickers, $40
/// from someone who only opened it, $20 from a flow clicker and $30 with no message.
async fn seed(h: &Harness) -> Data {
    let acct = account_id(h).await;
    let list: Uuid = Uuid::now_v7();
    sqlx::query("INSERT INTO lists (id, account_id, name) VALUES ($1, $2, 'Newsletter')").bind(list).bind(acct).execute(&h.state.db).await.unwrap();
    let domains = ["gmail.com", "gmail.com", "gmail.com", "gmail.com", "gmail.com", "yahoo.com", "yahoo.com", "yahoo.com", "acme.test", "outlook.com"];
    let mut profiles = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let p = profile(h, acct, &format!("p{i}@{d}"), json!({"country": if i < 6 { "US" } else { "CA" }, "tier": if i % 2 == 0 { "gold" } else { "silver" }})).await;
        sqlx::query("INSERT INTO list_members (list_id, profile_id, added_at) VALUES ($1, $2, now() - interval '3 days')").bind(list).bind(p).execute(&h.state.db).await.unwrap();
        profiles.push(p);
    }
    let c = campaign(h, acct, "Spring sale", json!({"include": [{"type": "list", "id": list}], "exclude": []})).await;
    let f = flow(h, acct, "Welcome").await;
    for (i, p) in profiles.iter().enumerate() {
        message(
            h,
            acct,
            Msg {
                profile: *p,
                to: format!("p{i}@{}", domains[i]),
                campaign: Some(c),
                flow: None,
                // p5 opened, then marked it as spam: still delivered.
                status: match i {
                    8 => "bounced",
                    5 => "complained",
                    _ => "delivered",
                },
                created_h: 48.0,
                opened_h: (i < 6).then_some(47.0),
                clicked_h: (i < 3).then_some(46.0),
                unsubscribed: i == 9,
            },
        )
        .await;
    }
    for (i, p) in profiles.iter().enumerate().take(4) {
        message(h, acct, Msg { profile: *p, to: format!("p{i}@gmail.com"), campaign: None, flow: Some(f), status: "delivered", created_h: 30.0, opened_h: None, clicked_h: (i == 3).then_some(29.0), unsubscribed: false }).await;
    }
    event(h, acct, profiles[0], "Placed Order", Some(100.0), 24.0, json!({})).await;
    event(h, acct, profiles[1], "Placed Order", Some(50.0), 24.0, json!({})).await;
    event(h, acct, profiles[4], "Placed Order", Some(40.0), 20.0, json!({})).await;
    event(h, acct, profiles[3], "Placed Order", Some(20.0), 10.0, json!({})).await;
    event(h, acct, profiles[7], "Placed Order", Some(30.0), 5.0, json!({})).await;
    Data { acct, campaign: c, flow: f, list, profiles }
}

fn close(a: &Value, b: f64) {
    let x = a.as_f64().unwrap_or_else(|| panic!("not a number: {a}"));
    assert!((x - b).abs() < 1e-6, "{x} != {b}");
}

async fn run(h: &mut Harness, spec: Value) -> Value {
    h.ok("POST", "/api/reports/run", Some(spec)).await
}

/// Make a saved report due, queue it and run the job. Returns the job's payload.
async fn due_now(h: &Harness, id: &str) -> Value {
    sqlx::query("UPDATE saved_reports SET next_run_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(id).execute(&h.state.db).await.unwrap();
    dumbmail::features::reports::saved::enqueue_due(&h.state).await.unwrap();
    let payload: Value = sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'reports_scheduled_send' ORDER BY id DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    h.drain().await;
    payload
}

fn report_emails(h: &Harness, subject: &str) -> Vec<dumbmail::providers::email::OutgoingEmail> {
    h.state.email.sent().into_iter().filter(|e| e.subject.starts_with(subject)).collect()
}

#[tokio::test]
async fn overview_custom_and_campaign_reports_agree() {
    let mut h = harness!();
    h.signup().await;
    let d = seed(&h).await;

    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    let t = &ov["totals"];
    close(&t["revenue"], 240.0);
    close(&t["attributed_revenue"], 210.0);
    close(&t["campaign_revenue"], 190.0);
    close(&t["flow_revenue"], 20.0);
    assert_eq!(t["attributed_orders"], 4);

    // All messages, no grouping: the overview's numbers.
    let all = run(&mut h, json!({"source": "messages", "measures": ["sent", "revenue", "conversions", "open_rate", "click_rate", "unsubscribe_rate", "bounce_rate"]})).await;
    let at = &all["totals"];
    assert_eq!(at["sent"], t["emails_sent"]);
    close(&at["revenue"], 210.0);
    assert_eq!(at["conversions"], 4);
    close(&at["open_rate"], t["open_rate"].as_f64().unwrap());
    close(&at["click_rate"], t["click_rate"].as_f64().unwrap());
    close(&at["unsubscribe_rate"], t["unsubscribe_rate"].as_f64().unwrap());
    close(&at["bounce_rate"], t["bounce_rate"].as_f64().unwrap());
    // The email opened and then marked as spam was delivered, everywhere.
    assert_eq!(t["emails_delivered"], 13);
    close(&t["open_rate"], 6.0 / 13.0);
    assert!(all["attribution"].as_str().unwrap().contains("5 days"));

    // Revenue source: store revenue and the split.
    let rev = run(&mut h, json!({"source": "revenue", "group_by": ["source"], "measures": ["revenue", "attributed_revenue", "conversions"]})).await;
    close(&rev["totals"]["revenue"], 240.0);
    close(&rev["totals"]["attributed_revenue"], 210.0);
    let by_source: Vec<(String, f64)> = rev["rows"].as_array().unwrap().iter().map(|r| (r["keys"][0].as_str().unwrap().to_string(), r["values"]["revenue"].as_f64().unwrap())).collect();
    assert!(by_source.contains(&("campaign".into(), 190.0)) && by_source.contains(&("flow".into(), 20.0)) && by_source.contains(&("none".into(), 30.0)), "{by_source:?}");

    // Per campaign: the campaign report's numbers.
    let report = h.ok("GET", &format!("/api/campaigns/{}/report", d.campaign), None).await;
    let s = &report["stats"];
    let camp = run(&mut h, json!({"source": "campaigns", "group_by": ["campaign"], "measures": ["delivered", "opens", "clicks", "open_rate", "click_rate", "conversions", "revenue"]})).await;
    let row = &camp["rows"][0];
    assert_eq!(row["labels"][0], "Spring sale");
    assert_eq!(row["values"]["delivered"], s["delivered"]);
    close(&row["values"]["open_rate"], s["open_rate"].as_f64().unwrap());
    close(&row["values"]["click_rate"], s["click_rate"].as_f64().unwrap());
    assert_eq!(row["values"]["conversions"], s["orders"]);
    close(&row["values"]["revenue"], s["revenue"].as_f64().unwrap());
    close(&row["values"]["revenue"], 190.0);

    // Per flow step: the flow report's numbers.
    let fr = h.ok("GET", &format!("/api/flows/{}/report", d.flow), None).await;
    let steps = run(&mut h, json!({"source": "flow_messages", "group_by": ["message"], "measures": ["sent", "click_rate", "revenue"]})).await;
    assert_eq!(steps["rows"][0]["labels"][0], "Welcome · Welcome email");
    close(&steps["rows"][0]["values"]["revenue"], 20.0);
    let flow_rev = fr["nodes"].as_array().and_then(|n| n.first()).map(|n| n["stats"]["revenue"].clone()).unwrap_or_else(|| fr["stats"]["revenue"].clone());
    close(&flow_rev, 20.0);

    // Ready-made campaign comparison agrees too, and every ready-made report runs.
    let list = h.ok("GET", "/api/reports", None).await;
    assert_eq!(list["builtin"].as_array().unwrap().len(), 8);
    for b in list["builtin"].as_array().unwrap() {
        let id = b["id"].as_str().unwrap();
        let r = h.ok("GET", &format!("/api/reports/{id}/run"), None).await;
        assert!(r["result"]["rows"].is_array(), "{id}: {r}");
        if id == "builtin:campaign_performance" {
            close(&r["result"]["rows"][0]["values"]["revenue"], 190.0);
        }
    }
    h.cleanup().await;
}

#[tokio::test]
async fn attribution_settings_preview_save_and_apply_everywhere() {
    let mut h = harness!();
    h.signup().await;
    let d = seed(&h).await;

    let s = h.ok("GET", "/api/reports/attribution", None).await;
    assert_eq!(s["email_open_window_hours"], 120);
    assert_eq!(s["sms_click_window_hours"], 24);
    assert_eq!(s["count_opens"], true);
    assert_eq!(s["model"], "last_touch");
    assert_eq!(s["conversion_metrics"], json!(["Placed Order"]));
    assert_eq!(s["is_default"], true);

    // Clicks only: the open-only $40 order loses its credit. Preview saves nothing.
    let p = h.ok("POST", "/api/reports/attribution/preview", Some(json!({"settings": {"count_opens": false}, "days": 30}))).await;
    close(&p["current"]["totals"]["attributed_revenue"], 210.0);
    close(&p["proposed"]["totals"]["attributed_revenue"], 170.0);
    close(&p["proposed"]["totals"]["campaign_revenue"], 150.0);
    assert_eq!(h.ok("GET", "/api/reports/attribution", None).await["count_opens"], true);

    h.ok("PATCH", "/api/reports/attribution", Some(json!({"count_opens": false}))).await;
    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    close(&ov["totals"]["attributed_revenue"], 170.0);
    let cr = h.ok("GET", &format!("/api/campaigns/{}/report", d.campaign), None).await;
    close(&cr["stats"]["revenue"], 150.0);
    let custom = run(&mut h, json!({"source": "campaigns", "measures": ["revenue"]})).await;
    close(&custom["totals"]["revenue"], 150.0);
    assert!(custom["attribution"].as_str().unwrap().contains("clicked an email"));

    // First touch: the flow clicker had opened the campaign earlier, so the campaign gets it.
    h.ok("PATCH", "/api/reports/attribution", Some(json!({"count_opens": true, "model": "first_touch"}))).await;
    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    close(&ov["totals"]["campaign_revenue"], 210.0);
    close(&ov["totals"]["flow_revenue"], 0.0);
    let fr = h.ok("GET", &format!("/api/flows/{}/report", d.flow), None).await;
    close(&fr["stats"]["revenue"], 0.0);

    // A shorter click window drops clicks from 46 hours before a 24-hours-ago order... (22h gap).
    h.ok("PATCH", "/api/reports/attribution", Some(json!({"model": "last_touch", "count_opens": false, "email_click_window_hours": 12}))).await;
    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    close(&ov["totals"]["attributed_revenue"], 0.0);

    // Two conversion metrics count; one is kept on the account too.
    h.ok("PATCH", "/api/reports/attribution", Some(json!({"email_click_window_hours": 120, "conversion_metrics": ["Placed Order", "Booked Call"]}))).await;
    event(&h, d.acct, d.profiles[2], "Booked Call", Some(15.0), 2.0, json!({})).await;
    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    close(&ov["totals"]["revenue"], 255.0);
    close(&ov["totals"]["campaign_revenue"], 165.0);

    // Bad values are refused.
    for bad in [json!({"sms_click_window_hours": 0}), json!({"email_open_window_hours": 721}), json!({"model": "linear"}), json!({"conversion_metrics": []}), json!({"nope": 1})] {
        let (st, _) = h.req("PATCH", "/api/reports/attribution", Some(bad.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}");
    }

    // Another account keeps the defaults.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let s = h.ok("GET", "/api/reports/attribution", None).await;
    assert_eq!(s["model"], "last_touch");
    assert_eq!(s["conversion_metrics"], json!(["Placed Order"]));
    h.cleanup().await;
}

#[tokio::test]
async fn custom_reports_group_filter_compare_save_and_schedule() {
    let mut h = harness!();
    h.signup().await;
    let d = seed(&h).await;

    // By day, gaps filled, in the account's time zone.
    let r = run(&mut h, json!({"source": "messages", "group_by": ["day"], "range": {"days": 7}, "measures": ["sent"], "compare": true})).await;
    assert!(r["rows"].as_array().unwrap().len() >= 7);
    assert_eq!(r["rows"].as_array().unwrap().iter().map(|r| r["values"]["sent"].as_i64().unwrap()).sum::<i64>(), 14);
    assert_eq!(r["previous"]["sent"], 0);
    assert_eq!(r["timezone"], "America/Los_Angeles");

    // Two groups, filters, provider-agnostic country.
    let r = run(&mut h, json!({"source": "campaigns", "group_by": ["country"], "measures": ["sent", "opens"], "filters": [{"field": "country", "values": ["CA"]}]})).await;
    assert_eq!(r["rows"].as_array().unwrap().len(), 1);
    assert_eq!(r["rows"][0]["keys"][0], "CA");
    assert_eq!(r["rows"][0]["values"]["sent"], 4);
    let r = run(&mut h, json!({"source": "campaigns", "group_by": ["list"], "measures": ["sent", "revenue"]})).await;
    assert_eq!(r["rows"][0]["labels"][0], "Newsletter");
    let r = run(&mut h, json!({"source": "messages", "group_by": ["weekday", "hour"], "measures": ["sent", "open_rate"]})).await;
    assert!(!r["rows"].as_array().unwrap().is_empty());
    let r = run(&mut h, json!({"source": "messages", "group_by": ["channel"], "filters": [{"field": "flow", "values": [d.flow.to_string()]}], "measures": ["sent"]})).await;
    assert_eq!(r["rows"][0]["labels"][0], "Email");
    assert_eq!(r["rows"][0]["values"]["sent"], 4);

    // Other sources.
    let r = run(&mut h, json!({"source": "metric", "metric": "Placed Order", "group_by": ["property"], "property": "missing", "measures": ["events", "value"]})).await;
    assert_eq!(r["totals"]["events"], 5);
    let r = run(&mut h, json!({"source": "customers", "measures": ["customers", "returning_customers", "repeat_purchase_rate"]})).await;
    assert_eq!(r["totals"]["customers"], 5);
    assert_eq!(r["totals"]["returning_customers"], 0);
    let r = run(&mut h, json!({"source": "list_growth", "group_by": ["list"], "measures": ["joined"]})).await;
    assert_eq!(r["rows"][0]["values"]["joined"], 10);
    let r = run(&mut h, json!({"source": "engagement"})).await;
    let tiers: Vec<&str> = r["rows"].as_array().unwrap().iter().map(|r| r["keys"][0].as_str().unwrap()).collect();
    assert_eq!(tiers, ["active_30", "active_90", "active_180", "at_risk", "unengaged", "not_emailed"]);
    assert_eq!(r["rows"][0]["values"]["profiles"], 6);
    assert_eq!(r["totals"]["profiles"], 10);

    // Bad specs are refused with a reason.
    for bad in [
        json!({"source": "nope"}),
        json!({"source": "campaigns", "measures": ["sent); DROP TABLE messages; --"]}),
        json!({"source": "campaigns", "group_by": ["flow"]}),
        json!({"source": "campaigns", "filters": [{"field": "campaign", "values": ["not-an-id"]}]}),
        json!({"source": "campaigns", "range": {"days": 5000}}),
        json!({"source": "campaigns", "range": {"start": "2026-02-01", "end": "2026-01-01"}}),
        json!({"source": "campaigns", "limit": 100000}),
    ] {
        let (st, v) = h.req("POST", "/api/reports/run", Some(bad.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }

    // CSV export of a report.
    let (st, headers, body) = {
        let cookie = h.cookie.clone().unwrap();
        h.raw("POST", "/api/reports/run.csv", &[("cookie", &cookie), ("content-type", "application/json")], &json!({"source": "campaigns", "group_by": ["campaign"], "measures": ["sent", "open_rate", "revenue"]}).to_string()).await
    };
    assert_eq!(st, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/csv"));
    assert_eq!(body, "Campaign,Sent,Open rate (%),Revenue\nSpring sale,10,66.67,190.00\nTotal,10,66.67,190.00\n");

    // Save, change, duplicate (also a ready-made one), delete.
    let saved = h.ok("POST", "/api/reports", Some(json!({"name": "Weekly revenue", "spec": {"source": "revenue", "group_by": ["week"]}}))).await;
    let id = saved["id"].as_str().unwrap().to_string();
    assert_eq!(saved["spec"]["measures"], json!(["conversions", "revenue", "attributed_revenue", "attributed_share"]));
    let up = h.ok("PATCH", &format!("/api/reports/{id}"), Some(json!({"name": "Revenue by week"}))).await;
    assert_eq!(up["name"], "Revenue by week");
    let copy = h.ok("POST", &format!("/api/reports/{id}/duplicate"), None).await;
    assert_eq!(copy["name"], "Revenue by week (copy)");
    let b = h.ok("POST", "/api/reports/builtin:engagement_tiers/duplicate", Some(json!({"name": "My tiers"}))).await;
    assert_eq!(b["spec"]["source"], "engagement");
    let (st, _) = h.req("DELETE", "/api/reports/builtin:engagement_tiers", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = h.req("DELETE", &format!("/api/reports/{}", copy["id"].as_str().unwrap()), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Schedules go only to members, then email a summary and a link to the CSV.
    let members = h.ok("GET", "/api/reports/members", None).await;
    let me = members["data"][0]["id"].as_str().unwrap().to_string();
    let (st, _) = h.req("PATCH", &format!("/api/reports/{id}"), Some(json!({"schedule": "weekly", "recipients": [Uuid::now_v7()]}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = h.req("PATCH", &format!("/api/reports/{id}"), Some(json!({"schedule": "daily", "recipients": [me]}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let s = h.ok("PATCH", &format!("/api/reports/{id}"), Some(json!({"schedule": "weekly", "recipients": [me]}))).await;
    assert_eq!(s["schedule"], "weekly");
    assert!(s["next_run_at"].is_string());
    // The owner is asked to confirm their address first; nothing is emailed until they do.
    assert_eq!(s["recipient_status"][0]["status"], "waiting");
    let asks: Vec<_> = h.state.email.sent().into_iter().filter(|e| e.subject.contains("Confirm your address")).collect();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].to, "owner@acme.test");
    let link = asks[0].text.split_whitespace().find(|w| w.contains("/report-emails/")).unwrap().to_string();
    let path = link.trim_start_matches("https://pl.test").to_string();
    due_now(&h, &id).await;
    assert_eq!(report_emails(&h, "Revenue by week").len(), 0);
    let waiting = h.ok("GET", &format!("/api/reports/{id}"), None).await;
    assert!(waiting["last_error"].as_str().unwrap().contains("confirm"), "{waiting}");
    // Saving the schedule again the same day doesn't email another link.
    h.ok("PATCH", &format!("/api/reports/{id}"), Some(json!({"schedule": "weekly", "recipients": [me]}))).await;
    assert_eq!(h.state.email.sent().iter().filter(|e| e.subject.contains("Confirm your address")).count(), 1);
    // Opening the link only shows a button; pressing it confirms.
    let (st, _, page) = h.raw("GET", &path, &[], "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("Yes, send them here") && page.contains("owner@acme.test"), "{page}");
    assert_eq!(h.ok("GET", "/api/reports/members", None).await["data"][0]["report_emails"], "waiting");
    let (st, _, page) = h.raw("POST", &path, &[], "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("You're set"), "{page}");
    assert_eq!(h.ok("GET", "/api/reports/members", None).await["data"][0]["report_emails"], "confirmed");
    let (st, _, _) = h.raw("GET", "/report-emails/rrnot-a-token.abc", &[], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let job_payload = due_now(&h, &id).await;
    dumbmail::features::reports::saved::enqueue_due(&h.state).await.unwrap(); // already moved forward: no second email
    h.drain().await;
    let sent = report_emails(&h, "Revenue by week");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "owner@acme.test");
    assert_eq!(sent[0].from_email, "help@pl.test");
    assert!(sent[0].text.contains("Revenue: $240.00"), "{}", sent[0].text);
    assert!(sent[0].html.contains("https://pl.test/analytics/exports"));
    // A worker that died after sending runs the same job again: nobody gets it twice, and there's one file.
    dumbmail::jobs::enqueue(&h.state.db, "reports_scheduled_send", job_payload, None).await.unwrap();
    h.drain().await;
    assert_eq!(report_emails(&h, "Revenue by week").len(), 1);
    let after = h.ok("GET", &format!("/api/reports/{id}"), None).await;
    assert!(after["last_sent_at"].is_string());
    assert!(after["last_error"].is_null(), "{after}");
    let files = h.ok("GET", "/api/reports/exports", None).await;
    assert_eq!(files["data"].as_array().unwrap().len(), 1);
    assert_eq!(files["data"][0]["kind"], "report");
    assert_eq!(files["data"][0]["status"], "done");

    // Another account sees none of it.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let (st, _) = h.req("GET", &format!("/api/reports/{id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = h.req("PATCH", &format!("/api/reports/{id}"), Some(json!({"name": "x"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(h.ok("GET", "/api/reports", None).await["saved"], json!([]));
    let r = run(&mut h, json!({"source": "revenue", "measures": ["revenue"]})).await;
    close(&r["totals"]["revenue"], 0.0);
    let (st, _) = h.req("GET", &format!("/api/reports/exports/{}", files["data"][0]["id"].as_str().unwrap()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn cohorts_and_funnels() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let a = profile(&h, acct, "a@x.test", json!({"plan": "pro"})).await;
    let b = profile(&h, acct, "b@x.test", json!({"plan": "free"})).await;
    let c = profile(&h, acct, "c@x.test", json!({})).await;
    // a: viewed, carted, ordered; b: viewed, carted; c: viewed. a ordered again 40 days later.
    for p in [a, b, c] {
        event(&h, acct, p, "Viewed Product", None, 24.0 * 50.0, json!({"source": "ig"})).await;
    }
    event(&h, acct, a, "Added to Cart", None, 24.0 * 50.0 - 1.0, json!({})).await;
    event(&h, acct, b, "Added to Cart", None, 24.0 * 50.0 - 2.0, json!({})).await;
    event(&h, acct, a, "Placed Order", Some(80.0), 24.0 * 50.0 - 3.0, json!({})).await;
    event(&h, acct, a, "Placed Order", Some(20.0), 24.0 * 10.0, json!({})).await;
    // b's order comes after the 7-day window.
    event(&h, acct, b, "Placed Order", Some(10.0), 24.0 * 30.0, json!({})).await;

    let f = h.ok("POST", "/api/reports/funnel", Some(json!({"steps": ["Viewed Product", "Added to Cart", "Placed Order"], "range": {"days": 60}}))).await;
    let people: Vec<i64> = f["total"].as_array().unwrap().iter().map(|s| s["people"].as_i64().unwrap()).collect();
    assert_eq!(people, [3, 2, 1]);
    close(&f["total"][1]["conversion_from_previous"], 2.0 / 3.0);
    assert_eq!(f["total"][2]["dropped"], 1);
    let f = h.ok("POST", "/api/reports/funnel", Some(json!({"steps": ["Viewed Product", "Added to Cart"], "range": {"days": 60}, "split_by": {"type": "profile_property", "key": "plan"}}))).await;
    assert_eq!(f["splits"].as_array().unwrap().len(), 3);
    let f = h.ok("POST", "/api/reports/funnel", Some(json!({"steps": ["Viewed Product", "Placed Order"], "range": {"days": 60}, "split_by": {"type": "source"}}))).await;
    assert_eq!(f["splits"][0]["label"], "No message");
    for bad in [json!({"steps": ["Viewed Product"]}), json!({"steps": ["a", "b"], "window_days": 0}), json!({"steps": ["a", "b"], "split_by": {"type": "property"}})] {
        let (st, _) = h.req("POST", "/api/reports/funnel", Some(bad.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}");
    }

    let co = h.ok("GET", "/api/reports/cohorts?months=3", None).await;
    let cohorts = co["cohorts"].as_array().unwrap();
    let total: i64 = cohorts.iter().map(|c| c["size"].as_i64().unwrap()).sum();
    assert_eq!(total, 2, "{co}");
    let a_cohort = cohorts.iter().find(|c| c["size"] == 1 && c["cells"].as_array().unwrap().iter().any(|x| x["customers"] == 1)).expect("a bought again");
    let last = a_cohort["cells"].as_array().unwrap().last().unwrap().clone();
    close(&last["cumulative_revenue_per_person"], 100.0);
    let (st, _, csv) = {
        let cookie = h.cookie.clone().unwrap();
        h.raw("GET", "/api/reports/cohorts?months=3&format=csv", &[("cookie", &cookie)], "").await
    };
    assert_eq!(st, StatusCode::OK);
    assert!(csv.starts_with("Cohort,People,Month 0 bought (%)"), "{csv}");
    let (st, _) = h.req("GET", "/api/reports/cohorts?months=99", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn deliverability_and_benchmarks() {
    let mut h = harness!();
    h.signup().await;
    seed(&h).await;
    let d = h.ok("GET", "/api/reports/deliverability?days=30", None).await;
    let get = |k: &str| d["providers"].as_array().unwrap().iter().find(|p| p["provider"] == k).unwrap().clone();
    assert_eq!(get("gmail")["sent"], 9);
    assert_eq!(get("yahoo")["sent"], 3);
    assert_eq!(get("outlook")["sent"], 1);
    assert_eq!(get("other")["sent"], 1);
    assert_eq!(get("other")["hard_bounces"], 1);
    assert_eq!(get("apple")["sent"], 0);
    assert_eq!(d["overall"]["sent"], 14);
    assert_eq!(d["overall"]["health"]["grade"], "not_enough_data");
    assert_eq!(d["sending_domains"][0]["domain"], "acme.test");
    assert!(!d["series"].as_array().unwrap().is_empty());
    let (st, _) = h.req("GET", "/api/reports/deliverability?days=0", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let b = h.ok("GET", "/api/reports/benchmarks?industry=ecommerce", None).await;
    assert_eq!(b["industry"]["label"], "E-commerce");
    assert_eq!(b["sources"]["engagement"]["published"], "2025-12-03");
    let open = b["metrics"].as_array().unwrap().iter().find(|m| m["key"] == "open_rate").unwrap();
    close(&open["industry"], 0.3267);
    assert_eq!(open["verdict"], "not_enough_data");
    let (st, _) = h.req("GET", "/api/reports/benchmarks?industry=nope", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // A real send records the sending domain.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Real"}))).await;
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nreal1@x.test\n", "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Real", "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let dom: Option<String> = sqlx::query_scalar("SELECT from_domain FROM messages WHERE to_address = 'real1@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(dom.as_deref(), Some("acme.test"));
    h.cleanup().await;
}

#[tokio::test]
async fn exports_write_files_members_can_download() {
    let mut h = harness!();
    h.signup().await;
    let d = seed(&h).await;
    sqlx::query("UPDATE profiles SET first_name = '=cmd|calc', email_consent = 'unsubscribed' WHERE id = $1").bind(d.profiles[9]).execute(&h.state.db).await.unwrap();

    let e = h.ok("POST", "/api/reports/exports", Some(json!({"kind": "profiles", "scope": "list", "list_id": d.list, "columns": ["email", "first_name", "email_consent"], "properties": ["tier"]}))).await;
    assert_eq!(e["status"], "queued");
    // One at a time.
    let (st, _) = h.req("POST", "/api/reports/exports", Some(json!({"kind": "suppressions"}))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    h.drain().await;
    let id = e["id"].as_str().unwrap().to_string();
    let done = h.ok("GET", &format!("/api/reports/exports/{id}"), None).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["row_count"], 10);
    let cookie = h.cookie.clone().unwrap();
    let (st, headers, body) = h.raw("GET", &format!("/api/reports/exports/{id}/download"), &[("cookie", &cookie)], "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(headers["content-disposition"].to_str().unwrap().contains("profiles-newsletter-"));
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines[0], "Email,First name,Email consent,tier");
    assert_eq!(lines.len(), 11);
    assert!(body.contains("p9@outlook.com,'=cmd|calc,unsubscribed,silver"), "{body}");
    // Signed out: no file.
    let (st, _, _) = h.raw("GET", &format!("/api/reports/exports/{id}/download"), &[], "").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    for (req, rows) in [
        (json!({"kind": "suppressions"}), 1),
        (json!({"kind": "events", "metric": "Placed Order"}), 5),
        (json!({"kind": "campaign_recipients", "campaign_id": d.campaign}), 10),
    ] {
        let e = h.ok("POST", "/api/reports/exports", Some(req.clone())).await;
        h.drain().await;
        let id = e["id"].as_str().unwrap();
        let done = h.ok("GET", &format!("/api/reports/exports/{id}"), None).await;
        assert_eq!(done["status"], "done", "{req}: {done}");
        assert_eq!(done["row_count"], rows, "{req}");
        if req["kind"] == "campaign_recipients" {
            let (_, _, body) = h.raw("GET", &format!("/api/reports/exports/{id}/download"), &[("cookie", &cookie)], "").await;
            assert!(body.lines().any(|l| l.starts_with(&d.profiles[0].to_string()) && l.ends_with(",1,100.00")), "{body}");
        }
    }
    for bad in [json!({"kind": "everything"}), json!({"kind": "profiles", "columns": ["password_hash"]}), json!({"kind": "profiles", "scope": "list"}), json!({"kind": "events"})] {
        let (st, _) = h.req("POST", "/api/reports/exports", Some(bad.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}");
    }
    // Expired files can't be downloaded.
    sqlx::query("UPDATE exports SET expires_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    let (st, _, _) = h.raw("GET", &format!("/api/reports/exports/{id}/download"), &[("cookie", &cookie)], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Another account can't see or download them, nor export someone else's list.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let other = h.cookie.clone().unwrap();
    let (st, _, _) = h.raw("GET", &format!("/api/reports/exports/{}/download", done["id"].as_str().unwrap()), &[("cookie", &other)], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(h.ok("GET", "/api/reports/exports", None).await["data"], json!([]));
    let (st, _) = h.req("POST", "/api/reports/exports", Some(json!({"kind": "profiles", "scope": "list", "list_id": d.list}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn reports_over_mcp() {
    let mut h = harness!();
    h.signup().await;
    seed(&h).await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Claude"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let call = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let mcp = async |name: &str, args: Value| -> Value {
        let (st, _, body) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &call(name, args)).await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        v["result"].clone()
    };
    let r = mcp("run_report", json!({"spec": {"source": "campaigns", "measures": ["revenue"]}})).await;
    assert_eq!(r["isError"], false, "{r}");
    close(&r["structuredContent"]["totals"]["revenue"], 190.0);
    let r = mcp("get_report", json!({"id": "builtin:revenue_by_source"})).await;
    assert_eq!(r["structuredContent"]["report"]["name"], "Revenue by channel and source");
    let r = mcp("update_attribution_settings", json!({"count_opens": false, "preview": true})).await;
    close(&r["structuredContent"]["proposed"]["totals"]["attributed_revenue"], 170.0);
    let r = mcp("get_attribution_settings", json!({})).await;
    assert_eq!(r["structuredContent"]["count_opens"], true);
    let r = mcp("save_report", json!({"name": "Weekly", "spec": {"source": "revenue"}, "schedule": "weekly", "recipients": []})).await;
    assert_eq!(r["structuredContent"]["dry_run"], true);
    let r = mcp("save_report", json!({"name": "Plain", "spec": {"source": "revenue"}})).await;
    let id = r["structuredContent"]["id"].as_str().unwrap().to_string();
    let r = mcp("delete_report", json!({"id": id})).await;
    assert_eq!(r["structuredContent"]["dry_run"], true);
    let r = mcp("delete_report", json!({"id": id, "confirm": true})).await;
    assert_eq!(r["structuredContent"]["deleted"], true);
    let r = mcp("deliverability_report", json!({"days": 30})).await;
    assert_eq!(r["structuredContent"]["overall"]["sent"], 14);
    let r = mcp("funnel_report", json!({"steps": ["Viewed Product", "Placed Order"]})).await;
    assert_eq!(r["isError"], false, "{r}");
    let r = mcp("cohort_report", json!({"months": 2})).await;
    assert_eq!(r["isError"], false, "{r}");
    let r = mcp("start_export", json!({"kind": "suppressions"})).await;
    let eid = r["structuredContent"]["id"].as_str().unwrap().to_string();
    let r = mcp("get_export", json!({"id": eid})).await;
    assert_eq!(r["structuredContent"]["kind"], "suppressions");
    let r = mcp("run_report", json!({"spec": {"source": "campaigns", "group_by": ["nope"]}})).await;
    assert_eq!(r["isError"], true);
    let (_, _, body) = h.raw("POST", "/mcp", &[("authorization", &auth), ("content-type", "application/json")], &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "dumbmail://docs/reports"}}).to_string()).await;
    assert!(body.contains("Report spec"), "{body}");
    h.cleanup().await;
}

// ---------- Review fixes ----------

use chrono::{DateTime, Duration, Utc};
use dumbmail::features::reports::query::{Mode, Run, Sql};

#[allow(clippy::too_many_arguments)]
async fn msg_at(
    h: &Harness,
    acct: Uuid,
    profile: Uuid,
    channel: &str,
    campaign: Option<Uuid>,
    flow: Option<Uuid>,
    status: &str,
    created: DateTime<Utc>,
    opened: Option<DateTime<Utc>>,
    clicked: Option<DateTime<Utc>>,
    is_test: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, status, created_at, sent_at, opened_at, clicked_at, is_test)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $6::uuid IS NULL THEN NULL ELSE 'e1' END, 'x@x.test', $7, $8, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(acct)
    .bind(profile)
    .bind(channel)
    .bind(campaign)
    .bind(flow)
    .bind(status)
    .bind(created)
    .bind(opened)
    .bind(clicked)
    .bind(is_test)
    .execute(&h.state.db)
    .await
    .unwrap();
    id
}

async fn event_at(h: &Harness, acct: Uuid, profile: Uuid, metric: &str, value: f64, at: DateTime<Utc>) -> Uuid {
    let metric_id = dumbmail::audience::metric_id(&h.state.db, acct, metric).await.unwrap();
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO events (id, account_id, profile_id, metric_id, properties, value, occurred_at) VALUES ($1, $2, $3, $4, '{}', $5, $6)")
        .bind(id)
        .bind(acct)
        .bind(profile)
        .bind(metric_id)
        .bind(value)
        .bind(at)
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

/// The attribution query on main before attribution settings existed, copied as it was.
const OLD_ATTRIBUTION_SQL: &str = "
    SELECT e.id AS event_id, COALESCE(e.value, 0) AS value, e.occurred_at, att.campaign_id, att.flow_id, att.flow_node_id, att.variant
    FROM events e
    JOIN metrics mt ON mt.id = e.metric_id
    JOIN accounts a ON a.id = e.account_id AND mt.name = a.conversion_metric
    LEFT JOIN LATERAL (
        SELECT m.campaign_id, m.flow_id, m.flow_node_id, m.variant FROM messages m
        WHERE m.profile_id = e.profile_id AND NOT m.is_test AND (
            (m.channel = 'email' AND GREATEST(m.opened_at, m.clicked_at) BETWEEN e.occurred_at - interval '5 days' AND e.occurred_at)
            OR (m.channel = 'sms' AND m.clicked_at BETWEEN e.occurred_at - interval '1 day' AND e.occurred_at))
        ORDER BY GREATEST(m.opened_at, m.clicked_at) DESC LIMIT 1
    ) att ON true
    WHERE e.account_id = $1";

type Credit = (Uuid, f64, Option<Uuid>, Option<Uuid>);

#[tokio::test]
async fn default_attribution_credits_what_the_old_rule_did() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let base = Utc::now() - Duration::hours(1);
    let t = |hours: f64| base - Duration::milliseconds((hours * 3_600_000.0) as i64);
    let mut p = Vec::new();
    for i in 0..10 {
        p.push(profile(&h, acct, &format!("q{i}@x.test"), json!({})).await);
    }
    let mut c = Vec::new();
    for i in 0..12 {
        c.push(campaign(&h, acct, &format!("C{i}"), json!({"include": []})).await);
    }
    let f1 = flow(&h, acct, "F1").await;
    let f2 = flow(&h, acct, "F2").await;
    // p0: a campaign click and a flow open at the same moment: the newer message wins (as before).
    msg_at(&h, acct, p[0], "email", Some(c[0]), None, "delivered", t(48.0), None, Some(t(10.0)), false).await;
    msg_at(&h, acct, p[0], "email", None, Some(f1), "delivered", t(30.0), Some(t(10.0)), None, false).await;
    // p1: an old open and a recent click on one email beat a flow open in between.
    msg_at(&h, acct, p[1], "email", Some(c[1]), None, "delivered", t(120.0), Some(t(100.0)), Some(t(2.0)), false).await;
    msg_at(&h, acct, p[1], "email", None, Some(f2), "delivered", t(20.0), Some(t(3.0)), None, false).await;
    // p2: a text click beats an older email open.
    msg_at(&h, acct, p[2], "sms", Some(c[2]), None, "delivered", t(6.0), None, Some(t(5.0)), false).await;
    msg_at(&h, acct, p[2], "email", Some(c[3]), None, "delivered", t(25.0), Some(t(20.0)), None, false).await;
    // p3: a text click outside its 24 hours loses to an email open inside 5 days.
    msg_at(&h, acct, p[3], "sms", Some(c[4]), None, "delivered", t(31.0), None, Some(t(30.0)), false).await;
    msg_at(&h, acct, p[3], "email", Some(c[5]), None, "delivered", t(55.0), Some(t(50.0)), None, false).await;
    // p4: emails opened and then bounced or marked as spam still count.
    msg_at(&h, acct, p[4], "email", Some(c[6]), None, "bounced", t(7.0), Some(t(5.0)), None, false).await;
    msg_at(&h, acct, p[4], "email", Some(c[7]), None, "complained", t(8.0), Some(t(6.0)), None, false).await;
    // p5: two texts clicked at the same moment: the newer one wins.
    msg_at(&h, acct, p[5], "sms", Some(c[8]), None, "delivered", t(40.0), None, Some(t(4.0)), false).await;
    msg_at(&h, acct, p[5], "sms", Some(c[9]), None, "delivered", t(20.0), None, Some(t(4.0)), false).await;
    // p6, the one disclosed change: opened before the purchase, clicked after it.
    msg_at(&h, acct, p[6], "email", Some(c[10]), None, "delivered", t(3.0), Some(t(2.0)), Some(base + Duration::hours(1)), false).await;
    msg_at(&h, acct, p[6], "email", Some(c[11]), None, "delivered", t(60.0), None, Some(t(50.0)), false).await;
    // p7: a test send never counts.
    msg_at(&h, acct, p[7], "email", Some(c[0]), None, "delivered", t(2.0), None, Some(t(1.0)), true).await;
    msg_at(&h, acct, p[7], "email", Some(c[1]), None, "delivered", t(70.0), Some(t(60.0)), None, false).await;
    // p8: a message recorded as created after the purchase but opened before it counts, as before.
    msg_at(&h, acct, p[8], "email", Some(c[2]), None, "delivered", base + Duration::hours(2), Some(t(1.0)), None, false).await;
    // p9: no messages.
    let mut disclosed = None;
    for (i, who) in p.iter().enumerate() {
        let e = event_at(&h, acct, *who, "Placed Order", 10.0 * (i as f64 + 1.0), base).await;
        if i == 6 {
            disclosed = Some(e);
        }
    }
    event_at(&h, acct, p[0], "Placed Order", 7.0, base + Duration::minutes(30)).await;
    let disclosed = disclosed.unwrap();

    // The old query, planned the way it runs on a big table: by messages_profile_idx.
    let mut tx = h.state.db.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off").execute(&mut *tx).await.unwrap();
    sqlx::query("SET LOCAL enable_bitmapscan = off").execute(&mut *tx).await.unwrap();
    let plan: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {OLD_ATTRIBUTION_SQL}")).bind(acct).fetch_all(&mut *tx).await.unwrap();
    assert!(plan.iter().any(|l| l.contains("messages_profile_idx")), "{plan:?}");
    let old: Vec<Credit> = sqlx::query_as(&format!("SELECT event_id, value::float8, campaign_id, flow_id FROM ({OLD_ATTRIBUTION_SQL}) x ORDER BY event_id"))
        .bind(acct)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let new: Vec<Credit> =
        sqlx::query_as(&format!("SELECT event_id, value::float8, campaign_id, flow_id FROM ({}) x ORDER BY event_id", dumbmail::features::reports::attribution::ATTRIBUTION_SQL))
            .bind(acct)
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(old.len(), 11);
    assert_eq!(new.len(), 11);
    for (o, n) in old.iter().zip(&new) {
        assert_eq!(o.0, n.0);
        if o.0 == disclosed {
            assert_eq!((o.2, n.2), (Some(c[11]), Some(c[10])), "the open before the purchase now gets the credit");
        } else {
            assert_eq!((o.2, o.3), (n.2, n.3), "event {} credited differently", o.0);
        }
    }
    // Bucket by bucket (campaign and flow), the same revenue, once the disclosed change is moved back.
    let buckets = |rows: &[Credit], fix: bool| {
        let mut m: std::collections::BTreeMap<String, f64> = Default::default();
        for r in rows {
            let key = if fix && r.0 == disclosed { Some(c[11]) } else { r.2 };
            *m.entry(format!("{key:?}/{:?}", r.3)).or_default() += r.1;
        }
        m
    };
    assert_eq!(buckets(&old, false), buckets(&new, true));
    let expect = [(0, None, Some(f1)), (1, Some(c[1]), None), (2, Some(c[2]), None), (3, Some(c[5]), None), (4, Some(c[6]), None), (5, Some(c[9]), None), (7, Some(c[1]), None), (8, Some(c[2]), None), (9, None, None)];
    for (i, camp, fl) in expect {
        let value = 10.0 * (i as f64 + 1.0);
        assert!(old.iter().any(|r| r.1 == value && r.2 == camp && r.3 == fl), "p{i}: {old:?}");
    }

    // The overview, custom and campaign reports agree with that.
    let ov = h.ok("GET", "/api/analytics/overview?days=30", None).await;
    let camp_rev: f64 = new.iter().filter(|r| r.2.is_some()).map(|r| r.1).sum();
    close(&ov["totals"]["campaign_revenue"], camp_rev);
    let custom = run(&mut h, json!({"source": "campaigns", "measures": ["revenue"]})).await;
    close(&custom["totals"]["revenue"], camp_rev);
    let rev = run(&mut h, json!({"source": "revenue", "measures": ["attributed_revenue"]})).await;
    close(&rev["totals"]["attributed_revenue"], ov["totals"]["attributed_revenue"].as_f64().unwrap());
    h.cleanup().await;
}

#[tokio::test]
async fn report_indexes_build_after_startup_and_replace_an_invalid_one() {
    use dumbmail::features::reports::indexes::{INDEXES, STATE_SQL};
    let mut h = harness!();
    h.signup().await;
    seed(&h).await;
    let pool = h.state.db.clone();
    let state_of = |name: &'static str| {
        let db = pool.clone();
        async move { sqlx::query_scalar::<_, bool>(STATE_SQL).bind(name).fetch_optional(&db).await.unwrap() }
    };
    // An interrupted concurrent build leaves an invalid index behind (here, a unique build over duplicates).
    assert!(sqlx::query("CREATE UNIQUE INDEX CONCURRENTLY messages_account_opened_idx ON messages (account_id)").execute(&h.state.db).await.is_err());
    assert_eq!(state_of("messages_account_opened_idx").await, Some(false));

    // A busy table makes the build wait; startup doesn't.
    let mut blocker = h.state.db.begin().await.unwrap();
    sqlx::query("LOCK TABLE messages IN SHARE MODE").execute(&mut *blocker).await.unwrap();
    let started = std::time::Instant::now();
    let task = dumbmail::features::spawn_after_start(h.state.clone());
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    // Reports work meanwhile, without the indexes.
    let r = run(&mut h, json!({"source": "messages", "measures": ["sent", "open_rate"]})).await;
    assert_eq!(r["totals"]["sent"], 14);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the build should be waiting for the lock");
    blocker.rollback().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(60), task).await.expect("index build finished").unwrap();

    for (name, _) in INDEXES {
        assert_eq!(state_of(name).await, Some(true), "{name}");
    }
    let def: String = sqlx::query_scalar("SELECT pg_get_indexdef('messages_account_opened_idx'::regclass)").fetch_one(&h.state.db).await.unwrap();
    assert!(def.contains("(account_id, opened_at)") && !def.contains("UNIQUE"), "{def}");
    // Another start finds them in place.
    dumbmail::features::reports::indexes::ensure(&h.state).await.unwrap();
    h.cleanup().await;
}

#[tokio::test]
async fn reports_run_one_at_a_time_per_account_within_one_deadline() {
    let mut h = harness!();
    h.signup().await;
    seed(&h).await;
    let acct = account_id(&h).await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Claude"}))).await["key"].as_str().unwrap().to_string();

    let held = Run::start(acct, Mode::Request).await.unwrap();
    for (method, path, body) in [
        ("POST", "/api/reports/run", Some(json!({"source": "campaigns"}))),
        ("GET", "/api/reports/builtin:campaign_performance/run", None),
        ("GET", "/api/reports/cohorts", None),
        ("POST", "/api/reports/funnel", Some(json!({"steps": ["Viewed Product", "Placed Order"]}))),
        ("GET", "/api/reports/deliverability", None),
        ("GET", "/api/reports/benchmarks", None),
        ("POST", "/api/reports/attribution/preview", Some(json!({"settings": {"count_opens": false}}))),
    ] {
        let (st, v) = h.req(method, path, body).await;
        assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{path}: {v}");
        assert_eq!(v["error"]["message"], "A report is already running for this account. Wait for it to finish, then try again.");
    }
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run_report", "arguments": {"spec": {"source": "campaigns"}}}}).to_string();
    let (_, _, body) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &call).await;
    assert!(body.contains("already running"), "{body}");
    // A scheduled send that finds one running is tried again later rather than skipped.
    let report = h.ok("POST", "/api/reports", Some(json!({"name": "R", "spec": {"source": "campaigns"}}))).await;
    let rid: Uuid = report["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE saved_reports SET schedule = 'weekly', schedule_recipients = ARRAY(SELECT user_id FROM memberships WHERE account_id = $2) WHERE id = $1")
        .bind(rid)
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO report_recipients (account_id, user_id, id, email, confirmed_at) SELECT $1, u.id, $2, u.email, now() FROM users u JOIN memberships m ON m.user_id = u.id WHERE m.account_id = $1")
        .bind(acct)
        .bind(Uuid::now_v7())
        .execute(&h.state.db)
        .await
        .unwrap();
    assert!(dumbmail::features::reports::saved::send_scheduled(&h.state, rid, Utc::now()).await.is_err());
    let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM report_deliveries").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(delivered, 0);
    drop(held);
    run(&mut h, json!({"source": "campaigns"})).await;

    // Every statement of a run shares one deadline.
    let run = Run::background(std::time::Duration::from_millis(1500)).await.unwrap();
    let mut tx = run.tx(&h.state).await.unwrap();
    let mut q = Sql::new(acct);
    q.push("SELECT pg_sleep(1), $1::uuid");
    tx.fetch_all(&q).await.unwrap();
    let e = tx.fetch_all(&q).await.unwrap_err();
    assert_eq!(e.public_message(), "This report took too long to run. Try a shorter date range or fewer groups.");
    h.cleanup().await;
}

#[tokio::test]
async fn reports_compute_only_what_they_show() {
    let mut h = harness!();
    h.signup().await;
    seed(&h).await;
    // With conversions locked away, a report of sends still runs: it never works out attribution.
    let mut lock = h.state.db.begin().await.unwrap();
    sqlx::query("LOCK TABLE events IN ACCESS EXCLUSIVE MODE").execute(&mut *lock).await.unwrap();
    let started = std::time::Instant::now();
    let r = run(&mut h, json!({"source": "campaigns", "group_by": ["campaign"], "measures": ["sent", "open_rate", "click_rate"], "compare": true})).await;
    assert_eq!(r["totals"]["sent"], 10);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    lock.rollback().await.unwrap();
    // Revenue alone still gets it right.
    let r = run(&mut h, json!({"source": "messages", "measures": ["revenue"]})).await;
    close(&r["totals"]["revenue"], 210.0);
    let r = run(&mut h, json!({"source": "revenue", "measures": ["revenue", "conversions"]})).await;
    close(&r["totals"]["revenue"], 240.0);
    h.cleanup().await;
}

#[tokio::test]
async fn grouped_reports_sort_before_the_cap_and_count_everyone() {
    let mut h = harness!();
    h.signup().await;
    let d = seed(&h).await;
    let acct = d.acct;

    // A campaign grouped by list and by segment: each column is its own.
    let seg = Uuid::now_v7();
    sqlx::query("INSERT INTO segments (id, account_id, name, definition) VALUES ($1, $2, 'VIPs', '{}')").bind(seg).bind(acct).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE campaigns SET audience = $2 WHERE id = $1")
        .bind(d.campaign)
        .bind(json!({"include": [{"type": "list", "id": d.list}, {"type": "segment", "id": seg}], "exclude": []}))
        .execute(&h.state.db)
        .await
        .unwrap();
    let r = run(&mut h, json!({"source": "messages", "group_by": ["list", "segment"], "measures": ["sent"]})).await;
    let rows = r["rows"].as_array().unwrap();
    let camp = rows.iter().find(|r| r["keys"][0] == d.list.to_string()).unwrap_or_else(|| panic!("{r}"));
    assert_eq!(camp["keys"][1], seg.to_string());
    assert_eq!(camp["labels"], json!(["Newsletter", "VIPs"]));
    assert_eq!(camp["values"]["sent"], 10);
    // The flow's messages have no audience: "(none)", not dropped.
    let flow_row = rows.iter().find(|r| r["keys"][0] == "").unwrap();
    assert_eq!(flow_row["labels"], json!(["(none)", "(none)"]));
    assert_eq!(flow_row["values"]["sent"], 4);

    // More groups than the cap: sorted first, so the top one is in.
    let metric = dumbmail::audience::metric_id(&h.state.db, acct, "Viewed Product").await.unwrap();
    sqlx::query(
        "INSERT INTO events (id, account_id, profile_id, metric_id, properties, value, occurred_at)
         SELECT gen_random_uuid(), $1, $2, $3, jsonb_build_object('sku', 'sku-' || g), 1, now() - interval '1 hour' FROM generate_series(1, 100000) g",
    )
    .bind(acct)
    .bind(d.profiles[0])
    .bind(metric)
    .execute(&h.state.db)
    .await
    .unwrap();
    event(&h, acct, d.profiles[1], "Viewed Product", Some(1000.0), 1.0, json!({"sku": "winner"})).await;
    event(&h, acct, d.profiles[1], "Viewed Product", Some(1.0), 1.0, json!({"sku": "x".repeat(1000)})).await;
    let r = run(&mut h, json!({"source": "metric", "metric": "Viewed Product", "group_by": ["property"], "property": "sku", "measures": ["value"], "sort": {"by": "value", "desc": true}, "limit": 1})).await;
    assert_eq!(r["rows"][0]["keys"][0], "winner", "{}", r["rows"]);
    assert_eq!(r["truncated"], true);
    close(&r["totals"]["value"], 101_001.0);
    // Long values are cut.
    let r = run(&mut h, json!({"source": "metric", "metric": "Viewed Product", "group_by": ["property"], "property": "sku", "measures": ["events"], "sort": {"by": "group", "desc": true}, "limit": 1})).await;
    assert_eq!(r["rows"][0]["keys"][0].as_str().unwrap(), "x".repeat(200));

    // A funnel with more splits than it shows still counts everyone.
    let signup = dumbmail::audience::metric_id(&h.state.db, acct, "Signed Up").await.unwrap();
    sqlx::query(
        "WITH p AS (INSERT INTO profiles (id, account_id, email, email_consent) SELECT gen_random_uuid(), $1, 'f' || g || '@x.test', 'subscribed' FROM generate_series(1, 600) g RETURNING id, email)
         INSERT INTO events (id, account_id, profile_id, metric_id, properties, occurred_at) SELECT gen_random_uuid(), $1, p.id, $2, jsonb_build_object('src', p.email), now() - interval '2 hours' FROM p",
    )
    .bind(acct)
    .bind(signup)
    .execute(&h.state.db)
    .await
    .unwrap();
    let f = h.ok("POST", "/api/reports/funnel", Some(json!({"steps": ["Signed Up", "Placed Order"], "split_by": {"type": "event_property", "key": "src"}}))).await;
    assert_eq!(f["total"][0]["people"], 600);
    let splits = f["splits"].as_array().unwrap();
    assert_eq!(splits.len(), 10);
    assert_eq!(splits[9]["label"], "Other");
    assert_eq!(splits[9]["steps"][0]["people"], 591);
    assert_eq!(splits.iter().map(|s| s["steps"][0]["people"].as_i64().unwrap()).sum::<i64>(), 600);
    h.cleanup().await;
}

/// Run an export's batches by hand with small limits until it finishes.
async fn export_with(h: &mut Harness, req: Value, limits: dumbmail::features::reports::exports::Limits) -> Value {
    let e = h.ok("POST", "/api/reports/exports", Some(req)).await;
    let id: Uuid = e["id"].as_str().unwrap().parse().unwrap();
    for _ in 0..200 {
        sqlx::query("DELETE FROM jobs WHERE kind = 'reports_export'").execute(&h.state.db).await.unwrap();
        dumbmail::features::reports::exports::run_batch_with(&h.state, id, limits).await.unwrap();
        let v = h.ok("GET", &format!("/api/reports/exports/{id}"), None).await;
        if v["status"] == "done" || v["status"] == "failed" {
            return v;
        }
    }
    panic!("export never finished");
}

#[tokio::test]
async fn exports_stay_within_their_byte_limits() {
    use dumbmail::features::reports::exports::Limits;
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    for i in 0..20 {
        profile(&h, acct, &format!("big{i:02}@x.test"), json!({"blob": "b".repeat(20_000), "tier": "gold", "junk": "j".repeat(200_000)})).await;
    }
    // Chunks stop at their size, and the file has every row once (a repeated column counts once).
    let done = export_with(&mut h, json!({"kind": "profiles", "columns": ["email", "email"], "properties": ["blob"]}), Limits { max_bytes: 10_000_000, chunk_bytes: 50_000 }).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["row_count"], 20);
    let id: Uuid = done["id"].as_str().unwrap().parse().unwrap();
    let sizes: Vec<i32> = sqlx::query_scalar("SELECT length(bytes) FROM export_chunks WHERE export_id = $1 ORDER BY seq").bind(id).fetch_all(&h.state.db).await.unwrap();
    assert!(sizes.len() >= 6, "{sizes:?}");
    assert!(sizes.iter().all(|s| *s <= 50_000 + 21_000), "{sizes:?}");
    let cookie = h.cookie.clone().unwrap();
    let (_, _, body) = h.raw("GET", &format!("/api/reports/exports/{id}/download"), &[("cookie", &cookie)], "").await;
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines[0], "Email,blob");
    assert_eq!(lines.len(), 21);
    assert!(!body.contains("jjjj"), "only the chosen properties are exported");

    // Past the file limit: stopped with a reason, nothing kept.
    let failed = export_with(&mut h, json!({"kind": "profiles", "columns": ["email"], "properties": ["blob"]}), Limits { max_bytes: 100_000, chunk_bytes: 50_000 }).await;
    assert_eq!(failed["status"], "failed");
    assert!(failed["error"].as_str().unwrap().contains("over 100 MB"), "{failed}");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM export_chunks WHERE export_id = $1::uuid").bind(failed["id"].as_str().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 0);

    // The big property that isn't asked for doesn't count toward the limit.
    let small = export_with(&mut h, json!({"kind": "profiles", "columns": ["email"], "properties": ["tier"]}), Limits { max_bytes: 10_000, chunk_bytes: 50_000 }).await;
    assert_eq!(small["status"], "done", "{small}");
    assert_eq!(small["row_count"], 20);
    h.cleanup().await;
}

#[tokio::test]
async fn recording_the_sender_domain_is_part_of_marking_a_message_sent() {
    let mut h = harness!();
    h.signup().await;
    // Refuse any later update that writes only the sender domain, as a failing second statement would.
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_lone_sender_update() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.from_domain IS DISTINCT FROM OLD.from_domain AND NEW.status = OLD.status THEN RAISE EXCEPTION 'separate from_domain update'; END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER refuse_lone_sender_update BEFORE UPDATE ON messages FOR EACH ROW EXECUTE FUNCTION refuse_lone_sender_update();",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Real"}))).await;
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": "email\nreal1@x.test\n", "list_id": list["id"], "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Real", "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]},
        "content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let (status, dom): (String, Option<String>) = sqlx::query_as("SELECT status, from_domain FROM messages WHERE to_address = 'real1@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "sent");
    assert_eq!(dom.as_deref(), Some("acme.test"));
    let received: i64 = sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Received Email'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(received, 1);
    let camp = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_ne!(camp["status"], "failed", "{camp}");
    h.cleanup().await;
}

#[tokio::test]
async fn mx_lookups_are_queued_once() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let tries = (0..12).map(|_| dumbmail::features::reports::delivery::queue_mx_lookup(&h.state, acct));
    for r in futures_util::future::join_all(tries).await {
        r.unwrap();
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'reports_mx_lookup'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn only_confirmed_addresses_get_scheduled_reports() {
    let mut h = harness!();
    h.signup().await;
    seed(&h).await;
    let acct = account_id(&h).await;
    // A teammate who signs in with Google has shown they own the address; one with a password hasn't.
    let add = |email: &'static str, google: Option<&'static str>| {
        let db = h.state.db.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO users (id, email, name, password_hash, google_sub) VALUES ($1, $2, NULL, NULL, $3)").bind(id).bind(email).bind(google).execute(&db).await.unwrap();
            sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, 'member')").bind(id).bind(acct).execute(&db).await.unwrap();
            id
        }
    };
    let ann = add("ann@acme.test", Some("google-ann")).await;
    let bob = add("bob@unowned.test", None).await;
    let members = h.ok("GET", "/api/reports/members", None).await;
    let status = |id: Uuid| members["data"].as_array().unwrap().iter().find(|m| m["id"] == id.to_string()).unwrap()["report_emails"].clone();
    assert_eq!(status(ann), "confirmed");
    assert_eq!(status(bob), "not_asked");

    let r = h.ok("POST", "/api/reports", Some(json!({"name": "Team report", "spec": {"source": "campaigns"}, "schedule": "monthly", "recipients": [ann, bob]}))).await;
    let id = r["id"].as_str().unwrap().to_string();
    let st: Vec<(String, String)> = r["recipient_status"].as_array().unwrap().iter().map(|x| (x["email"].as_str().unwrap().into(), x["status"].as_str().unwrap().into())).collect();
    assert!(st.contains(&("ann@acme.test".into(), "confirmed".into())) && st.contains(&("bob@unowned.test".into(), "waiting".into())), "{st:?}");
    let asks: Vec<_> = h.state.email.sent().into_iter().filter(|e| e.subject.contains("Confirm your address")).collect();
    assert_eq!(asks.iter().map(|e| e.to.as_str()).collect::<Vec<_>>(), ["bob@unowned.test"]);
    // The confirmation email carries no account-written text.
    assert!(!asks[0].text.contains("Acme Goods"));

    due_now(&h, &id).await;
    let sent = report_emails(&h, "Team report");
    assert_eq!(sent.iter().map(|e| e.to.as_str()).collect::<Vec<_>>(), ["ann@acme.test"]);

    // An expired link can't confirm.
    sqlx::query("UPDATE report_recipients SET asked_at = now() - interval '8 days' WHERE user_id = $1").bind(bob).execute(&h.state.db).await.unwrap();
    let link = asks[0].text.split_whitespace().find(|w| w.contains("/report-emails/")).unwrap().trim_start_matches("https://pl.test").to_string();
    let (_, _, page) = h.raw("POST", &link, &[], "").await;
    assert!(page.contains("expired"), "{page}");
    let members = h.ok("GET", "/api/reports/members", None).await;
    assert_eq!(members["data"].as_array().unwrap().iter().find(|m| m["id"] == bob.to_string()).unwrap()["report_emails"], "waiting");
    h.cleanup().await;
}

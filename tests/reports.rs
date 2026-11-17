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
/// unsubscribe), a flow email to four, and five orders: $100 and $50 from campaign clickers, $40
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
                status: if i == 8 { "bounced" } else { "delivered" },
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
    sqlx::query("UPDATE saved_reports SET next_run_at = now() - interval '1 minute' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    dumbmail::features::reports::saved::enqueue_due(&h.state).await.unwrap();
    dumbmail::features::reports::saved::enqueue_due(&h.state).await.unwrap(); // already moved forward: no second email
    h.drain().await;
    let sent: Vec<_> = h.state.email.sent().into_iter().filter(|e| e.subject.starts_with("Revenue by week")).collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "owner@acme.test");
    assert_eq!(sent[0].from_email, "help@pl.test");
    assert!(sent[0].text.contains("Revenue: $240.00"), "{}", sent[0].text);
    assert!(sent[0].html.contains("https://pl.test/analytics/exports"));
    let after = h.ok("GET", &format!("/api/reports/{id}"), None).await;
    assert!(after["last_sent_at"].is_string());
    assert!(after["next_run_at"].as_str().unwrap() > s["next_run_at"].as_str().unwrap() || after["next_run_at"] == s["next_run_at"]);
    let files = h.ok("GET", "/api/reports/exports", None).await;
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

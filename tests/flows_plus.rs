//! Flow tools: delays until a time or date, trigger splits, notification steps, back-populate,
//! per-step results and the waiting list.

#[macro_use]
mod common;

use axum::http::StatusCode;
use chrono::{Datelike, Duration, Utc};
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

async fn live_flow(h: &mut Harness, trigger: Value, definition: Value) -> String {
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": trigger, "definition": definition}))).await;
    let id = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
    id
}

async fn event(h: &mut Harness, metric: &str, email: &str, props: Value) {
    h.ok("POST", "/api/events", Some(json!({"metric": metric, "profile": {"email": email}, "properties": props}))).await;
}

async fn run_of(h: &Harness, flow: &str, email: &str) -> (Uuid, String, Option<String>, Value) {
    sqlx::query_as("SELECT r.id, r.status, r.current_node, r.context FROM flow_runs r JOIN profiles p ON p.id = r.profile_id WHERE r.flow_id = $1::uuid AND p.email = $2")
        .bind(flow)
        .bind(email)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn prop(h: &Harness, email: &str, k: &str) -> Option<String> {
    sqlx::query_scalar("SELECT properties->>$2 FROM profiles WHERE email = $1").bind(email).bind(k).fetch_one(&h.state.db).await.unwrap()
}

async fn job_at(h: &Harness, run: Uuid) -> chrono::DateTime<Utc> {
    sqlx::query_scalar("SELECT run_at FROM jobs WHERE kind = 'flow_step' AND payload->>'run_id' = $1::text ORDER BY id DESC LIMIT 1").bind(run).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn delays_until_a_date_in_the_event() {
    let mut h = harness!();
    h.signup().await;
    // Wait until 2 days before check-in at 10:00, then mark the profile.
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Booked"}), json!({"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 0, "unit": "days", "until": {"field": "event.check_in", "offset_amount": 2, "offset_unit": "days", "offset_direction": "before"}, "at": "10:00", "next": "u"},
        {"id": "u", "type": "update_profile", "properties": {"reminded": "yes"}, "next": null}]})).await;
    let check_in = (Utc::now() + Duration::days(20)).with_timezone(&chrono_tz::America::Los_Angeles).date_naive();
    event(&mut h, "Booked", "future@x.test", json!({"check_in": check_in.to_string()})).await;
    event(&mut h, "Booked", "past@x.test", json!({"check_in": "2020-01-01"})).await;
    event(&mut h, "Booked", "nodate@x.test", json!({})).await;
    h.drain().await;

    let (run, status, node, ctx) = run_of(&h, &fid, "future@x.test").await;
    assert_eq!((status.as_str(), node.as_deref()), ("active", Some("d")));
    let day = check_in - Duration::days(2);
    let want = chrono::TimeZone::with_ymd_and_hms(&chrono_tz::America::Los_Angeles, day.year(), day.month(), day.day(), 10, 0, 0).earliest().unwrap().with_timezone(&Utc);
    let until: chrono::DateTime<Utc> = serde_json::from_value(ctx["_wait_until"].clone()).unwrap();
    assert_eq!(until.with_timezone(&chrono_tz::America::Los_Angeles).date_naive(), check_in - Duration::days(2), "{ctx}");
    assert_eq!(until, want);
    assert_eq!(job_at(&h, run).await, until);
    // "Before" offsets that are already past exit by default; so does a missing date.
    assert_eq!(run_of(&h, &fid, "past@x.test").await.1, "exited");
    assert_eq!(run_of(&h, &fid, "nodate@x.test").await.1, "exited");

    // Woken early (a paused flow, a fast-forward): it keeps waiting.
    h.fast_forward().await;
    let (_, status, node, _) = run_of(&h, &fid, "future@x.test").await;
    assert_eq!((status.as_str(), node.as_deref()), ("active", Some("d")));
    assert_eq!(prop(&h, "future@x.test", "reminded").await, None);
    assert_eq!(job_at(&h, run).await, until, "rescheduled for the same moment");
    // When the time comes it goes on.
    sqlx::query("UPDATE flow_runs SET context = jsonb_set(context, '{_wait_until}', to_jsonb((now() - interval '1 minute')::text)) WHERE id = $1").bind(run).execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    assert_eq!(run_of(&h, &fid, "future@x.test").await.1, "completed");
    assert_eq!(prop(&h, "future@x.test", "reminded").await.as_deref(), Some("yes"));
    h.cleanup().await;
}

#[tokio::test]
async fn plain_delays_store_what_they_always_did() {
    let mut h = harness!();
    h.signup().await;
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Ping"}), json!({"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 2, "unit": "hours", "next": null}]})).await;
    let before = Utc::now();
    event(&mut h, "Ping", "plain@x.test", json!({})).await;
    h.drain().await;
    let (run, _, node, ctx) = run_of(&h, &fid, "plain@x.test").await;
    assert_eq!(node.as_deref(), Some("d"));
    let keys: Vec<&String> = ctx.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["_waited", "event"], "{ctx}");
    let at = job_at(&h, run).await;
    assert!(at >= before + Duration::hours(2) && at <= Utc::now() + Duration::hours(2));
    // A fast-forward ends a plain delay, as before.
    h.fast_forward().await;
    assert_eq!(run_of(&h, &fid, "plain@x.test").await.1, "completed");
    h.cleanup().await;
}

#[tokio::test]
async fn weekdays_and_time_of_day_are_saved_and_checked() {
    let mut h = harness!();
    h.signup().await;
    for bad in [json!({"at": "9am"}), json!({"weekdays": ["someday"]}), json!({"until": {"field": "check_in"}}), json!({"until": {"field": "event.x", "if_past": "later"}})] {
        let mut node = json!({"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null});
        for (k, v) in bad.as_object().unwrap() {
            node[k] = v.clone();
        }
        let (s, v) = h.req("POST", "/api/flows", Some(json!({"name": "x", "trigger": {"type": "metric", "metric": "M"}, "definition": {"entry": "d", "nodes": [node]}}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    // Tue-Thu at 09:30 in the person's own zone (their profile says New York).
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Signed"}), json!({"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "minutes", "weekdays": ["tue", "wed", "thu"], "at": "09:30", "next": null}]})).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "ny@x.test", "properties": {"timezone": "America/New_York"}}))).await;
    event(&mut h, "Signed", "ny@x.test", json!({})).await;
    h.drain().await;
    let (_, _, _, ctx) = run_of(&h, &fid, "ny@x.test").await;
    let until: chrono::DateTime<Utc> = serde_json::from_value(ctx["_wait_until"].clone()).unwrap();
    let local = until.with_timezone(&chrono_tz::America::New_York);
    assert_eq!(local.format("%H:%M").to_string(), "09:30", "{local}");
    assert!(matches!(local.weekday(), chrono::Weekday::Tue | chrono::Weekday::Wed | chrono::Weekday::Thu), "{local}");
    assert!(until > Utc::now() && until < Utc::now() + Duration::days(8));
    h.cleanup().await;
}

#[tokio::test]
async fn trigger_split_reads_the_event() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Booked Stay"}), json!({"entry": "c", "nodes": [
        {"id": "c", "type": "condition", "condition": {"match": "all", "conditions": []},
         "event_condition": {"match": "all", "filters": [{"field": "properties.nights", "op": "gte", "value": 3}, {"field": "from_browser", "op": "eq", "value": false}]},
         "yes": "a", "no": "b"},
        {"id": "a", "type": "update_profile", "properties": {"path": "long"}, "next": null},
        {"id": "b", "type": "update_profile", "properties": {"path": "short"}, "next": null}]})).await;
    event(&mut h, "Booked Stay", "long@x.test", json!({"nights": 4})).await;
    event(&mut h, "Booked Stay", "short@x.test", json!({"nights": 1})).await;
    // The same event from the website script is untrusted: it fails from_browser = false.
    let (s, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Booked Stay", "profile": {"email": "forged@x.test"}, "properties": {"nights": 9}}).to_string()).await;
    assert!(s.is_success(), "{b}");
    h.drain().await;
    assert_eq!(prop(&h, "long@x.test", "path").await.as_deref(), Some("long"));
    assert_eq!(prop(&h, "short@x.test", "path").await.as_deref(), Some("short"));
    assert_eq!(prop(&h, "forged@x.test", "path").await.as_deref(), Some("short"));
    let (s, _) = h.req("POST", "/api/flows", Some(json!({"name": "x", "trigger": {"type": "metric", "metric": "M"}, "definition": {"entry": "c", "nodes": [
        {"id": "c", "type": "condition", "condition": {"conditions": []}, "event_condition": {"filters": [{"field": "nights", "op": "eq", "value": 1}]}, "yes": null, "no": null}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let trust = h.ok("GET", "/api/flows-plus/metric-trust?metric=Booked%20Stay", None).await;
    assert_eq!(trust["browser_can_send"], true);
    let trust = h.ok("GET", "/api/flows-plus/metric-trust?metric=Price%20Dropped", None).await;
    assert_eq!(trust["browser_can_send"], false);
    assert!(fid.len() > 10);
    h.cleanup().await;
}

fn notify_flow(to: Value, extra: Value) -> Value {
    let mut node = json!({"id": "n", "type": "notify", "to": to, "subject": "New lead: {{ person.email }}", "body": "Nights: {{ event.properties.nights }}\n\nCall them.", "next": null});
    for (k, v) in extra.as_object().unwrap() {
        node[k] = v.clone();
    }
    json!({"entry": "n", "nodes": [node]})
}

fn notes_to(h: &Harness, to: &str) -> Vec<dumbmail::providers::email::OutgoingEmail> {
    h.state.email.sent().into_iter().filter(|e| e.to == to && e.subject.starts_with("New lead")).collect()
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// Confirm `email` through the link it was sent (the newest one).
async fn confirm_link(h: &Harness, email: &str) {
    let asks: Vec<_> = h.state.email.sent().into_iter().filter(|e| e.to == email && e.subject.starts_with("Confirm notifications")).collect();
    let text = &asks.last().unwrap_or_else(|| panic!("no confirmation email to {email}")).text;
    let link = &text[text.find("/flow-notify/confirm?t=").unwrap()..];
    let token = link.split_whitespace().next().unwrap().trim_start_matches("/flow-notify/confirm?t=").to_string();
    let (s, _, page) = h.raw("GET", &format!("/flow-notify/confirm?t={token}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("Confirm notifications") && page.contains(email));
    let (s, _, page) = h.raw("POST", "/flow-notify/confirm", &[("content-type", "application/x-www-form-urlencoded")], &format!("t={token}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("You're set"), "{page}");
    let (_, _, again) = h.raw("POST", "/flow-notify/confirm", &[("content-type", "application/x-www-form-urlencoded")], &format!("t={token}")).await;
    assert!(again.contains("doesn't work"), "a link works once");
}

fn confirm_emails(h: &Harness, to: &str) -> usize {
    h.state.email.sent().into_iter().filter(|e| e.to == to && e.subject.starts_with("Confirm notifications")).count()
}

#[tokio::test]
async fn notification_steps_reach_the_team_and_confirmed_addresses_once() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    // Limits on the step.
    for (to, extra) in [
        (json!(["a@x.test", "b@x.test", "c@x.test", "d@x.test", "e@x.test", "f@x.test"]), json!({})),
        (json!([]), json!({})),
        (json!(["owner@acme.test"]), json!({"body": "Your code {% coupon_code 'WELCOME' %}"})),
        (json!(["owner@acme.test"]), json!({"subject": ""})),
        (json!(["not-an-address"]), json!({})),
    ] {
        let (s, v) = h.req("POST", "/api/flows", Some(json!({"name": "x", "trigger": {"type": "metric", "metric": "Lead"}, "definition": notify_flow(to.clone(), extra.clone())}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{to} {extra}: {v}");
    }
    let mut def = notify_flow(json!(["owner@acme.test", "pm@outside.test"]), json!({}));
    def["settings"] = json!({"notify_daily_cap": 3});
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Lead"}), def).await;
    h.drain().await;
    // Saving only recorded the outside address: nobody was emailed.
    assert_eq!(confirm_emails(&h, "pm@outside.test"), 0, "saving a flow never emails a stranger");
    let st = h.ok("GET", "/api/flows-plus/notify-addresses?emails=owner@acme.test,pm@outside.test,new@outside.test", None).await;
    let status = |e: &str| st["statuses"].as_array().unwrap().iter().find(|s| s["email"] == e).unwrap()["status"].as_str().unwrap().to_string();
    assert_eq!((status("owner@acme.test").as_str(), status("pm@outside.test").as_str(), status("new@outside.test").as_str()), ("team", "unconfirmed", "new"));

    event(&mut h, "Lead", "guest1@x.test", json!({"nights": 3})).await;
    h.drain().await;
    let owner = notes_to(&h, "owner@acme.test");
    assert_eq!(owner.len(), 1);
    assert_eq!(owner[0].subject, "New lead: guest1@x.test");
    assert!(owner[0].text.contains("Nights: 3") && owner[0].text.contains("set up a notification"), "{}", owner[0].text);
    assert!(owner[0].unsubscribe_url.is_none() && !owner[0].html.contains("/u/"), "no unsubscribe footer");
    assert_eq!(owner[0].from_email, "hello@acme.test");
    assert!(notes_to(&h, "pm@outside.test").is_empty(), "unconfirmed addresses get nothing");

    // Asking it to confirm is its own action: a dry run first, then confirm: true.
    let dry = h.ok("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "pm@outside.test"}))).await;
    assert_eq!(dry["dry_run"], true);
    h.drain().await;
    assert_eq!(confirm_emails(&h, "pm@outside.test"), 0);
    h.ok("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "pm@outside.test", "confirm": true}))).await;
    h.drain().await;
    assert_eq!(confirm_emails(&h, "pm@outside.test"), 1);
    // One a day per address.
    let (s, _) = h.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "pm@outside.test", "confirm": true}))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    confirm_link(&h, "pm@outside.test").await;

    event(&mut h, "Lead", "guest2@x.test", json!({"nights": 5})).await;
    h.drain().await;
    assert_eq!(notes_to(&h, "owner@acme.test").len(), 2);
    assert_eq!(notes_to(&h, "pm@outside.test").len(), 1);

    // A replayed step (a job run again after a crash) sends nothing twice.
    let (run, ..) = run_of(&h, &fid, "guest2@x.test").await;
    sqlx::query("UPDATE flow_runs SET status = 'active', current_node = 'n', finished_at = NULL WHERE id = $1").bind(run).execute(&h.state.db).await.unwrap();
    dumbmail::flows::step(&h.state, run).await.unwrap();
    assert_eq!(notes_to(&h, "owner@acme.test").len(), 2);
    assert_eq!(notes_to(&h, "pm@outside.test").len(), 1);

    // A run started from the website script gets no notification (the step didn't opt in).
    let (s, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Lead", "profile": {"email": "bot@x.test"}, "properties": {"nights": 1}}).to_string()).await;
    assert!(s.is_success(), "{b}");
    h.drain().await;
    let skipped: Vec<String> = sqlx::query_scalar("SELECT n.error FROM flow_notifications n JOIN flow_runs r ON r.id = n.run_id JOIN profiles p ON p.id = r.profile_id WHERE p.email = 'bot@x.test'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(skipped, vec!["started by a website event"; 2]);

    // The daily cap (3 here) holds: one more lead fills it, later ones are skipped.
    for i in 3..6 {
        event(&mut h, "Lead", &format!("guest{i}@x.test"), json!({"nights": 1})).await;
    }
    h.drain().await;
    let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_notifications WHERE flow_id = $1::uuid AND status = 'sent'").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(sent, 3);
    let capped: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_notifications WHERE flow_id = $1::uuid AND error = 'daily limit reached'").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(capped, 6);
    let (s, _) = h.req("POST", "/api/flows", Some(json!({"name": "x", "trigger": {"type": "metric", "metric": "Lead"}, "definition": {"entry": null, "nodes": [], "settings": {"notify_daily_cap": 5000}}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Results count them per step.
    let r = h.ok("GET", &format!("/api/flows/{fid}/results"), None).await;
    let n = &r["steps"][0];
    assert_eq!(n["notifications"]["sent"], 3, "{r}");
    // One unconfirmed, two from the website, six over the cap.
    assert_eq!(n["notifications"]["skipped"], 9, "{r}");

    // Removing the address stops it.
    h.ok("POST", "/api/flows-plus/notify-addresses/remove", Some(json!({"email": "pm@outside.test"}))).await;
    assert_eq!(h.ok("GET", "/api/flows-plus/notify-addresses?emails=pm@outside.test", None).await["statuses"][0]["status"], "removed");
    event(&mut h, "Lead", "guest9@x.test", json!({"nights": 1})).await;
    h.drain().await;
    assert_eq!(notes_to(&h, "pm@outside.test").len(), 1, "removed addresses get nothing");
    h.cleanup().await;
}

#[tokio::test]
async fn confirmation_emails_need_settings_rights_and_respect_pause_and_limits() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    // An API key that can edit flows but not account settings.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "flows", "scopes": ["messaging"]}))).await["key"].as_str().unwrap().to_string();
    let bearer = |path: &str, body: Value| {
        axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    };
    use tower::ServiceExt;
    let res = h.router.clone().oneshot(bearer("/api/flows-plus/notify-addresses", json!({"email": "a@outside.test"}))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "recording needs only flow editing");
    let res = h.router.clone().oneshot(bearer("/api/flows-plus/notify-addresses/send-confirmation", json!({"email": "a@outside.test", "confirm": true}))).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "emailing an outside address needs account settings");

    // Refused while sending is paused.
    sqlx::query("UPDATE accounts SET sending_paused_at = now() WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    let (s, _) = h.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "a@outside.test", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    sqlx::query("UPDATE accounts SET sending_paused_at = NULL WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();

    // Asked, then paused before it went out: checked again at the handoff, so nothing is sent.
    h.ok("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "a@outside.test", "confirm": true}))).await;
    sqlx::query("UPDATE accounts SET sending_paused_at = now() WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(confirm_emails(&h, "a@outside.test"), 0);
    sqlx::query("UPDATE accounts SET sending_paused_at = NULL WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();

    // Each account's first request to an address goes, so another account can't block it; repeats
    // within a day are refused, from either account.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "else@other.test", "password": "correct horse", "company": "Other"}))).await;
    other.ok("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "a@outside.test", "confirm": true}))).await;
    let (s, v) = other.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "a@outside.test", "confirm": true}))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    let (s, v) = h.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "a@outside.test", "confirm": true}))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    // However many accounts ask, an address gets at most 3 a day.
    for i in 0..3 {
        let mut x = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
        x.ok("POST", "/api/auth/signup", Some(json!({"email": format!("x{i}@many.test"), "password": "correct horse", "company": format!("Many {i}")}))).await;
        let (s, v) = x.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "a@outside.test", "confirm": true}))).await;
        assert_eq!(s, if i == 0 { StatusCode::OK } else { StatusCode::TOO_MANY_REQUESTS }, "account {i}: {v}");
    }

    // Two requests racing for the same address and account: exactly one is recorded.
    let (a, b) = tokio::join!(
        dumbmail::features::flows_plus::notify::send_confirmation_request(&h.state, acct, "race@outside.test", true),
        dumbmail::features::flows_plus::notify::send_confirmation_request(&h.state, acct, "race@outside.test", true),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "{a:?} {b:?}");
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_notify_confirm_sends WHERE email = 'race@outside.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(recorded, 1);

    // 50 a day per account.
    for i in 0..60 {
        sqlx::query("INSERT INTO flow_notify_confirm_sends (account_id, email) VALUES ($1, $2)").bind(acct).bind(format!("filler{i}@outside.test")).execute(&h.state.db).await.unwrap();
    }
    let (s, v) = h.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "late@outside.test", "confirm": true}))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    // Asking a removed address again counts against the 20 waiting.
    // Waiting now: a@, race@ and late@. 16 more makes 19; removing race@ makes 18; two more fill it.
    for i in 0..16 {
        h.ok("POST", "/api/flows-plus/notify-addresses", Some(json!({"email": format!("wait{i}@outside.test")}))).await;
    }
    h.ok("POST", "/api/flows-plus/notify-addresses/remove", Some(json!({"email": "race@outside.test"}))).await;
    for i in 16..18 {
        h.ok("POST", "/api/flows-plus/notify-addresses", Some(json!({"email": format!("wait{i}@outside.test")}))).await;
    }
    let (s, _) = h.req("POST", "/api/flows-plus/notify-addresses", Some(json!({"email": "one-too-many@outside.test"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    sqlx::query("UPDATE flow_notify_confirm_sends SET sent_at = sent_at - interval '2 days'").execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", "/api/flows-plus/notify-addresses/send-confirmation", Some(json!({"email": "race@outside.test", "confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    h.cleanup().await;
}

#[tokio::test]
async fn notifications_count_against_the_daily_allowance_and_stop_after_a_bounce() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Lead"}), notify_flow(json!(["owner@acme.test"]), json!({}))).await;
    // The account's daily allowance is used up: the step waits, nothing is sent.
    sqlx::query("UPDATE accounts SET email_daily_limit_override = 0 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::deliverability::forget_daily_limit(&h.state, acct);
    event(&mut h, "Lead", "g1@x.test", json!({"nights": 1})).await;
    h.drain().await;
    assert!(notes_to(&h, "owner@acme.test").is_empty());
    let (run, status, node, _) = run_of(&h, &fid, "g1@x.test").await;
    assert_eq!((status.as_str(), node.as_deref()), ("active", Some("n")));
    assert!(job_at(&h, run).await > Utc::now(), "it waits for the allowance");
    let pending: String = sqlx::query_scalar("SELECT status FROM flow_notifications WHERE run_id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(pending, "pending");
    // Room again: it goes out, once.
    sqlx::query("UPDATE accounts SET email_daily_limit_override = NULL WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::deliverability::forget_daily_limit(&h.state, acct);
    h.fast_forward().await;
    assert_eq!(notes_to(&h, "owner@acme.test").len(), 1);
    let counted: i64 = sqlx::query_scalar("SELECT count(*) FROM email_sends WHERE account_id = $1 AND counts_toward_limit").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(counted, 1, "it counts toward the warm-up limit");

    // The provider reports a hard bounce for it: the address gets no more.
    let pid: String = sqlx::query_scalar("SELECT provider_message_id FROM flow_notifications WHERE run_id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
    let ses = |m: Value| json!({"Type": "Notification", "Message": m.to_string()}).to_string();
    let (s, _, _) = h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &ses(json!({"eventType": "Bounce", "mail": {"messageId": pid}, "bounce": {"bounceType": "Permanent", "bounceSubType": "General"}}))).await;
    assert!(s.is_success());
    let st = h.ok("GET", "/api/flows-plus/notify-addresses?emails=owner@acme.test", None).await;
    assert_eq!(st["statuses"][0]["status"], "bounced", "{st}");
    event(&mut h, "Lead", "g2@x.test", json!({"nights": 1})).await;
    h.drain().await;
    assert_eq!(notes_to(&h, "owner@acme.test").len(), 1);
    let why: String = sqlx::query_scalar("SELECT n.error FROM flow_notifications n JOIN flow_runs r ON r.id = n.run_id JOIN profiles p ON p.id = r.profile_id WHERE p.email = 'g2@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(why, "address bounced or complained");

    // A Manager can't clear a bounce; an Admin or the owner can.
    let inv = h.ok("POST", "/api/team/invites", Some(json!({"email": "mgr@acme.test", "role": "manager"}))).await;
    assert_eq!(inv["email_sent"], true, "{inv}");
    let invite = h.state.email.sent().into_iter().rev().find(|e| e.to == "mgr@acme.test").unwrap().text;
    let token = invite[invite.find("/invite/").unwrap() + "/invite/".len()..].split_whitespace().next().unwrap().to_string();
    let mut mgr = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    mgr.ok("POST", &format!("/api/invites/{token}/accept"), Some(json!({"name": "Mgr", "password": "member pass 1"}))).await;
    let (s, v) = mgr.req("POST", "/api/flows-plus/notify-addresses/clear-bounce", Some(json!({"email": "owner@acme.test"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    h.ok("POST", "/api/flows-plus/notify-addresses/clear-bounce", Some(json!({"email": "owner@acme.test"}))).await;
    event(&mut h, "Lead", "g3@x.test", json!({"nights": 1})).await;
    h.drain().await;
    assert_eq!(notes_to(&h, "owner@acme.test").len(), 2);

    // A complaint blocks it for good.
    let pid: String = sqlx::query_scalar("SELECT n.provider_message_id FROM flow_notifications n JOIN flow_runs r ON r.id = n.run_id JOIN profiles p ON p.id = r.profile_id WHERE p.email = 'g3@x.test'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    h.raw("POST", "/webhooks/ses/hooksecret", &[("content-type", "text/plain")], &ses(json!({"notificationType": "Complaint", "mail": {"messageId": pid}, "complaint": {}}))).await;
    let st = h.ok("GET", "/api/flows-plus/notify-addresses?emails=owner@acme.test", None).await;
    assert_eq!(st["statuses"][0]["status"], "complained");
    let (s, _) = h.req("POST", "/api/flows-plus/notify-addresses/clear-bounce", Some(json!({"email": "owner@acme.test"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn notification_recipients_are_fixed_the_first_time_and_handoffs_are_never_repeated() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    // Two confirmed outside addresses; the step emails the one in the person's `manager` property.
    for e in ["first@outside.test", "second@outside.test"] {
        h.ok("POST", "/api/flows-plus/notify-addresses", Some(json!({"email": e}))).await;
    }
    sqlx::query("UPDATE flow_notify_addresses SET confirmed_at = now()").execute(&h.state.db).await.unwrap();
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Lead"}), notify_flow(json!([]), json!({"to_property": "manager"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "g@x.test", "properties": {"manager": "first@outside.test"}}))).await;
    sqlx::query("UPDATE accounts SET email_daily_limit_override = 0 WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::deliverability::forget_daily_limit(&h.state, acct);
    event(&mut h, "Lead", "g@x.test", json!({"nights": 2})).await;
    h.drain().await;
    // The property changes before the retry: the stored recipient is used.
    sqlx::query("UPDATE profiles SET properties = '{\"manager\": \"second@outside.test\"}' WHERE email = 'g@x.test'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE accounts SET email_daily_limit_override = NULL WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    dumbmail::deliverability::forget_daily_limit(&h.state, acct);
    h.fast_forward().await;
    let sent_to: Vec<String> = h.state.email.sent().into_iter().filter(|e| e.subject.starts_with("New lead")).map(|e| e.to).collect();
    assert_eq!(sent_to, vec!["first@outside.test"]);

    // A handoff cut off before its result was recorded: never sent again, marked unconfirmed, billed.
    let (run, ..) = run_of(&h, &fid, "g@x.test").await;
    sqlx::query("UPDATE flow_notifications SET status = 'handing', handed_at = now() - interval '1 hour', provider_message_id = NULL, cost_micros = NULL WHERE run_id = $1")
        .bind(run)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE flow_runs SET status = 'active', current_node = 'n', finished_at = NULL WHERE id = $1").bind(run).execute(&h.state.db).await.unwrap();
    dumbmail::flows::step(&h.state, run).await.unwrap();
    dumbmail::features::flows_plus::notify::sweep(&h.state).await.unwrap();
    let (status, cost): (String, Option<i64>) = sqlx::query_as("SELECT status, cost_micros FROM flow_notifications WHERE run_id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "unconfirmed");
    assert!(cost.is_some(), "billed, since it may have gone out");
    assert_eq!(h.state.email.sent().into_iter().filter(|e| e.subject.starts_with("New lead")).count(), 1);
    let r = h.ok("GET", &format!("/api/flows/{fid}/results"), None).await;
    assert_eq!(r["steps"][0]["notifications"]["unconfirmed"], 1, "{r}");
    h.cleanup().await;
}

#[tokio::test]
async fn back_populate_segment_flows_once() {
    let mut h = harness!();
    h.signup().await;
    for i in 0..3 {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("gold{i}@x.test"), "properties": {"tier": "gold"}}))).await;
    }
    h.ok("POST", "/api/profiles", Some(json!({"email": "silver@x.test", "properties": {"tier": "silver"}}))).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Gold", "definition": {"match": "all", "conditions": [{"type": "property", "field": "tier", "op": "eq", "value": "gold"}]}}))).await;
    let sid = seg["id"].as_str().unwrap();
    let def = json!({"entry": "d", "settings": {"reentry": "never"}, "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]});
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "Gold", "trigger": {"type": "segment", "segment_id": sid}, "definition": def}))).await;
    let fid = f["id"].as_str().unwrap().to_string();
    // Not live yet: counting is fine, starting isn't.
    let dry = h.ok("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({}))).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["would_add"].as_i64()), (Some(true), Some(3)), "{dry}");
    let (s, _) = h.req("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({"confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.drain().await;
    // Turning it on started nobody (they were already in the segment).
    let runs = |h: &Harness| {
        let db = h.state.db.clone();
        let fid = fid.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM flow_runs WHERE flow_id = $1::uuid").bind(fid).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(runs(&h).await, 0);

    let started = h.ok("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({"confirm": true}))).await;
    assert_eq!(started["status"], "running");
    // One at a time per flow, enforced by the insert.
    let (s, _) = h.req("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({"confirm": true}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.drain().await;
    assert_eq!(runs(&h).await, 3);
    let list = h.ok("GET", &format!("/api/flows/{fid}/backfills"), None).await;
    assert_eq!((list["data"][0]["status"].as_str(), list["data"][0]["added"].as_i64(), list["data"][0]["total"].as_i64()), (Some("done"), Some(3), Some(3)), "{list}");
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'flow.backfill_start'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(audited, 1);

    // A batch run again after a crash adds nobody twice: undo the done marks and replay.
    let bid: Uuid = started["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE flow_backfills SET status = 'running', finished_at = NULL WHERE id = $1").bind(bid).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE flow_backfill_members SET done_at = NULL WHERE backfill_id = $1").bind(bid).execute(&h.state.db).await.unwrap();
    dumbmail::jobs::enqueue_background(&h.state.db, "flows_plus_backfill", json!({"backfill_id": bid}), None).await.unwrap();
    h.drain().await;
    assert_eq!(runs(&h).await, 3);

    // A second back-populate finds nobody new (re-entry "never").
    let dry = h.ok("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({}))).await;
    assert_eq!(dry["would_add"], 0, "{dry}");

    // Cancelling stops it before anyone is added.
    h.ok("POST", "/api/profiles", Some(json!({"email": "gold9@x.test", "properties": {"tier": "gold"}}))).await;
    let b2 = h.ok("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({"confirm": true}))).await;
    h.ok("POST", &format!("/api/flows/{fid}/backfills/{}/cancel", b2["id"].as_str().unwrap()), None).await;
    h.drain().await;
    assert_eq!(runs(&h).await, 3);

    // Metric flows can't be back-populated.
    let mf = h.ok("POST", "/api/flows", Some(json!({"name": "M", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": def}))).await;
    let (s, v) = h.req("POST", &format!("/api/flows/{}/backfill", mf["id"].as_str().unwrap()), Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("past events"), "{v}");

    // Another account can't see or start it.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "else@other.test", "password": "correct horse", "company": "Other"}))).await;
    for (m, p) in [("POST", format!("/api/flows/{fid}/backfill")), ("GET", format!("/api/flows/{fid}/backfills")), ("GET", format!("/api/flows/{fid}/results"))] {
        let (s, _) = other.req(m, &p, Some(json!({"confirm": true}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {p}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn date_and_metric_flows_are_not_back_populated() {
    let mut h = harness!();
    h.signup().await;
    let def = json!({"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]});
    for (trigger, words) in [
        (json!({"type": "date_property", "field": "birthday", "offset_days": -7}), "people whose date is still ahead will enter on their own"),
        (json!({"type": "metric", "metric": "Placed Order"}), "past events don't start flows"),
    ] {
        let fid = live_flow(&mut h, trigger, def.clone()).await;
        for body in [json!({}), json!({"confirm": true})] {
            let (s, v) = h.req("POST", &format!("/api/flows/{fid}/backfill"), Some(body)).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
            assert!(v.to_string().contains(words), "{v}");
        }
    }
    let started: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_backfills").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(started, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn results_waiting_list_and_removing_people() {
    let mut h = harness!();
    h.signup().await;
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Joined"}), json!({"entry": "e", "nodes": [
        {"id": "e", "type": "email", "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}, "next": "d"},
        {"id": "d", "type": "delay", "amount": 3, "unit": "days", "next": "e2"},
        {"id": "e2", "type": "email", "content": {"subject": "Again", "blocks": [{"type": "text", "html": "<p>Again</p>"}]}, "next": null}]})).await;
    for i in 0..5 {
        event(&mut h, "Joined", &format!("j{i}@x.test"), json!({})).await;
    }
    h.drain().await;
    let r = h.ok("GET", &format!("/api/flows/{fid}/results?days=7"), None).await;
    let step = |id: &str| r["steps"].as_array().unwrap().iter().find(|s| s["node_id"] == id).unwrap().clone();
    assert_eq!(step("e")["reached"], 5, "{r}");
    assert_eq!(step("e")["messages"]["sent"], 5, "{r}");
    assert_eq!(step("d")["waiting"], 5);
    assert_eq!(step("e2")["reached"], 0);
    assert_eq!(step("e2")["messages"]["sent"], 0);

    // The waiting list pages.
    let p1 = h.ok("GET", &format!("/api/flows/{fid}/steps/d/waiting?limit=3"), None).await;
    assert_eq!(p1["data"].as_array().unwrap().len(), 3);
    let after = p1["next_after"].as_str().unwrap();
    let p2 = h.ok("GET", &format!("/api/flows/{fid}/steps/d/waiting?limit=3&after={after}"), None).await;
    assert_eq!(p2["data"].as_array().unwrap().len(), 2);
    assert!(p2["next_after"].is_null());
    assert!(p1["data"][0]["email"].as_str().unwrap().ends_with("@x.test"));

    // Removing needs confirm; then they get nothing more.
    let ids: Vec<Value> = p1["data"].as_array().unwrap().iter().map(|d| d["run_id"].clone()).collect();
    let dry = h.ok("POST", &format!("/api/flows/{fid}/runs/remove"), Some(json!({"run_ids": ids}))).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["would_remove"].as_i64()), (Some(true), Some(3)));
    let done = h.ok("POST", &format!("/api/flows/{fid}/runs/remove"), Some(json!({"run_ids": ids, "confirm": true}))).await;
    assert_eq!(done["removed"], 3);
    h.fast_forward().await;
    let again: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE flow_id = $1::uuid AND flow_node_id = 'e2'").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(again, 2);

    // A read-only key reads results but can't take people out or back-populate.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let call = |method: &str, path: String, body: Option<Value>| {
        let mut b = axum::http::Request::builder().method(method).uri(path).header("authorization", format!("Bearer {key}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        b.body(axum::body::Body::from(body.map(|v| v.to_string()).unwrap_or_default())).unwrap()
    };
    use tower::ServiceExt;
    let st = |r: axum::http::Request<axum::body::Body>| {
        let router = h.router.clone();
        async move { router.oneshot(r).await.unwrap().status() }
    };
    assert_eq!(st(call("GET", format!("/api/flows/{fid}/results"), None)).await, StatusCode::OK);
    assert_eq!(st(call("POST", format!("/api/flows/{fid}/runs/remove"), Some(json!({"run_ids": [ids[0]], "confirm": true})))).await, StatusCode::FORBIDDEN);
    assert_eq!(st(call("POST", format!("/api/flows/{fid}/backfill"), Some(json!({})))).await, StatusCode::FORBIDDEN);

    // The MCP tool gives the same results.
    let full = h.ok("POST", "/api/api-keys", Some(json!({"name": "all"}))).await["key"].as_str().unwrap().to_string();
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {full}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_flow_results", "arguments": {"flow_id": fid}}}).to_string()))
        .unwrap();
    let res = h.router.clone().oneshot(req).await.unwrap();
    let body = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(!v["result"]["isError"].as_bool().unwrap_or(false), "{v}");
    assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("\"node_id\""), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_wait_keeps_its_moment_when_the_person_moves() {
    let mut h = harness!();
    h.signup().await;
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Keep"}), json!({"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "days", "at": "10:00", "next": null}]})).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "mover@x.test", "properties": {"timezone": "America/New_York"}}))).await;
    event(&mut h, "Keep", "mover@x.test", json!({})).await;
    h.drain().await;
    let until = |ctx: &Value| serde_json::from_value::<chrono::DateTime<Utc>>(ctx["_wait_until"].clone()).unwrap();
    let (run, _, _, ctx) = run_of(&h, &fid, "mover@x.test").await;
    let ny = until(&ctx);
    assert_eq!(ny.with_timezone(&chrono_tz::America::New_York).format("%H:%M").to_string(), "10:00");
    assert!(ctx.get("_wait_from").is_none(), "{ctx}");
    // They move to Los Angeles and the job wakes early: the moment worked out on arrival is kept.
    sqlx::query("UPDATE profiles SET properties = properties || '{\"timezone\": \"America/Los_Angeles\"}' WHERE email = 'mover@x.test'").execute(&h.state.db).await.unwrap();
    h.fast_forward().await;
    let (_, status, _, ctx) = run_of(&h, &fid, "mover@x.test").await;
    assert_eq!(status, "active");
    assert_eq!(until(&ctx), ny);
    assert_eq!(job_at(&h, run).await, ny);
    // The option to move it is gone: saving it is ignored.
    let f = h.ok("POST", "/api/flows", Some(json!({"name": "x", "trigger": {"type": "metric", "metric": "M"}, "definition": {"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "days", "at": "10:00", "recompute_zone": true, "next": null}]}}))).await;
    assert!(f["id"].is_string());
    h.cleanup().await;
}

// ---------- Races and the cost of entering a flow ----------

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// A webhook receiver that holds each request until released, and counts them.
struct Hook {
    url: String,
    hits: Arc<AtomicUsize>,
    arrived: Arc<tokio::sync::Semaphore>,
    go: Arc<tokio::sync::Semaphore>,
}

async fn hook_server() -> Hook {
    dumbmail::features::webhooks::net::allow_localhost_for_tests();
    let hits = Arc::new(AtomicUsize::new(0));
    let arrived = Arc::new(tokio::sync::Semaphore::new(0));
    let go = Arc::new(tokio::sync::Semaphore::new(0));
    let (h2, a2, g2) = (hits.clone(), arrived.clone(), go.clone());
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move || {
            let (h, a, g) = (h2.clone(), a2.clone(), g2.clone());
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                a.add_permits(1);
                if let Ok(p) = g.acquire().await {
                    p.forget();
                }
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/hook", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Hook { url, hits, arrived, go }
}

#[tokio::test]
async fn removing_someone_and_sending_to_them_cant_both_happen() {
    let mut h = harness!();
    h.signup().await;
    let hook = hook_server().await;
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Hook"}), json!({"entry": "w", "nodes": [
        {"id": "w", "type": "webhook", "url": hook.url, "next": "u"},
        {"id": "u", "type": "update_profile", "properties": {"after": "yes"}, "next": null}]})).await;

    // A worker is mid-send (the webhook is waiting): removal leaves that run alone and says so.
    event(&mut h, "Hook", "busy@x.test", json!({})).await;
    let state = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&state).await });
    hook.arrived.acquire().await.unwrap().forget();
    let (run, ..) = run_of(&h, &fid, "busy@x.test").await;
    let r = h.ok("POST", &format!("/api/flows/{fid}/runs/remove"), Some(json!({"run_ids": [run], "confirm": true}))).await;
    assert_eq!(r["removed"], 0, "{r}");
    assert_eq!(r["sending_now"], json!([run]), "{r}");
    hook.go.add_permits(1);
    worker.await.unwrap().unwrap();
    assert_eq!(run_of(&h, &fid, "busy@x.test").await.1, "completed");

    // Something else ends the run while the send is out (turning the flow to draft does this): the
    // worker's next write is refused, so the run stays ended and the next step never runs.
    event(&mut h, "Hook", "ended@x.test", json!({})).await;
    let state = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&state).await });
    hook.arrived.acquire().await.unwrap().forget();
    let (run2, ..) = run_of(&h, &fid, "ended@x.test").await;
    sqlx::query("UPDATE flow_runs SET status = 'exited', finished_at = now() WHERE id = $1").bind(run2).execute(&h.state.db).await.unwrap();
    hook.go.add_permits(1);
    worker.await.unwrap().unwrap();
    assert_eq!(run_of(&h, &fid, "ended@x.test").await.1, "exited");
    assert_eq!(prop(&h, "ended@x.test", "after").await, None, "the run didn't go on after it was ended");
    // The same when the send is the last step: finishing doesn't turn "exited" into "completed".
    let last = live_flow(&mut h, json!({"type": "metric", "metric": "HookLast"}), json!({"entry": "w", "nodes": [{"id": "w", "type": "webhook", "url": hook.url, "next": null}]})).await;
    event(&mut h, "HookLast", "last@x.test", json!({})).await;
    let state = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&state).await });
    hook.arrived.acquire().await.unwrap().forget();
    let (run4, ..) = run_of(&h, &last, "last@x.test").await;
    sqlx::query("UPDATE flow_runs SET status = 'exited', finished_at = now() WHERE id = $1").bind(run4).execute(&h.state.db).await.unwrap();
    hook.go.add_permits(1);
    worker.await.unwrap().unwrap();
    assert_eq!(run_of(&h, &last, "last@x.test").await.1, "exited");

    // Removed before its step runs: nothing is sent.
    let fid2 = live_flow(&mut h, json!({"type": "metric", "metric": "Later"}), json!({"entry": "d", "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "minutes", "next": "w"},
        {"id": "w", "type": "webhook", "url": hook.url, "next": null}]})).await;
    event(&mut h, "Later", "gone@x.test", json!({})).await;
    h.drain().await;
    let (run3, ..) = run_of(&h, &fid2, "gone@x.test").await;
    let r = h.ok("POST", &format!("/api/flows/{fid2}/runs/remove"), Some(json!({"run_ids": [run3], "confirm": true}))).await;
    assert_eq!(r["removed"], 1, "{r}");
    let before = hook.hits.load(Ordering::SeqCst);
    hook.go.add_permits(10);
    h.fast_forward().await;
    assert_eq!(hook.hits.load(Ordering::SeqCst), before);
    assert_eq!(run_of(&h, &fid2, "gone@x.test").await.1, "exited");
    h.cleanup().await;
}

/// A list-triggered live flow with these people on its list, and a back-populate with its people fixed.
async fn list_backfill(h: &mut Harness, emails: &[&str]) -> (String, Uuid, Uuid) {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Guests"}))).await;
    let lid = list["id"].as_str().unwrap().to_string();
    for e in emails {
        h.ok("POST", "/api/profiles", Some(json!({"email": e, "list_ids": [lid]}))).await;
    }
    h.drain().await;
    let fid = live_flow(h, json!({"type": "list", "list_id": lid}), json!({"entry": "d", "nodes": [{"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]})).await;
    // Joining the list before the flow was live started nobody; clear anything that did.
    sqlx::query("DELETE FROM flow_runs WHERE flow_id = $1::uuid").bind(&fid).execute(&h.state.db).await.unwrap();
    let b = h.ok("POST", &format!("/api/flows/{fid}/backfill"), Some(json!({"confirm": true}))).await;
    let bid: Uuid = b["id"].as_str().unwrap().parse().unwrap();
    // Fix the people (the job's first step), leaving the batches to the test.
    dumbmail::features::flows_plus::backfill::run(&h.state, &json!({"backfill_id": bid})).await.unwrap();
    sqlx::query("DELETE FROM jobs WHERE kind = 'flows_plus_backfill'").execute(&h.state.db).await.unwrap();
    (fid, bid, lid.parse().unwrap())
}

async fn flow_runs(h: &Harness, fid: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM flow_runs WHERE flow_id = $1::uuid").bind(fid).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn back_populate_and_deleting_a_profile_dont_deadlock() {
    use dumbmail::features::flows_plus::backfill::{run, EnrollPause};
    let mut h = harness!();
    h.signup().await;
    let (fid, bid, _) = list_backfill(&mut h, &["del@x.test"]).await;
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'del@x.test'").fetch_one(&h.state.db).await.unwrap();
    // Hold the enrollment after it claimed the person; delete the profile meanwhile.
    let pause = EnrollPause::hold(bid);
    let state = h.state.clone();
    let enrolling = tokio::spawn(async move { run(&state, &json!({"backfill_id": bid})).await });
    pause.arrived().await;
    let db = h.state.db.clone();
    let deleting = tokio::spawn(async move { sqlx::query("DELETE FROM profiles WHERE id = $1").bind(pid).execute(&db).await.map(|r| r.rows_affected()) });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!deleting.is_finished(), "the delete waits for the enrollment that holds the profile");
    pause.release(bid);
    let (e, d) = (enrolling.await.unwrap(), deleting.await.unwrap());
    assert!(e.is_ok(), "enrollment: {e:?}");
    assert_eq!(d.expect("delete"), 1, "no deadlock: both finished");
    assert_eq!(flow_runs(&h, &fid).await, 0, "the deleted profile's run went with it");

    // A profile deleted before its turn counts as skipped.
    let (_, bid2, _) = list_backfill(&mut h, &["a@x.test", "b@x.test"]).await;
    sqlx::query("DELETE FROM profiles WHERE email = 'a@x.test'").execute(&h.state.db).await.unwrap();
    run(&h.state, &json!({"backfill_id": bid2})).await.unwrap();
    let (added, skipped, status): (i32, i32, String) = sqlx::query_as("SELECT added, skipped, status FROM flow_backfills WHERE id = $1").bind(bid2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((added, skipped, status.as_str()), (1, 1, "done"));
    h.cleanup().await;
}

#[tokio::test]
async fn back_populate_rechecks_the_list_and_stops_when_cancelled() {
    use dumbmail::features::flows_plus::backfill::{run, EnrollPause};
    let mut h = harness!();
    h.signup().await;
    // Taken off the list while their enrollment is under way: not enrolled.
    let (fid, bid, lid) = list_backfill(&mut h, &["left@x.test"]).await;
    let pause = EnrollPause::hold(bid);
    let state = h.state.clone();
    let enrolling = tokio::spawn(async move { run(&state, &json!({"backfill_id": bid})).await });
    pause.arrived().await;
    sqlx::query("DELETE FROM list_members WHERE list_id = $1").bind(lid).execute(&h.state.db).await.unwrap();
    pause.release(bid);
    enrolling.await.unwrap().unwrap();
    assert_eq!(flow_runs(&h, &fid).await, 0);
    let outcome: String = sqlx::query_scalar("SELECT outcome FROM flow_backfill_members WHERE backfill_id = $1").bind(bid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(outcome, "skipped: no longer in the trigger list or segment");

    // A cancel while someone is being enrolled waits for that one; nobody is enrolled after it returns.
    let (fid, bid, _) = list_backfill(&mut h, &["c1@x.test", "c2@x.test", "c3@x.test"]).await;
    let pause = EnrollPause::hold(bid);
    let state = h.state.clone();
    let enrolling = tokio::spawn(async move { run(&state, &json!({"backfill_id": bid})).await });
    pause.arrived().await;
    let db = h.state.db.clone();
    let cancelling = tokio::spawn(async move {
        sqlx::query("UPDATE flow_backfills SET status = 'cancelled', finished_at = now() WHERE id = $1 AND status = 'running'").bind(bid).execute(&db).await.map(|r| r.rows_affected())
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!cancelling.is_finished(), "the cancel waits for the enrollment in progress");
    pause.release(bid);
    assert_eq!(cancelling.await.unwrap().unwrap(), 1);
    let at_cancel = flow_runs(&h, &fid).await;
    enrolling.await.unwrap().unwrap();
    assert_eq!(at_cancel, 1, "the one in progress finished first");
    assert_eq!(flow_runs(&h, &fid).await, 1, "nobody after the cancel returned");
    h.cleanup().await;
}

#[tokio::test]
async fn back_populate_is_paced_and_a_dead_job_frees_the_flow() {
    use dumbmail::features::flows_plus::backfill::{run, BATCH};
    let mut h = harness!();
    h.signup().await;
    let emails: Vec<String> = (0..(BATCH + 50)).map(|i| format!("p{i}@x.test")).collect();
    let refs: Vec<&str> = emails.iter().map(String::as_str).collect();
    let (fid, bid, _) = list_backfill(&mut h, &refs).await;
    run(&h.state, &json!({"backfill_id": bid})).await.unwrap();
    assert_eq!(flow_runs(&h, &fid).await, BATCH, "one batch");
    // Their first steps haven't been picked up: the next batch waits.
    run(&h.state, &json!({"backfill_id": bid})).await.unwrap();
    assert_eq!(flow_runs(&h, &fid).await, BATCH, "no second batch while the first one's steps wait");
    let later: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flows_plus_backfill' AND run_at > now()").fetch_one(&h.state.db).await.unwrap();
    assert!(later >= 1, "it checks again shortly");
    // Sending picks them up; the next batch goes.
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::MAIN).await.unwrap();
    run(&h.state, &json!({"backfill_id": bid})).await.unwrap();
    assert_eq!(flow_runs(&h, &fid).await, BATCH + 50);

    // A back-populate whose job runs out of retries is marked failed, and the flow can start another.
    let (fid2, bid2, _) = list_backfill(&mut h, &["x1@x.test"]).await;
    dumbmail::jobs::enqueue_background(&h.state.db, "flows_plus_backfill", json!({"backfill_id": bid2}), None).await.unwrap();
    sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE kind = 'flows_plus_backfill'").execute(&h.state.db).await.unwrap();
    sqlx::query("ALTER TABLE flow_backfill_members RENAME TO flow_backfill_members_gone").execute(&h.state.db).await.unwrap();
    dumbmail::jobs::drain_lane(&h.state, dumbmail::jobs::BACKGROUND).await.unwrap();
    sqlx::query("ALTER TABLE flow_backfill_members_gone RENAME TO flow_backfill_members").execute(&h.state.db).await.unwrap();
    let (status, error): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM flow_backfills WHERE id = $1").bind(bid2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "failed");
    assert!(error.unwrap_or_default().contains("repeated errors"));
    h.ok("POST", &format!("/api/flows/{fid2}/backfill"), Some(json!({"confirm": true}))).await;
    h.cleanup().await;
}

/// A proxy in front of Postgres that counts the statements its clients run (simple queries and
/// extended-protocol executes) while `on` is set.
async fn counting_proxy(target: &str) -> (u16, Arc<AtomicUsize>, Arc<AtomicBool>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let count = Arc::new(AtomicUsize::new(0));
    let on = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = target.to_string();
    let (c, o) = (count.clone(), on.clone());
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else { return };
            let server = tokio::net::TcpStream::connect(&target).await.unwrap();
            let (mut cr, mut cw) = client.into_split();
            let (mut sr, mut sw) = server.into_split();
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut sr, &mut cw).await;
            });
            let (c, o) = (c.clone(), o.clone());
            tokio::spawn(async move {
                // Untyped messages first (an SSL request, then the startup message).
                let mut typed = false;
                loop {
                    let mut head = [0u8; 5];
                    let len = if typed {
                        if cr.read_exact(&mut head).await.is_err() {
                            return;
                        }
                        if o.load(Ordering::SeqCst) && (head[0] == b'Q' || head[0] == b'E') {
                            c.fetch_add(1, Ordering::SeqCst);
                        }
                        u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize
                    } else {
                        if cr.read_exact(&mut head[..4]).await.is_err() {
                            return;
                        }
                        u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize
                    };
                    let mut body = vec![0u8; len - 4];
                    if cr.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let out = if typed { [&head[..], &body[..]].concat() } else { [&head[..4], &body[..]].concat() };
                    if sw.write_all(&out).await.is_err() {
                        return;
                    }
                    // 80877103 is the SSL request; anything else untyped is the startup message.
                    if !typed && !(body.len() == 4 && u32::from_be_bytes([body[0], body[1], body[2], body[3]]) == 80877103) {
                        typed = true;
                    }
                }
            });
        }
    });
    (port, count, on)
}

#[tokio::test]
async fn entering_a_flow_costs_what_it_did() {
    let mut h = harness!();
    h.signup().await;
    let fid = live_flow(&mut h, json!({"type": "metric", "metric": "Count"}), json!({"entry": "d", "settings": {"reentry": "never"}, "nodes": [
        {"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": null}]})).await;
    let flow: dumbmail::models::FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1::uuid").bind(&fid).fetch_one(&h.state.db).await.unwrap();
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "count@x.test"}))).await;
    let pid: Uuid = p["id"].as_str().unwrap().parse().unwrap();
    // The same app on the same database, through the counting proxy, one connection.
    let opts: sqlx::postgres::PgConnectOptions = h.state.cfg.database_url.parse().unwrap();
    let (port, count, on) = counting_proxy(&format!("127.0.0.1:{}", opts.get_port())).await;
    let db = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect_with(opts.host("127.0.0.1").port(port).ssl_mode(sqlx::postgres::PgSslMode::Disable)).await.unwrap();
    let s = &h.state;
    let state = dumbmail::AppState(Arc::new(dumbmail::state::Inner {
        db,
        cfg: s.cfg.clone(),
        email: s.email.clone(),
        sms: s.sms.clone(),
        http: s.http.clone(),
        pending_checks: Default::default(),
        daily_limits: Default::default(),
    }));
    let measure = |state: dumbmail::AppState| {
        let (count, on, flow) = (count.clone(), on.clone(), flow.clone());
        async move {
            count.store(0, Ordering::SeqCst);
            on.store(true, Ordering::SeqCst);
            let id = dumbmail::flows::start(&state, &flow, pid, json!({})).await.unwrap();
            on.store(false, Ordering::SeqCst);
            (id, count.load(Ordering::SeqCst))
        }
    };
    let (accepted, accepted_n) = measure(state.clone()).await;
    assert!(accepted.is_some());
    let (blocked, blocked_n) = measure(state.clone()).await;
    assert!(blocked.is_none());
    eprintln!("statements: accepted {accepted_n}, blocked {blocked_n}");
    // On main before this change: accepted 3 (check, insert, enqueue), blocked 1 (the check).
    assert_eq!(blocked_n, 1, "a blocked entry is one statement, as before");
    assert!(accepted_n <= 3 + 2, "an accepted entry is at most two more: {accepted_n}");
    h.cleanup().await;
}

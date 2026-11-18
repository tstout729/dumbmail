//! Short links in text messages: fixed-length links, one target per destination, clicks recorded
//! like long links, tampering and cross-message forgery refused, link tags on texts, previews of the
//! real length, replies, closed accounts, cleanup, and the statements a send costs.

#[macro_use]
mod common;

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::http::StatusCode;
use dumbmail::features::short_links::{self, code};
use serde_json::{json, Value};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;
use uuid::Uuid;

use common::*;

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// A signed-up account with credit, a texting number and the short link index in place.
async fn setup(h: &mut Harness) -> (Uuid, String) {
    h.signup().await;
    h.credit(20.0).await;
    h.claim_number().await;
    short_links::ensure_index(&h.state).await.unwrap();
    assert!(short_links::index_ready(&h.state));
    short_links::mark_serving(&h.state);
    short_links::runtime::set_issue_delay(&h.state, std::time::Duration::ZERO);
    assert!(short_links::issuing(&h.state));
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texts"}))).await["id"].as_str().unwrap().to_string();
    (account_id(h).await, list)
}

/// A number whose own time zone is in the middle of the day now, so marketing texts to it aren't
/// held for quiet hours. `n` keeps numbers distinct.
fn daytime_number(n: u32) -> String {
    let candidates = [
        format!("+1808555{:04}", n), format!("+1503555{:04}", n), format!("+1303555{:04}", n), format!("+1312555{:04}", n), format!("+1212555{:04}", n),
        format!("+4420794{:05}", n), format!("+9198123{:05}", n), format!("+97798123{:04}", n), format!("+8190123{:05}", n),
    ];
    candidates
        .into_iter()
        .find(|c| dumbmail::features::sendtime::zone::phone_zone(c).is_some_and(|tz| (11..17).contains(&chrono::Timelike::hour(&chrono::Utc::now().with_timezone(&tz)))))
        .expect("some zone is in the middle of the day")
}

async fn texter(h: &mut Harness, list: &str, n: u32) -> (Uuid, String) {
    let phone = daytime_number(n);
    let p = h.ok("POST", "/api/profiles", Some(json!({"phone_number": phone, "sms_consent": "subscribed", "list_ids": [list]}))).await;
    (p["id"].as_str().unwrap().parse().unwrap(), phone)
}

fn body_to(h: &Harness, phone: &str) -> String {
    h.state.sms.sent().into_iter().rev().find(|s| s.to == phone).unwrap_or_else(|| panic!("no text to {phone}")).body
}

/// The short paths (after `/s/`) in a text, and the punctuation right after each.
fn short_paths(body: &str) -> Vec<String> {
    body.split("https://pl.test/s/").skip(1).map(|rest| rest.chars().take(code::PATH_LEN).collect()).collect()
}

async fn click(h: &Harness, path: &str) -> (StatusCode, Option<String>, String) {
    let (s, headers, body) = h.raw("GET", &format!("/s/{path}"), &[], "").await;
    (s, headers.get("location").map(|l| l.to_str().unwrap().to_string()), body)
}

async fn clicks_of(h: &Harness, message: Uuid) -> (i64, i64, bool) {
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM message_clicks WHERE message_id = $1").bind(message).fetch_one(&h.state.db).await.unwrap();
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE message_id = $1 AND metric_id IN (SELECT id FROM metrics WHERE name = 'Clicked SMS')").bind(message).fetch_one(&h.state.db).await.unwrap();
    let clicked: bool = sqlx::query_scalar("SELECT clicked_at IS NOT NULL FROM messages WHERE id = $1").bind(message).fetch_one(&h.state.db).await.unwrap();
    (rows, events, clicked)
}

async fn message_to(h: &Harness, phone: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM messages WHERE to_address = $1 AND status = 'sent' ORDER BY created_at DESC LIMIT 1").bind(phone).fetch_one(&h.state.db).await.unwrap()
}

async fn sms_campaign(h: &mut Harness, list: &str, body: &str) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Black Friday", "channel": "sms", "audience": {"include": [{"type": "list", "id": list}]}, "content": {"body": body}}))).await;
    c["id"].as_str().unwrap().to_string()
}

async fn launch(h: &mut Harness, id: &str) {
    h.ok("POST", &format!("/api/campaigns/{id}/send"), None).await;
    h.drain().await;
}

#[tokio::test]
async fn campaign_texts_get_one_short_link_per_destination_and_clicks_count_like_long_links() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    h.ok("PUT", "/api/link-tags", Some(json!({"enabled": true}))).await;
    let mut people = Vec::new();
    for n in 1..=3 {
        people.push(texter(&mut h, &list, 7000 + n).await);
    }
    let long = format!("https://shop.test/collections/black-friday?{}", "x=1&".repeat(40));
    let long = long.trim_end_matches('&');
    let id = sms_campaign(&mut h, &list, &format!("Acme Goods: 40% off today: {long} - also https://shop.test/b and {{{{ unsubscribe_url }}}} Reply STOP to opt out")).await;
    launch(&mut h, &id).await;

    // One target per destination, not per recipient, with the campaign's link tags on it.
    let targets: Vec<(String, String)> = sqlx::query_as("SELECT code, url FROM short_link_targets WHERE account_id = $1 ORDER BY url").bind(acct).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(targets.iter().any(|(_, u)| u.starts_with("https://shop.test/b?utm_source=dumbmail&utm_medium=sms&utm_campaign=black-friday-")), "{targets:?}");
    assert!(targets.iter().any(|(_, u)| u.starts_with(&format!("{long}&utm_source=dumbmail&utm_medium=sms"))), "{targets:?}");

    let mut paths = Vec::new();
    for (pid, phone) in &people {
        let body = body_to(&h, phone);
        // Every link the sender wrote is the same length whatever it points to, and the variable's
        // link (DumbMail's unsubscribe link) isn't shortened.
        let p = short_paths(&body);
        assert_eq!(p.len(), 2, "{body}");
        assert!(body.contains(&format!("https://pl.test/s/{} - also", p[0])), "{body}");
        assert!(!body.contains("shop.test"), "{body}");
        assert!(body.contains("https://pl.test/t/c/"), "the unsubscribe link from a variable keeps the long form: {body}");
        assert!(body.ends_with("Reply STOP to opt out"), "{body}");
        // Billing counts the final text.
        let (segments, short_ref): (i32, Option<String>) = sqlx::query_as("SELECT segments, short_ref FROM messages WHERE to_address = $1").bind(phone).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(segments as usize, dumbmail::providers::sms::segment_count(&body).1);
        assert_eq!(short_ref.as_deref(), Some(&p[0][code::TARGET_LEN..code::TARGET_LEN + code::REF_LEN]));
        paths.push((*pid, phone.clone(), p[0].clone(), p[1].clone()));
    }

    // A click redirects to the tagged destination and is recorded exactly like a /t/c/ click.
    let (pid, phone, path, second) = paths[0].clone();
    let mid = message_to(&h, &phone).await;
    let (s, location, _) = click(&h, &path).await;
    assert!(s.is_redirection(), "{s}");
    assert!(location.as_deref().unwrap().starts_with(&format!("{long}&utm_source=dumbmail&utm_medium=sms")), "{location:?}");
    assert_eq!(clicks_of(&h, mid).await, (1, 1, true));
    let ev: (Uuid, Value) = sqlx::query_as("SELECT profile_id, properties FROM events WHERE message_id = $1 AND metric_id IN (SELECT id FROM metrics WHERE name = 'Clicked SMS')").bind(mid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ev.0, pid);
    assert_eq!(ev.1["campaign_id"], id.as_str());
    assert!(ev.1["url"].as_str().unwrap().starts_with(long), "{}", ev.1);

    // Changing any one character of the path fails, and nothing is recorded.
    for i in 0..code::PATH_LEN {
        let mut b = path.clone().into_bytes();
        b[i] = if b[i] == b'a' { b'b' } else { b'a' };
        let (s, location, body) = click(&h, std::str::from_utf8(&b).unwrap()).await;
        assert_eq!((s, location, body.as_str()), (StatusCode::NOT_FOUND, None, "link not found"), "changed character {i}");
    }
    for bad in [path[1..].to_string(), format!("{path}0"), format!("{}-{}", &path[..10], &path[11..])] {
        assert_eq!(click(&h, &bad).await.0, StatusCode::NOT_FOUND, "{bad:?}");
    }
    assert_eq!(clicks_of(&h, mid).await, (1, 1, true));

    // One message's valid tag can't record a click for another message, even on the same target.
    let (_, other_phone, other_path, _) = paths[1].clone();
    let other = message_to(&h, &other_phone).await;
    let forged = format!("{}{}", &path[..code::TARGET_LEN + code::REF_LEN], &other_path[code::TARGET_LEN + code::REF_LEN..]);
    assert_eq!(click(&h, &forged).await.0, StatusCode::NOT_FOUND);
    assert_eq!(&other_path[..code::TARGET_LEN], &path[..code::TARGET_LEN], "both texts link to the same target");
    // The tag covers the target too: this message's tag for one link doesn't open its other link.
    let forged = format!("{}{}", &second[..code::TARGET_LEN], &path[code::TARGET_LEN..]);
    assert_ne!(forged, second);
    assert_eq!(click(&h, &forged).await.0, StatusCode::NOT_FOUND, "the target code is covered too");
    assert_eq!(clicks_of(&h, other).await, (0, 0, false));
    h.cleanup().await;
}

#[tokio::test]
async fn texts_without_short_links_keep_the_long_form_and_old_links_keep_working() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 7101).await;
    h.ok("PUT", "/api/link-tags", Some(json!({"enabled": true}))).await;
    let s = h.ok("PATCH", "/api/short-links/settings", Some(json!({"enabled": false}))).await;
    assert_eq!(s["enabled"], false);
    let id = sms_campaign(&mut h, &list, "Acme Goods: sale https://shop.test/s?a=1 Reply STOP to opt out").await;
    launch(&mut h, &id).await;
    let body = body_to(&h, &phone);
    assert!(short_paths(&body).is_empty() && body.contains("https://pl.test/t/c/"), "{body}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM short_link_targets WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);
    // A long link isn't tagged (tags would make it longer) and its click still works.
    let link = body.split_whitespace().find(|w| w.contains("/t/c/")).unwrap();
    let dest = url::form_urlencoded::parse(link.split_once('?').unwrap().1.as_bytes()).find(|(k, _)| k == "u").unwrap().1.into_owned();
    assert_eq!(dest, "https://shop.test/s?a=1");
    let (s, hd, _) = h.raw("GET", link.strip_prefix("https://pl.test").unwrap(), &[], "").await;
    assert!(s.is_redirection());
    assert_eq!(hd.get("location").unwrap(), "https://shop.test/s?a=1");
    let mid = message_to(&h, &phone).await;
    assert_eq!(clicks_of(&h, mid).await, (1, 1, true));
    // The setting change is in the activity log.
    let logged: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'short_links.update'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(logged, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn previews_show_a_sample_link_of_the_real_length() {
    let mut h = harness!();
    setup(&mut h).await;
    let body = "Acme Goods: new arrivals https://shop.test/collections/new-arrivals-for-the-winter-season?ref=sms";
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": body}}))).await;
    let shown = r["body"].as_str().unwrap();
    let settings = h.ok("GET", "/api/short-links/settings", None).await;
    let sample = settings["sample"].as_str().unwrap();
    assert_eq!(sample.len(), "https://pl.test/s/".len() + code::PATH_LEN);
    assert_eq!(settings["link_length"], sample.len());
    assert_eq!(settings["ready"], true);
    assert!(shown.contains(sample) && !shown.contains("shop.test"), "{shown}");
    assert_eq!(r["characters"].as_u64().unwrap() as usize, dumbmail::providers::sms::segment_count(shown).0);
    // The real send is exactly as long as the preview said.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "One"}))).await["id"].as_str().unwrap().to_string();
    let (_, phone) = texter(&mut h, &list, 7201).await;
    let id = sms_campaign(&mut h, &list, body).await;
    launch(&mut h, &id).await;
    let sent = body_to(&h, &phone);
    assert_eq!(sent.chars().count(), shown.chars().count(), "{sent}\n{shown}");
    assert_ne!(sent, shown);
    // Off: the preview shows the link as written.
    h.ok("PATCH", "/api/short-links/settings", Some(json!({"enabled": false}))).await;
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": body}}))).await;
    assert!(r["body"].as_str().unwrap().contains("https://shop.test/collections/"), "{r}");
    h.cleanup().await;
}

#[tokio::test]
async fn flow_texts_and_replies_get_short_links_too() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let (pid, phone) = texter(&mut h, &list, 7301).await;
    let def = json!({"entry": "s", "nodes": [{"id": "s", "type": "sms", "name": "Thanks", "content": {"body": "Acme Goods: thanks! Track it at https://shop.test/track"}, "next": null}]});
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": def}))).await;
    let fid = flow["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"phone_number": phone}}))).await;
    h.drain().await;
    let body = body_to(&h, &phone);
    let p = short_paths(&body);
    assert_eq!(p.len(), 1, "{body}");
    let (s, location, _) = click(&h, &p[0]).await;
    assert!(s.is_redirection());
    assert_eq!(location.as_deref(), Some("https://shop.test/track"));
    let mid = message_to(&h, &phone).await;
    assert_eq!(clicks_of(&h, mid).await, (1, 1, true));

    // A reply typed in the inbox (the conversational send path) reuses the same target.
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1", dumbmail::models::PROFILE_COLS);
    let profile: dumbmail::models::Profile = sqlx::query_as(&q).bind(pid).fetch_one(&h.state.db).await.unwrap();
    let ours: String = sqlx::query_scalar("SELECT phone_number FROM phone_numbers WHERE account_id = $1 LIMIT 1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let content = json!({"body": "Here you go: https://shop.test/track"});
    let spec = dumbmail::sending::SendSpec {
        account: &account, profile: &profile, channel: "sms", content: &content, campaign_id: None, flow_id: None, flow_node_id: None, event: None, is_test: false, message_id: None, to_override: None,
    };
    let conv = dumbmail::sending::Conversational { kind: "reply", sent_by: None, to: phone.clone(), from: ours };
    let out = dumbmail::sending::send_conversational(&h.state, spec, &conv).await.unwrap();
    assert!(matches!(out, dumbmail::sending::Outcome::Sent(_)), "{out:?}");
    let reply = body_to(&h, &phone);
    let rp = short_paths(&reply);
    assert_eq!(rp.len(), 1, "{reply}");
    assert_eq!(&rp[0][..code::TARGET_LEN], &p[0][..code::TARGET_LEN], "the same destination reuses its target");
    assert_ne!(rp[0], p[0]);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM short_link_targets WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.cleanup().await;
}

/// A second account in the same database.
async fn other_account(h: &Harness) -> Harness {
    let mut o = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    o.ok("POST", "/api/auth/signup", Some(json!({"email": "owner@other.test", "password": "correct horse", "company": "Other Co"}))).await;
    o
}

#[tokio::test]
async fn accounts_are_kept_apart_and_closed_accounts_stop_redirecting() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 7401).await;
    let id = sms_campaign(&mut h, &list, "Acme Goods: https://shop.test/mine").await;
    launch(&mut h, &id).await;
    let path = short_paths(&body_to(&h, &phone))[0].clone();

    // Another account linking to the same destination gets its own target.
    let mut o = other_account(&h).await;
    let other: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE id <> $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let mut conn = h.state.db.acquire().await.unwrap();
    let theirs = short_links::send::ensure_targets(&mut conn, other, &["https://shop.test/mine".to_string()]).await.unwrap();
    drop(conn);
    let mine: String = sqlx::query_scalar("SELECT code FROM short_link_targets WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_ne!(theirs["https://shop.test/mine"], mine);
    // Their target code with this message's reference and tag doesn't resolve.
    assert_eq!(click(&h, &format!("{}{}", theirs["https://shop.test/mine"], &path[code::TARGET_LEN..])).await.0, StatusCode::NOT_FOUND);
    // The other account sees its own setting, and changing it needs settings permission (see tests/team.rs).
    assert_eq!(o.ok("GET", "/api/short-links/settings", None).await["enabled"], true);

    assert!(click(&h, &path).await.0.is_redirection());
    sqlx::query("UPDATE accounts SET closed_at = now() WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    assert_eq!(click(&h, &path).await.0, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn targets_unused_for_400_days_are_deleted() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 7501).await;
    let id = sms_campaign(&mut h, &list, "Acme Goods: https://shop.test/old and https://shop.test/new").await;
    launch(&mut h, &id).await;
    let paths = short_paths(&body_to(&h, &phone));
    sqlx::query("UPDATE short_link_targets SET last_used_at = now() - interval '401 days' WHERE url = 'https://shop.test/old'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE short_link_targets SET last_used_at = now() - interval '399 days' WHERE url = 'https://shop.test/new'").execute(&h.state.db).await.unwrap();
    dumbmail::jobs::enqueue_background(&h.state.db, short_links::JOB_CLEANUP, json!({}), None).await.unwrap();
    h.drain().await;
    let left: Vec<String> = sqlx::query_scalar("SELECT url FROM short_link_targets WHERE account_id = $1").bind(acct).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(left, vec!["https://shop.test/new".to_string()]);
    assert_eq!(click(&h, &paths[0]).await.0, StatusCode::NOT_FOUND);
    assert!(click(&h, &paths[1]).await.0.is_redirection());
    h.cleanup().await;
}

#[tokio::test]
async fn a_new_destination_racing_itself_gets_one_target() {
    let mut h = harness!();
    let (acct, _) = setup(&mut h).await;
    let url = "https://shop.test/race".to_string();
    let runs = (0..8).map(|_| {
        let db = h.state.db.clone();
        let url = url.clone();
        tokio::spawn(async move {
            let mut conn = db.acquire().await.unwrap();
            short_links::send::ensure_targets(&mut conn, acct, &[url]).await.unwrap()
        })
    });
    let codes: Vec<String> = futures_util::future::join_all(runs).await.into_iter().map(|r| r.unwrap()["https://shop.test/race"].clone()).collect();
    assert!(codes.windows(2).all(|w| w[0] == w[1]), "{codes:?}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM short_link_targets WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.cleanup().await;
}

// ---------- Statement counts ----------

#[derive(Clone, Default)]
struct Counter(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for Counter {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

async fn counted<T>(f: impl Future<Output = T>) -> (T, usize) {
    let c = Counter::default();
    let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(c.clone()));
    let out = f.await;
    drop(guard);
    (out, c.0.load(Ordering::SeqCst))
}

/// Statements for one flow text step with `body`.
async fn flow_text(h: &mut Harness, name: &str, body: &str, phone: &str) -> usize {
    let def = json!({"entry": "s", "nodes": [{"id": "s", "type": "sms", "content": {"body": body}, "next": null}]});
    let fid: Uuid = h.ok("POST", "/api/flows", Some(json!({"name": name, "trigger": {"type": "metric", "metric": "Never"}, "definition": def}))).await["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    let flow: dumbmail::models::FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(fid).fetch_one(&h.state.db).await.unwrap();
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE phone_number = $1").bind(phone).fetch_one(&h.state.db).await.unwrap();
    let run = dumbmail::flows::start(&h.state, &flow, pid, json!({})).await.unwrap().unwrap();
    let (r, n) = counted(dumbmail::flows::step(&h.state, run)).await;
    r.unwrap();
    assert!(body_to(h, phone).contains(&body[12..20]), "the step sent its text");
    n
}

#[tokio::test]
async fn a_text_with_a_link_costs_at_most_one_statement_and_a_campaign_none_per_recipient() {
    let mut h = harness!();
    let (_, list) = setup(&mut h).await;
    let (_, a) = texter(&mut h, &list, 7601).await;
    let (_, b) = texter(&mut h, &list, 7602).await;
    // Warm up (first-message wording, metric rows) so both counts below take the same path.
    flow_text(&mut h, "Warm A", "Acme Goods: hello there", &a).await;
    flow_text(&mut h, "Warm B", "Acme Goods: hello there", &b).await;
    // No link: exactly the path on main (short links run no statement). One link: one more.
    let plain = flow_text(&mut h, "Plain", "Acme Goods: no links here", &a).await;
    let linked = flow_text(&mut h, "Linked", "Acme Goods: see https://shop.test/x", &b).await;
    assert!(!short_paths(&body_to(&h, &b)).is_empty());
    assert_eq!(linked, plain + 1, "a text with a link costs one statement more than one without");

    // A campaign text: no statement per recipient. Compare a batch of texts with and without a link.
    let mut per_batch = Vec::new();
    for (name, body) in [("No link", "Acme Goods: no link today"), ("Link", "Acme Goods: shop https://shop.test/sale")] {
        let id = sms_campaign(&mut h, &list, body).await;
        let cid: Uuid = id.parse().unwrap();
        sqlx::query("UPDATE campaigns SET status = 'sending', name = $2 WHERE id = $1").bind(cid).bind(name).execute(&h.state.db).await.unwrap();
        dumbmail::campaigns::prepare_send(&h.state, cid).await.unwrap();
        // A retried send job keeps the targets the first run set up.
        let frozen: Value = sqlx::query_scalar("SELECT content FROM campaigns WHERE id = $1").bind(cid).fetch_one(&h.state.db).await.unwrap();
        dumbmail::campaigns::prepare_send(&h.state, cid).await.unwrap();
        let again: Value = sqlx::query_scalar("SELECT content FROM campaigns WHERE id = $1").bind(cid).fetch_one(&h.state.db).await.unwrap();
        assert_eq!(frozen, again);
        assert_eq!(frozen.get(short_links::send::CONTENT_KEY).is_some(), name == "Link", "{frozen}");
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM messages WHERE campaign_id = $1 ORDER BY id").bind(cid).fetch_all(&h.state.db).await.unwrap();
        assert_eq!(ids.len(), 2);
        let (r, n) = counted(dumbmail::campaigns::send_batch(&h.state, cid, &ids)).await;
        r.unwrap();
        per_batch.push(n);
    }
    assert!(!short_paths(&body_to(&h, &a)).is_empty());
    assert_eq!(per_batch[0], per_batch[1], "a campaign's short links cost nothing per recipient");
    h.cleanup().await;
}

#[tokio::test]
async fn a_code_given_to_another_destination_never_opens_an_old_link() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 7701).await;
    let id = sms_campaign(&mut h, &list, "Acme Goods: https://shop.test/original").await;
    launch(&mut h, &id).await;
    let path = short_paths(&body_to(&h, &phone))[0].clone();
    assert!(click(&h, &path).await.0.is_redirection());
    // Cleanup frees the code, and later the same account gets it for another destination.
    let code = &path[..code::TARGET_LEN];
    sqlx::query("DELETE FROM short_link_targets WHERE code = $1").bind(code).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO short_link_targets (code, account_id, url) VALUES ($1, $2, 'https://elsewhere.test/new')").bind(code).bind(acct).execute(&h.state.db).await.unwrap();
    let (s, location, _) = click(&h, &path).await;
    assert_eq!((s, location), (StatusCode::NOT_FOUND, None), "the old link must not open the new destination");
    h.cleanup().await;
}

#[tokio::test]
async fn cleanup_never_deletes_a_target_a_send_is_refreshing() {
    let mut h = harness!();
    let (acct, _) = setup(&mut h).await;
    let url = "https://shop.test/stale".to_string();
    let mut conn = h.state.db.acquire().await.unwrap();
    let code = short_links::send::ensure_targets(&mut conn, acct, std::slice::from_ref(&url)).await.unwrap()[&url].clone();
    drop(conn);
    sqlx::query("UPDATE short_link_targets SET last_used_at = now() - interval '401 days' WHERE code = $1").bind(&code).execute(&h.state.db).await.unwrap();
    // A send refreshes the stale target in a transaction that hasn't committed yet...
    let mut tx = h.state.db.begin().await.unwrap();
    let again = short_links::send::ensure_targets(&mut tx, acct, std::slice::from_ref(&url)).await.unwrap();
    assert_eq!(again[&url], code);
    // ...while cleanup picks the row as stale and waits for the send's lock.
    let st = h.state.clone();
    let job = tokio::spawn(async move { short_links::cleanup(&st).await });
    let mut waited = false;
    for _ in 0..100 {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query ILIKE 'DELETE FROM short_link_targets%'")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
        if n > 0 {
            waited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(waited, "cleanup should be waiting on the send's row lock");
    tx.commit().await.unwrap();
    job.await.unwrap().unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM short_link_targets WHERE code = $1").bind(&code).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 1, "a target refreshed by a send survives cleanup");
    h.cleanup().await;
}

#[tokio::test]
async fn a_freshly_started_server_sends_long_links_until_it_has_served_a_while() {
    let mut h = harness!();
    h.signup().await;
    h.credit(20.0).await;
    h.claim_number().await;
    short_links::ensure_index(&h.state).await.unwrap();
    // Just started: the old server of a deploy may still get the clicks, and it can't resolve /s/.
    short_links::mark_serving(&h.state);
    short_links::runtime::set_issue_delay(&h.state, std::time::Duration::from_secs(2));
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Texts"}))).await["id"].as_str().unwrap().to_string();
    let (_, phone) = texter(&mut h, &list, 7801).await;
    flow_text(&mut h, "Early", "Acme Goods: early https://shop.test/early", &phone).await;
    let early = body_to(&h, &phone);
    assert!(short_paths(&early).is_empty() && early.contains("https://pl.test/t/c/"), "{early}");
    assert_eq!(h.ok("GET", "/api/short-links/settings", None).await["ready"], false);
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": "Acme Goods: https://shop.test/early"}}))).await;
    assert!(r["body"].as_str().unwrap().contains("https://shop.test/early"), "the preview matches the send: {r}");
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    flow_text(&mut h, "Later", "Acme Goods: later https://shop.test/later", &phone).await;
    let later = body_to(&h, &phone);
    assert_eq!(short_paths(&later).len(), 1, "{later}");
    assert_eq!(h.ok("GET", "/api/short-links/settings", None).await["ready"], true);
    h.cleanup().await;
}

/// Wait until the current clock minute has room for a burst (the limits count in fixed minutes).
async fn clear_of_minute_edge() {
    let s = chrono::Timelike::second(&chrono::Utc::now());
    if s > 45 {
        tokio::time::sleep(std::time::Duration::from_secs(u64::from(61 - s))).await;
    }
}

async fn click_from(h: &Harness, path: &str, ip: &str) -> StatusCode {
    h.raw("GET", &format!("/s/{path}"), &[("x-real-ip", ip)], "").await.0
}

#[tokio::test]
async fn guessing_is_limited_per_address_and_valid_clicks_never_count() {
    let mut h = harness!();
    let (_, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 7901).await;
    let id = sms_campaign(&mut h, &list, "Acme Goods: https://shop.test/limited").await;
    launch(&mut h, &id).await;
    let path = short_paths(&body_to(&h, &phone))[0].clone();
    let mut wrong = path.clone().into_bytes();
    wrong[code::PATH_LEN - 1] = if wrong[code::PATH_LEN - 1] == b'a' { b'b' } else { b'a' };
    let wrong = String::from_utf8(wrong).unwrap();
    clear_of_minute_edge().await;
    // Valid clicks, however many, never count against an address.
    for _ in 0..40 {
        assert!(click_from(&h, &path, "198.51.100.7").await.is_redirection());
    }
    // 30 misses from one address are answered; then it gets 429 without a lookup, valid link or not.
    for i in 0..30 {
        assert_eq!(click_from(&h, &wrong, "198.51.100.8").await, StatusCode::NOT_FOUND, "miss {i}");
    }
    assert_eq!(counted(click_from(&h, &wrong, "198.51.100.8")).await, (StatusCode::TOO_MANY_REQUESTS, 0));
    let (status, queries) = counted(click_from(&h, &path, "198.51.100.8")).await;
    assert_eq!((status, queries), (StatusCode::TOO_MANY_REQUESTS, 0), "refused before any query");
    // Other addresses are unaffected.
    assert!(click_from(&h, &path, "198.51.100.9").await.is_redirection());
    h.cleanup().await;
}

#[tokio::test]
async fn without_its_index_a_server_answers_not_found_without_a_query() {
    let mut h = harness!();
    let (_, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 7902).await;
    let id = sms_campaign(&mut h, &list, "Acme Goods: https://shop.test/indexed").await;
    launch(&mut h, &id).await;
    let path = short_paths(&body_to(&h, &phone))[0].clone();
    assert!(click(&h, &path).await.0.is_redirection());
    // The index goes missing; the scheduler's check notices.
    sqlx::query(&format!("DROP INDEX {}", short_links::INDEXES[0].0)).execute(&h.state.db).await.unwrap();
    assert!(!short_links::check_index(&h.state).await);
    assert!(!short_links::issuing(&h.state), "and no new short links are issued");
    let ((status, _, _), queries) = counted(click(&h, &path)).await;
    assert_eq!((status, queries), (StatusCode::NOT_FOUND, 0), "no lookup that would scan messages");
    // Rebuilt: links resolve again.
    short_links::ensure_index(&h.state).await.unwrap();
    assert!(click(&h, &path).await.0.is_redirection());
    h.cleanup().await;
}

#[tokio::test]
async fn shortening_changes_nothing_but_the_link() {
    let mut h = harness!();
    let (_, list) = setup(&mut h).await;
    let (_, phone) = texter(&mut h, &list, 8001).await;
    let id = sms_campaign(&mut h, &list, "Acme Goods: wow https://shop.test/deal! and {{ 'https://shop.test/deal!' }} or https://shop.test/unsubscribe-me").await;
    launch(&mut h, &id).await;
    let body = body_to(&h, &phone);
    // Only the link written literally is shortened: the same link produced by the template, and a
    // link about unsubscribing, keep the long form that shows where they go.
    let p = short_paths(&body);
    assert_eq!(p.len(), 1, "{body}");
    assert!(body.starts_with(&format!("Acme Goods: wow https://pl.test/s/{} and https://pl.test/t/c/", p[0])), "{body}");
    assert_eq!(body.matches("/t/c/").count(), 2, "{body}");
    // The destination is exactly what was replaced, `!` included.
    let (s, location, _) = click(&h, &p[0]).await;
    assert!(s.is_redirection());
    assert_eq!(location.as_deref(), Some("https://shop.test/deal!"));
    h.cleanup().await;
}

#[tokio::test]
async fn the_send_and_every_estimate_decide_the_same_way() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    h.ok("PUT", "/api/link-tags", Some(json!({"enabled": true}))).await;
    let (_, phone) = texter(&mut h, &list, 8101).await;
    // A link just under the limit: its tags would take it over, so it's shortened untagged, and the
    // preview (which knows no tags) shows the same short link the send uses.
    let near = format!("https://shop.test/p?{}", "a".repeat(2000 - "https://shop.test/p?".len()));
    let body = format!("Acme Goods: {near}");
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": body}}))).await;
    assert_eq!(short_paths(r["body"].as_str().unwrap()).len(), 1, "{r}");
    let id = sms_campaign(&mut h, &list, &body).await;
    launch(&mut h, &id).await;
    let sent = body_to(&h, &phone);
    let p = short_paths(&sent);
    assert_eq!(p.len(), 1, "{sent}");
    assert_eq!(sent.chars().count(), r["body"].as_str().unwrap().chars().count());
    assert_eq!(click(&h, &p[0]).await.1.as_deref(), Some(near.as_str()), "untagged: the tags didn't fit");

    // The launch check counts the short link: one segment each, not the raw template's two.
    let list2 = h.ok("POST", "/api/lists", Some(json!({"name": "Two"}))).await["id"].as_str().unwrap().to_string();
    texter(&mut h, &list2, 8102).await;
    texter(&mut h, &list2, 8103).await;
    let link = format!("https://shop.test/collections/{}", "winter-".repeat(20));
    let body = format!("Acme Goods: new arrivals {link}");
    assert_eq!(dumbmail::providers::sms::segment_count(&format!("Acme Goods: {body} Reply STOP to opt out.")).1, 2);
    let id = sms_campaign(&mut h, &list2, &body).await;
    let one_segment_each = 2 * dumbmail::billing::to_micros(0.0099);
    sqlx::query("UPDATE accounts SET balance_micros = $2 WHERE id = $1").bind(acct).bind(one_segment_each).execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/campaigns/{id}/send"), None).await;
    assert!(s.is_success(), "{s}: {v}");
    h.cleanup().await;
}

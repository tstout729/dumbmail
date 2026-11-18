//! Link tagging (UTM), resends to non-openers, tags and archive, list filters and the calendar.

#[macro_use]
mod common;

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use dumbmail::features::campaigns_plus::{calendar, resend, tags, utm};
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

/// A profile on `list` with `consent`.
async fn person(h: &mut Harness, acct: Uuid, list: &str, email: &str, consent: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, $3, $4)").bind(id).bind(acct).bind(email).bind(consent).execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/lists/{list}/members"), Some(json!({"profile_ids": [id]}))).await;
    id
}

async fn setup(h: &mut Harness) -> (Uuid, String) {
    h.signup().await;
    h.credit(20.0).await;
    let acct = account_id(h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Everyone"}))).await["id"].as_str().unwrap().to_string();
    (acct, list)
}

fn email_content(subject: &str) -> Value {
    json!({"subject": subject, "blocks": [
        {"type": "text", "html": "<p><a href=\"https://shop.test/a?x=1#top\">Shop</a> <a href=\"https://skip.test/p\">Skip</a> <a href=\"mailto:a@b.test\">Mail</a> <a href=\"https://shop.test/b?utm_source=ig\">B</a></p>"},
        {"type": "button", "text": "Go", "href": "https://shop.test/c"}
    ]})
}

async fn create_campaign(h: &mut Harness, list: &str, name: &str) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": name, "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": email_content("Hello")}))).await;
    c["id"].as_str().unwrap().to_string()
}

async fn send(h: &mut Harness, id: &str) {
    h.ok("POST", &format!("/api/campaigns/{id}/send"), None).await;
    h.drain().await;
}

/// Where each click-tracked link in `html` goes.
fn destinations(html: &str) -> Vec<String> {
    html.split("/t/c/")
        .skip(1)
        .map(|rest| {
            let href = rest.split('"').next().unwrap().replace("&amp;", "&");
            let q = href.split_once('?').unwrap().1;
            url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "u").unwrap().1.into_owned()
        })
        .collect()
}

fn short(id: &str) -> String {
    id.replace('-', "")[..8].to_string()
}

#[tokio::test]
async fn campaign_links_get_utm_tags_and_dumbmail_links_never_do() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;

    // Off by default: nothing changes.
    let plain = create_campaign(&mut h, &list, "Plain one").await;
    send(&mut h, &plain).await;
    let sent = h.state.email.sent();
    assert!(destinations(&sent.last().unwrap().html).iter().all(|d| !d.contains("utm_medium")), "no tags while the account has tagging off");
    let content: Value = sqlx::query_scalar("SELECT content FROM campaigns WHERE id = $1").bind(plain.parse::<Uuid>().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert!(content.get(utm::CONTENT_KEY).is_none(), "untagged content isn't touched");

    let s = h.ok("PUT", "/api/link-tags", Some(json!({"enabled": true, "params": {"content": "{version}{send_date}", "custom": [{"name": "ref", "value": "{channel}-x"}]}, "skip_domains": ["skip.test"]}))).await;
    assert_eq!(s["enabled"], true);
    let (st, err) = h.req("PUT", "/api/link-tags", Some(json!({"params": {"source": "{oops}"}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");

    let id = create_campaign(&mut h, &list, "Black Friday!").await;
    let preview = h.ok("GET", &format!("/api/campaigns/{id}/link-tags"), None).await;
    assert_eq!(preview["tagged"], true);
    assert!(preview["preview"].as_str().unwrap().contains(&format!("utm_campaign=black-friday-{}", short(&id))), "{preview}");
    send(&mut h, &id).await;
    let m = h.state.email.sent().last().unwrap().clone();
    let today = utm::local_date("America/Los_Angeles", Utc::now());
    let tags = format!("utm_source=dumbmail&utm_medium=email&utm_campaign=black-friday-{}&utm_content={today}&ref=email-x", short(&id));
    let dest = destinations(&m.html);
    assert!(dest.contains(&format!("https://shop.test/a?x=1&{tags}#top")), "query and fragment kept: {dest:?}");
    assert!(dest.contains(&format!("https://shop.test/c?{tags}")), "{dest:?}");
    assert!(dest.contains(&"https://skip.test/p".to_string()), "a don't-tag domain is left alone: {dest:?}");
    assert!(dest.iter().any(|d| d.starts_with("https://shop.test/b?utm_source=ig&utm_medium=email")), "an existing parameter is kept: {dest:?}");
    assert!(m.html.contains("href=\"mailto:a@b.test\""), "mailto stays as is");
    assert!(!m.html.contains("/u/") || m.html.split("/u/").skip(1).all(|r| !r.split('"').next().unwrap().contains("utm_")), "the unsubscribe link isn't tagged");
    // A click goes to the tagged destination.
    let link = format!("https://pl.test/t/c/{}", m.html.split("/t/c/").nth(1).unwrap().split('"').next().unwrap().replace("&amp;", "&"));
    let (st, headers, _) = h.raw("GET", link.trim_start_matches("https://pl.test"), &[], "").await;
    assert!(st.is_redirection(), "{st}");
    assert!(headers["location"].to_str().unwrap().contains("utm_source=dumbmail"));

    // Off for one campaign; custom for another.
    let off = create_campaign(&mut h, &list, "Off one").await;
    h.ok("PUT", &format!("/api/campaigns/{off}/link-tags"), Some(json!({"mode": "off"}))).await;
    send(&mut h, &off).await;
    assert!(destinations(&h.state.email.sent().last().unwrap().html).iter().all(|d| !d.contains("utm_medium")));
    let custom = create_campaign(&mut h, &list, "Custom one").await;
    h.ok("PUT", &format!("/api/campaigns/{custom}/link-tags"), Some(json!({"mode": "custom", "params": {"source": "newsletter", "campaign": "{message_name}"}}))).await;
    // Test sends carry the tags too.
    h.ok("POST", &format!("/api/campaigns/{custom}/test"), Some(json!({"to": ["owner@acme.test"]}))).await;
    assert!(destinations(&h.state.email.sent().last().unwrap().html).contains(&"https://shop.test/c?utm_source=newsletter&utm_medium=email&utm_campaign=Custom+one".to_string()));
    send(&mut h, &custom).await;
    assert!(destinations(&h.state.email.sent().last().unwrap().html).contains(&"https://shop.test/c?utm_source=newsletter&utm_medium=email&utm_campaign=Custom+one".to_string()));
    // A sent campaign's choice can't change.
    let (st, _) = h.req("PUT", &format!("/api/campaigns/{custom}/link-tags"), Some(json!({"mode": "off"}))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    h.cleanup().await;
}

#[tokio::test]
async fn tags_typed_into_content_by_hand_are_replaced_at_send() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    let mut content = email_content("Hi");
    content[utm::CONTENT_KEY] = json!({"params": [["utm_source", "forged"]], "skip_domains": []});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Forged", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    send(&mut h, c["id"].as_str().unwrap()).await;
    assert!(!h.state.email.sent().last().unwrap().html.contains("forged"), "tagging is off, so the hand-made key is dropped at send");
    h.cleanup().await;
}

#[tokio::test]
async fn flow_messages_are_tagged_from_the_flow_row() {
    let mut h = harness!();
    let (_acct, _list) = setup(&mut h).await;
    h.ok("PUT", "/api/link-tags", Some(json!({"enabled": true}))).await;
    let flow = h
        .ok("POST", "/api/flows", Some(json!({"name": "Welcome series", "trigger": {"type": "metric", "metric": "Signed Up"}, "definition": {"entry": "e1", "nodes": [
            {"id": "e1", "type": "email", "name": "Day one", "next": null, "content": email_content("Welcome")}
        ]}})))
        .await;
    let fid = flow["id"].as_str().unwrap().to_string();
    h.ok("PUT", &format!("/api/flows/{fid}/link-tags"), Some(json!({"mode": "custom", "params": {"campaign": "{flow_name}", "content": "{message_name}"}}))).await;
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Signed Up", "profile": {"email": "new@x.test", "email_consent": "subscribed"}}))).await;
    h.drain().await;
    let m = h.state.email.sent().into_iter().find(|e| e.to == "new@x.test").expect("flow email");
    assert!(destinations(&m.html).contains(&"https://shop.test/c?utm_source=dumbmail&utm_medium=email&utm_campaign=Welcome+series&utm_content=Day+one".to_string()), "{:?}", destinations(&m.html));
    h.cleanup().await;
}

#[tokio::test]
async fn sms_links_are_tagged_at_the_destination() {
    let mut h = harness!();
    let (acct, _) = setup(&mut h).await;
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let tagger = utm::Tagger::new(vec![("utm_source".into(), "dumbmail".into()), ("utm_medium".into(), "sms".into())], vec![], vec!["pl.test".into()]);
    let t = dumbmail::render::Tracking { public_url: "https://pl.test", secret: "s", message_id: Uuid::nil(), unsubscribe_url: "https://pl.test/u/x".into() };
    let content = dumbmail::render::SmsContent { body: "Sale! https://shop.test/s?a=1 Reply STOP to opt out".into(), media_url: None };
    let plain = dumbmail::render::render_sms(&account, &content, &json!({}), Some(&t), false).unwrap();
    let tagged = dumbmail::render::render_sms_hooked(&account, &content, &json!({}), Some(&t), false, Some(&tagger)).unwrap();
    assert_ne!(plain, tagged);
    let u = |body: &str| {
        let link = body.split_whitespace().find(|w| w.contains("/t/c/")).unwrap().to_string();
        url::form_urlencoded::parse(link.split_once('?').unwrap().1.as_bytes()).find(|(k, _)| k == "u").unwrap().1.into_owned()
    };
    assert_eq!(u(&tagged), "https://shop.test/s?a=1&utm_source=dumbmail&utm_medium=sms");
    assert_eq!(u(&plain), "https://shop.test/s?a=1");
    h.cleanup().await;
}

/// A second account, signed in, in the same database (never cleaned up on its own).
async fn other_account(h: &Harness) -> Harness {
    let mut o = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    o.ok("POST", "/api/auth/signup", Some(json!({"email": "owner@other.test", "password": "correct horse", "company": "Other Co"}))).await;
    o
}

/// Backdate a sent campaign as if it finished `hours` ago.
async fn age(h: &Harness, id: &str, hours: i64) {
    let id: Uuid = id.parse().unwrap();
    let at = Utc::now() - Duration::hours(hours);
    sqlx::query("UPDATE campaigns SET sent_at = $2 WHERE id = $1").bind(id).bind(at).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE messages SET sent_at = $2 WHERE campaign_id = $1 AND sent_at IS NOT NULL").bind(id).bind(at).execute(&h.state.db).await.unwrap();
}

async fn message_of(h: &Harness, campaign: &str, profile: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM messages WHERE campaign_id = $1 AND profile_id = $2").bind(campaign.parse::<Uuid>().unwrap()).bind(profile).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn resend_goes_only_to_subscribed_non_openers_fixed_at_send_start() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let opened = person(&mut h, acct, &list, "opened@x.test", "subscribed").await;
    let clicked = person(&mut h, acct, &list, "clicked@x.test", "subscribed").await;
    let quiet = person(&mut h, acct, &list, "quiet@x.test", "subscribed").await;
    let later_unsub = person(&mut h, acct, &list, "unsub@x.test", "subscribed").await;
    let never = person(&mut h, acct, &list, "never@x.test", "never_subscribed").await;
    let bounced = person(&mut h, acct, &list, "bounced@x.test", "subscribed").await;
    let orig = create_campaign(&mut h, &list, "Launch").await;
    send(&mut h, &orig).await;
    // Someone who joins the list later never got the original, so never gets the resend.
    person(&mut h, acct, &list, "joined-later@x.test", "subscribed").await;

    let m = message_of(&h, &orig, opened).await;
    sqlx::query("UPDATE messages SET opened_at = now() WHERE id = $1").bind(m).execute(&h.state.db).await.unwrap();
    let m = message_of(&h, &orig, clicked).await;
    sqlx::query("UPDATE messages SET clicked_at = now() WHERE id = $1").bind(m).execute(&h.state.db).await.unwrap();
    let m = message_of(&h, &orig, bounced).await;
    sqlx::query("UPDATE messages SET status = 'bounced' WHERE id = $1").bind(m).execute(&h.state.db).await.unwrap();

    // Too soon: opens need time to come in.
    let (st, err) = h.req("POST", &format!("/api/campaigns/{orig}/resend"), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{err}");
    age(&h, &orig, 5).await;
    let info = h.ok("GET", &format!("/api/campaigns/{orig}/resends"), None).await;
    assert_eq!(info["can_resend"]["ok"], true, "{info}");

    let r = h.ok("POST", &format!("/api/campaigns/{orig}/resend"), Some(json!({"subject": "In case you missed it"}))).await;
    let rid = r["id"].as_str().unwrap().to_string();
    assert_eq!(r["status"], "draft");
    assert_eq!(r["content"]["subject"], "In case you missed it");
    assert_eq!(r["audience"]["include"][0]["type"], "resend");
    let est = h.ok("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": r["audience"]}))).await;
    assert_eq!(est["count"], 2, "quiet and unsub: {est}");

    // A resend can't be resent.
    let (st, _) = h.req("POST", &format!("/api/campaigns/{rid}/resend"), None).await;
    assert_eq!(st, StatusCode::CONFLICT);

    // Unsubscribes after the original: never gets the resend.
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed' WHERE id = $1").bind(later_unsub).execute(&h.state.db).await.unwrap();
    let before = h.state.email.sent().len();
    send(&mut h, &rid).await;
    let got: Vec<String> = h.state.email.sent()[before..].iter().map(|e| e.to.clone()).collect();
    assert_eq!(got, vec!["quiet@x.test".to_string()], "never_subscribed ({never}), bounced, openers and clickers are left out");
    assert_eq!(h.state.email.sent().last().unwrap().subject, "In case you missed it");

    // The audience was fixed when the send started: a retry adds nobody.
    let rid_u: Uuid = rid.parse().unwrap();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(rid_u).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE messages SET opened_at = NULL WHERE campaign_id = $1").bind(orig.parse::<Uuid>().unwrap()).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, rid_u).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1").bind(rid_u).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1, "not recomputed on retry");

    // The original shows its resend and totals that count each person once.
    let m = message_of(&h, &orig, opened).await;
    sqlx::query("UPDATE messages SET opened_at = now() WHERE id = $1").bind(m).execute(&h.state.db).await.unwrap();
    let rm = message_of(&h, &rid, quiet).await;
    sqlx::query("UPDATE messages SET opened_at = now() WHERE id = $1").bind(rm).execute(&h.state.db).await.unwrap();
    let info = h.ok("GET", &format!("/api/campaigns/{orig}/resends"), None).await;
    assert_eq!(info["resends"].as_array().unwrap().len(), 1);
    assert_eq!(info["combined"]["people_reached"], 5, "{info}");
    assert_eq!(info["combined"]["people_opened"], 3, "opened, clicked, quiet (via the resend): {info}");
    assert_eq!(info["combined"]["people_clicked"], 1);
    let of = h.ok("GET", &format!("/api/campaigns/{rid}/resends"), None).await;
    assert_eq!(of["resend_of"]["id"], orig.as_str());
    h.cleanup().await;
}

#[tokio::test]
async fn resend_guardrails() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    let orig = create_campaign(&mut h, &list, "Old one").await;
    send(&mut h, &orig).await;
    age(&h, &orig, 24 * 31).await;
    let (st, err) = h.req("POST", &format!("/api/campaigns/{orig}/resend"), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "more than 30 days: {err}");
    // A draft can't be resent.
    let draft = create_campaign(&mut h, &list, "Draft").await;
    let (st, _) = h.req("POST", &format!("/api/campaigns/{draft}/resend"), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    // Another campaign can't borrow a resend's audience.
    age(&h, &orig, 6).await;
    let r = h.ok("POST", &format!("/api/campaigns/{orig}/resend"), Some(json!({"target": "no_click"}))).await;
    let borrow = h.ok("POST", "/api/campaigns", Some(json!({"name": "Borrow", "channel": "email", "audience": r["audience"], "content": email_content("x")}))).await;
    let (st, err) = h.req("POST", &format!("/api/campaigns/{}/send", borrow["id"].as_str().unwrap()), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("resend"), "{err}");
    let (st, _) = h.req("POST", "/api/campaigns/estimate", Some(json!({"channel": "email", "audience": {"include": [{"type": "resend", "id": borrow["id"]}]}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a campaign that isn't a resend has no resend audience");
    // Another account can't resend this campaign.
    let mut other = other_account(&h).await;
    let (st, _) = other.req("POST", &format!("/api/campaigns/{orig}/resend"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_resends_make_at_most_two() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    let orig = create_campaign(&mut h, &list, "Popular").await;
    send(&mut h, &orig).await;
    age(&h, &orig, 5).await;
    let orig: Uuid = orig.parse().unwrap();
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let state = h.state.clone();
            tokio::spawn(async move { resend::create(&state, acct, orig, Default::default()).await.is_ok() })
        })
        .collect();
    let mut made = 0;
    for t in tasks {
        if t.await.unwrap() {
            made += 1;
        }
    }
    assert_eq!(made, 2);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM campaign_resends WHERE original_id = $1").bind(orig).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 2);
    let drafts: i64 = sqlx::query_scalar("SELECT count(*) FROM campaigns WHERE audience->'include'->0->>'type' = 'resend'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(drafts, 2, "the losers' drafts were rolled back");
    h.cleanup().await;
}

#[tokio::test]
async fn tags_archive_and_list_filters() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    let a = create_campaign(&mut h, &list, "Spring sale").await;
    let b = create_campaign(&mut h, &list, "Summer sale").await;
    let c = create_campaign(&mut h, &list, "Newsletter").await;
    let r = h.ok("POST", "/api/campaigns/tags", Some(json!({"ids": [a, b], "add": ["Promo", " promo ", "2026"]}))).await;
    assert_eq!(r["data"][0]["tags"].as_array().unwrap().len(), 2, "names are matched without case: {r}");
    let tags = h.ok("GET", "/api/tags", None).await;
    let promo = tags["data"].as_array().unwrap().iter().find(|t| t["name"] == "Promo").unwrap()["id"].as_str().unwrap().to_string();

    let list_of = |v: &Value| v["data"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let all = h.ok("GET", "/api/campaigns", None).await;
    assert_eq!(all["total"], 3);
    assert!(all["data"][0]["tags"].is_array());
    let by_tag = h.ok("GET", &format!("/api/campaigns?tag={promo}"), None).await;
    assert_eq!(by_tag["total"], 2);
    let by_text = h.ok("GET", "/api/campaigns?q=summer", None).await;
    assert_eq!(list_of(&by_text), vec!["Summer sale"]);
    let by_subject = h.ok("GET", "/api/campaigns?q=hello", None).await;
    assert_eq!(by_subject["total"], 3, "the subject is searched too");
    let paged = h.ok("GET", "/api/campaigns?limit=1&offset=1", None).await;
    assert_eq!((paged["data"].as_array().unwrap().len(), paged["total"].as_i64()), (1, Some(3)));
    let today = utm::local_date("America/Los_Angeles", Utc::now());
    assert_eq!(h.ok("GET", &format!("/api/campaigns?from={today}&to={today}"), None).await["total"], 3);
    assert_eq!(h.ok("GET", "/api/campaigns?from=2020-01-01&to=2020-01-02", None).await["total"], 0);
    let (st, _) = h.req("GET", "/api/campaigns?tag=nope", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    h.ok("POST", "/api/campaigns/tags", Some(json!({"ids": [b], "remove": ["promo"]}))).await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns?tag={promo}"), None).await["total"], 1);

    // Archive: hidden by default, still listed when asked, can't be sent.
    let r = h.ok("POST", "/api/campaigns/archive", Some(json!({"ids": [c]}))).await;
    assert_eq!(r["changed"].as_array().unwrap().len(), 1);
    assert_eq!(h.ok("GET", "/api/campaigns", None).await["total"], 2);
    assert_eq!(list_of(&h.ok("GET", "/api/campaigns?archived=true", None).await), vec!["Newsletter"]);
    assert_eq!(h.ok("GET", "/api/campaigns?archived=all", None).await["total"], 3);
    let (st, err) = h.req("POST", &format!("/api/campaigns/{c}/send"), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{err}");
    assert!(err.to_string().contains("archived"));
    let counts = h.ok("GET", "/api/campaigns/counts", None).await;
    assert_eq!((counts["by_status"]["draft"].as_i64(), counts["archived"].as_i64()), (Some(2), Some(1)));
    h.ok("POST", "/api/campaigns/archive", Some(json!({"ids": [c], "archived": false}))).await;
    assert_eq!(h.ok("GET", "/api/campaigns", None).await["total"], 3);

    // A scheduled campaign isn't archived.
    let at = (Utc::now() + Duration::days(2)).to_rfc3339();
    h.ok("POST", &format!("/api/campaigns/{a}/schedule"), Some(json!({"send_at": at}))).await;
    let r = h.ok("POST", "/api/campaigns/archive", Some(json!({"ids": [a]}))).await;
    assert_eq!(r["changed"].as_array().unwrap().len(), 0);
    assert!(r["skipped"][0]["reason"].as_str().unwrap().contains("scheduled"), "{r}");

    // Limits: 20 per item, checked in the write.
    let many: Vec<String> = (0..19).map(|i| format!("t{i}")).collect();
    let (st, err) = h.req("POST", "/api/campaigns/tags", Some(json!({"ids": [b], "add": many}))).await;
    assert!(st.is_success(), "{err}");
    let (st, _) = h.req("POST", "/api/campaigns/tags", Some(json!({"ids": [b], "add": ["one more"]}))).await;
    assert!(!st.is_success(), "over the limit is refused");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM item_tags WHERE campaign_id = $1").bind(b.parse::<Uuid>().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 20, "nothing was added");
    let (st, _) = h.req("POST", "/api/campaigns/tags", Some(json!({"ids": [a], "add": ["x".repeat(41)]}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Templates and flows: same tags, own archive rules.
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "Base", "channel": "email", "content": email_content("T")}))).await;
    let tid = t["id"].as_str().unwrap();
    h.ok("POST", "/api/templates/tags", Some(json!({"ids": [tid], "add": ["Promo"]}))).await;
    assert_eq!(h.ok("GET", &format!("/api/templates?tag={promo}"), None).await["total"], 1);
    h.ok("POST", "/api/templates/archive", Some(json!({"ids": [tid]}))).await;
    assert_eq!(h.ok("GET", "/api/templates", None).await["total"], 0);
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Winback", "trigger": {"type": "metric", "metric": "X"}, "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "next": null, "content": email_content("F")}]}}))).await;
    let fid = flow["id"].as_str().unwrap();
    h.ok("POST", "/api/flows/archive", Some(json!({"ids": [fid]}))).await;
    let (st, err) = h.req("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{err}");
    assert_eq!(h.ok("GET", "/api/flows", None).await["total"], 0);

    // Deleting a campaign takes its tags with it; deleting a tag takes it off everything.
    h.ok("DELETE", &format!("/api/tags/{promo}"), None).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM item_tags WHERE tag_id = $1").bind(promo.parse::<Uuid>().unwrap()).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);

    // Another account's ids are refused.
    let mut other = other_account(&h).await;
    let (st, _) = other.req("POST", "/api/campaigns/tags", Some(json!({"ids": [b], "add": ["mine"]}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = other.req("POST", "/api/campaigns/archive", Some(json!({"ids": [b]}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(other.ok("GET", "/api/tags", None).await["data"].as_array().unwrap().len(), 0);
    h.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_tag_limit_holds_under_racing_requests() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    let mut ids = Vec::new();
    for i in 0..12 {
        ids.push(create_campaign(&mut h, &list, &format!("c{i}")).await.parse::<Uuid>().unwrap());
    }
    // 190 tags already; 12 requests at once each add one new tag.
    for i in 0..190 {
        sqlx::query("INSERT INTO tags (id, account_id, name) VALUES ($1, $2, $3)").bind(Uuid::now_v7()).bind(acct).bind(format!("old{i}")).execute(&h.state.db).await.unwrap();
    }
    let tasks: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let state = h.state.clone();
            let id = *id;
            tokio::spawn(async move { tags::apply(&state, acct, tags::Kind::Campaign, tags::ApplyBody { ids: vec![id], add: vec![format!("new{i}")], remove: vec![] }).await.is_ok() })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        ok += t.await.unwrap() as i32;
    }
    assert_eq!(ok, 10);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tags WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 200);
    h.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_and_schedule_racing_never_both_win() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    for i in 0..8 {
        let id: Uuid = create_campaign(&mut h, &list, &format!("race {i}")).await.parse().unwrap();
        let (s1, s2) = (h.state.clone(), h.state.clone());
        let at = Utc::now() + Duration::days(1);
        let sched = tokio::spawn(async move { dumbmail::api::messaging::launch(&s1, acct, id, Some(at), None).await.is_ok() });
        let arch = tokio::spawn(async move { tags::archive(&s2, acct, tags::Kind::Campaign, tags::ArchiveBody { ids: vec![id], archived: Some(true) }).await.unwrap()["changed"].as_array().unwrap().len() == 1 });
        let (scheduled, archived) = (sched.await.unwrap(), arch.await.unwrap());
        let (status, is_archived): (String, bool) = sqlx::query_as("SELECT status, archived_at IS NOT NULL FROM campaigns WHERE id = $1").bind(id).fetch_one(&h.state.db).await.unwrap();
        assert!(!(status == "scheduled" && is_archived), "round {i}: scheduled and archived");
        assert!(!(scheduled && archived), "round {i}: both said they won");
        assert_eq!(status == "scheduled", scheduled);
        assert_eq!(is_archived, archived);
    }
    h.cleanup().await;
}

thread_local! {
    /// SQL statements sqlx ran on this thread while counting (None: not counting).
    static STATEMENTS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Counts sqlx's statement events on the thread that asked. A process-wide subscriber, set once,
/// so callsite caching across the parallel test threads can't hide events.
struct Statements;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Statements {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if e.metadata().target() == "sqlx::query" {
            STATEMENTS.with(|c| c.set(c.get().map(|n| n + 1)));
        }
    }
}

fn count_statements() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Statements));
    });
    STATEMENTS.with(|c| c.set(Some(0)));
}

fn statements() -> usize {
    STATEMENTS.with(|c| c.replace(Some(0))).unwrap_or(0)
}

#[tokio::test]
async fn listing_campaigns_without_filters_runs_the_same_statements_as_before() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    for i in 0..4 {
        create_campaign(&mut h, &list, &format!("c{i}")).await;
    }
    count_statements();
    let rows = dumbmail::api::messaging::list_campaigns(&h.state, acct, &dumbmail::api::messaging::ChannelQuery { channel: None, status: None }).await.unwrap();
    assert_eq!(rows.len(), 4);
    // origin/main (d200260): one SELECT for the list, then two per campaign for its stats
    // (`campaigns::stats`). Tags, archive and resend details ride on the one list statement.
    assert_eq!(statements(), 1 + 2 * 4);
    let (rows, _) = tags::campaigns(&h.state, acct, &Default::default()).await.unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(statements(), 1 + 2 * 4, "the REST list too");
    h.cleanup().await;
}

#[tokio::test]
async fn calendar_places_campaigns_by_local_day_and_moves_them() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    let tz = chrono_tz::America::Los_Angeles;
    let day = (Utc::now().with_timezone(&tz) + Duration::days(3)).date_naive();
    let at = dumbmail::features::sendtime::clock::local_instant(tz, day, 9 * 60 + 30);
    let id = create_campaign(&mut h, &list, "Planned").await;
    h.ok("POST", &format!("/api/campaigns/{id}/schedule"), Some(json!({"send_at": at.to_rfc3339()}))).await;
    let sent = create_campaign(&mut h, &list, "Already out").await;
    send(&mut h, &sent).await;

    let from = day - Duration::days(5);
    let cal = h.ok("GET", &format!("/api/campaigns/calendar?from={from}&to={}", day + Duration::days(5)), None).await;
    let items = cal["items"].as_array().unwrap();
    let planned = items.iter().find(|i| i["id"] == id.as_str()).unwrap();
    assert_eq!((planned["date"].as_str(), planned["time"].as_str(), planned["can_move"].as_bool()), (Some(day.to_string().as_str()), Some("09:30"), Some(true)), "{planned}");
    assert!(items.iter().any(|i| i["id"] == sent.as_str() && i["status"] == "sent" && i["can_move"] == false));
    let (st, _) = h.req("GET", &format!("/api/campaigns/calendar?from={from}&to={}", from + Duration::days(62)), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "at most 62 days");

    // Drag to two days later: same time of day.
    let to = day + Duration::days(2);
    let moved = h.ok("POST", &format!("/api/campaigns/{id}/reschedule"), Some(json!({"date": to.to_string()}))).await;
    let new_at: chrono::DateTime<Utc> = moved["send_at"].as_str().unwrap().parse().unwrap();
    assert_eq!(new_at.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string(), format!("{to} 09:30"));
    let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'campaign.reschedule'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(audit, 1);
    // Not to a day that would send it now, and not a campaign that's about to go.
    let yesterday = (Utc::now().with_timezone(&tz) - Duration::days(1)).date_naive();
    let (st, _) = h.req("POST", &format!("/api/campaigns/{id}/reschedule"), Some(json!({"date": yesterday.to_string()}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    sqlx::query("UPDATE campaigns SET send_at = now() + interval '10 minutes' WHERE id = $1").bind(id.parse::<Uuid>().unwrap()).execute(&h.state.db).await.unwrap();
    let (st, _) = h.req("POST", &format!("/api/campaigns/{id}/reschedule"), Some(json!({"date": to.to_string()}))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = h.req("POST", &format!("/api/campaigns/{sent}/reschedule"), Some(json!({"date": to.to_string()}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "a sent campaign can't move");

    // Local-time sends move their day too.
    let c = dumbmail::campaigns::load(&h.state.db, acct, id.parse().unwrap()).await.unwrap();
    let mut c2 = c.clone();
    c2.send_options = Some(json!({"timing": "local_time", "local_time": "10:00", "local_date": day.to_string()}));
    c2.send_at = Some(Utc::now() + Duration::days(3));
    let (send_at, start) = calendar::plan_move(&h.state, acct, &c2, c2.send_at.unwrap().with_timezone(&tz).date_naive() + Duration::days(1)).await.unwrap();
    assert_eq!(send_at.with_timezone(&tz).date_naive(), day + Duration::days(1));
    assert!(start < send_at, "starts when the first time zone reaches 10:00");
    h.cleanup().await;
}

/// An API key and the MCP endpoint.
async fn mcp(h: &Harness, key: &str, tool: &str, args: Value) -> (bool, Value) {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}).to_string();
    let (st, _, text) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {key}")), ("content-type", "application/json")], &body).await;
    assert_eq!(st, StatusCode::OK, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let r = &v["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    (r["isError"].as_bool().unwrap_or(false), serde_json::from_str(text).unwrap_or(json!({"_text": text})))
}

#[tokio::test]
async fn mcp_tools_and_permissions() {
    let mut h = harness!();
    let (acct, list) = setup(&mut h).await;
    person(&mut h, acct, &list, "a@x.test", "subscribed").await;
    let full = h.ok("POST", "/api/api-keys", Some(json!({"name": "all"}))).await["key"].as_str().unwrap().to_string();
    let read = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let orig = create_campaign(&mut h, &list, "Via MCP").await;
    send(&mut h, &orig).await;
    age(&h, &orig, 5).await;

    let (err, v) = mcp(&h, &full, "update_link_tagging", json!({"enabled": true, "skip_domains": ["skip.test"]})).await;
    assert!(!err, "{v}");
    let (err, v) = mcp(&h, &full, "create_resend", json!({"campaign_id": orig, "target": "no_click"})).await;
    assert!(!err, "{v}");
    assert_eq!(v["status"], "draft", "a draft, nothing sent: {v}");
    let (err, v) = mcp(&h, &full, "tag_campaigns", json!({"ids": [orig], "add": ["Launch"]})).await;
    assert!(!err, "{v}");
    let (err, v) = mcp(&h, &full, "search_campaigns", json!({"q": "via"})).await;
    assert!(!err && v["total"] == 2, "the campaign and its resend: {v}");
    let tag = mcp(&h, &full, "list_tags", json!({})).await.1["data"][0]["id"].as_str().unwrap().to_string();
    let (err, v) = mcp(&h, &full, "delete_tag", json!({"tag_id": tag})).await;
    assert!(!err && v["dry_run"] == true, "{v}");
    let today = utm::local_date("America/Los_Angeles", Utc::now());
    let (err, v) = mcp(&h, &full, "campaign_calendar", json!({"from": today, "to": today})).await;
    assert!(!err && v["items"].as_array().unwrap().len() == 1, "the sent campaign; the resend draft has no date yet: {v}");

    // A read-only key can read but not change anything.
    let (err, _) = mcp(&h, &read, "campaign_calendar", json!({"from": today, "to": today})).await;
    assert!(!err);
    for (tool, args) in [
        ("create_resend", json!({"campaign_id": orig})),
        ("tag_campaigns", json!({"ids": [orig], "add": ["x"]})),
        ("archive_campaigns", json!({"ids": [orig]})),
        ("update_link_tagging", json!({"enabled": false})),
    ] {
        let (err, v) = mcp(&h, &read, tool, args).await;
        assert!(err, "{tool}: {v}");
    }
    let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'link_tags.update'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(audit, 1);
    h.cleanup().await;
}

//! Unique coupon codes: pools, giving codes out in messages and forms exactly once, holds when a
//! pool runs out, releases for unsent messages, redemptions from orders and segments.

#[macro_use]
mod common;

use axum::http::StatusCode;
use dumbmail::features::coupons::{self, store, Via};
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn subscriber(h: &Harness, acct: Uuid, email: &str, consent: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(acct)
        .bind(email)
        .bind(consent)
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

async fn coupon(h: &mut Harness, name: &str, codes: &[&str]) -> String {
    let c = h.ok("POST", "/api/coupons", Some(json!({"name": name, "description": "10% off"}))).await;
    let id = c["id"].as_str().unwrap().to_string();
    if !codes.is_empty() {
        h.ok("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"codes": codes}))).await;
    }
    id
}

async fn list_with(h: &mut Harness, emails: &[(&str, &str)]) -> String {
    let acct = account_id(h).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": format!("List {}", Uuid::now_v7())}))).await;
    let lid: Uuid = list["id"].as_str().unwrap().parse().unwrap();
    for (e, consent) in emails {
        let p = subscriber(h, acct, e, consent).await;
        sqlx::query("INSERT INTO list_members (list_id, profile_id) VALUES ($1, $2)").bind(lid).bind(p).execute(&h.state.db).await.unwrap();
    }
    lid.to_string()
}

fn coupon_email(name: &str) -> Value {
    json!({"subject": format!("Your code {{% coupon_code '{name}' %}}"), "blocks": [
        {"type": "text", "html": format!("<p>Hi! Use {{{{ coupon_code('{name}') }}}}.</p>")},
        {"type": "coupon", "coupon": name, "title": "Your code", "button_text": "Shop", "href": "https://acme.test"}
    ]})
}

async fn campaign(h: &mut Harness, list: &str, content: Value) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    c["id"].as_str().unwrap().to_string()
}

fn code_in(subject: &str) -> String {
    subject.trim_start_matches("Your code ").to_string()
}

async fn status_of(h: &Harness, code: &str) -> (String, Option<Uuid>, Option<bool>) {
    sqlx::query_as("SELECT status, profile_id, redeemed_verified FROM coupon_codes WHERE code = $1").bind(code).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn coupons_crud_uploads_generation_and_isolation() {
    let mut h = harness!();
    h.signup().await;
    let (s, v) = h.req("POST", "/api/coupons", Some(json!({"name": "bad{{name"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let id = coupon(&mut h, "WELCOME", &[]).await;
    let (s, _) = h.req("POST", "/api/coupons", Some(json!({"name": "welcome"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "names are unique ignoring case");
    let (s, _) = h.req("PATCH", &format!("/api/coupons/{id}"), Some(json!({"name": "OTHER"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the name templates use can't change");
    let (s, _) = h.req("PATCH", &format!("/api/coupons/{id}"), Some(json!({"expires_at": "2027-01-01T00:00:00Z", "expires_after_days": 30}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "one kind of expiry");

    // Pasted CSV: header skipped, duplicates in any case skipped, bad lines refused.
    let r = h.ok("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"text": "code\nSAVE-1\nsave-1\nSAVE-2\nhas space\nSAVE-3\n"}))).await;
    assert_eq!((r["added"].as_i64(), r["duplicates"].as_i64(), r["refused"].as_i64()), (Some(3), Some(0), Some(1)), "{r}");
    let r = h.ok("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"codes": ["save-2", "SAVE-4"]}))).await;
    assert_eq!((r["added"].as_i64(), r["duplicates"].as_i64()), (Some(1), Some(1)), "{r}");
    assert_eq!(r["coupon"]["stock"]["available"], 4);
    let too_many: Vec<String> = (0..200_001).map(|i| format!("C{i}")).collect();
    let (s, _) = h.req("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"codes": too_many}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "uploads are capped");

    // Generated codes from a pattern, without look-alikes.
    let g = h.ok("POST", "/api/coupons", Some(json!({"name": "GEN", "source": "generated", "pattern": {"prefix": "VIP-", "length": 8}}))).await;
    let gid = g["id"].as_str().unwrap();
    let r = h.ok("POST", &format!("/api/coupons/{gid}/generate"), Some(json!({"count": 500}))).await;
    assert_eq!(r["added"], 500);
    let page = h.ok("GET", &format!("/api/coupons/{gid}/codes?q=vip-&limit=200"), None).await;
    assert_eq!(page["total"], 500);
    for c in page["data"].as_array().unwrap() {
        let code = c["code"].as_str().unwrap();
        assert!(code.starts_with("VIP-") && code.len() == 12 && !code[4..].contains(['0', 'O', '1', 'I', 'L']), "{code}");
    }
    let (s, _) = h.req("POST", &format!("/api/coupons/{gid}/generate"), Some(json!({"count": 3000, "pattern": {"length": 4, "alphabet": "digits"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a pattern too small for the pool is refused");

    // Export streams every code as CSV.
    let (s, hd, body) = h.bytes("GET", &format!("/api/coupons/{gid}/export?status=available"), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(hd.get("content-type").unwrap().to_str().unwrap().starts_with("text/csv"));
    assert_eq!(String::from_utf8(body).unwrap().lines().count(), 501);

    let list = h.ok("GET", "/api/coupons", None).await;
    assert_eq!(list["total"], 2);
    assert_eq!(list["data"][1]["usage"]["tag"], "{% coupon_code 'WELCOME' %}");

    // Another account sees none of it and can't touch it.
    let mine = h.cookie.take();
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    assert_eq!(h.ok("GET", "/api/coupons", None).await["total"], 0);
    for (m, p, b) in [("GET", format!("/api/coupons/{id}"), None), ("GET", format!("/api/coupons/{id}/codes"), None), ("POST", format!("/api/coupons/{id}/codes"), Some(json!({"codes": ["X1"]}))), ("DELETE", format!("/api/coupons/{id}"), None)] {
        let (s, _) = h.req(m, &p, b).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {p}");
    }
    h.cookie = mine;

    // Deleting removes it at once (codes in the background) and frees the name.
    h.ok("DELETE", &format!("/api/coupons/{gid}"), None).await;
    assert_eq!(h.ok("GET", "/api/coupons", None).await["total"], 1);
    h.drain().await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE coupon_id = $1::uuid").bind(gid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 0);
    h.ok("POST", "/api/coupons", Some(json!({"name": "GEN"}))).await;
    h.cleanup().await;
}

#[tokio::test]
async fn campaigns_give_each_person_one_code_and_previews_use_none() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let cid = coupon(&mut h, "WELCOME", &["W-1", "W-2", "W-3", "W-4"]).await;
    let list = list_with(&mut h, &[("ann@x.test", "subscribed"), ("bob@x.test", "subscribed"), ("gone@x.test", "unsubscribed"), ("cat@x.test", "subscribed")]).await;
    let acct = account_id(&h).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'bounce' WHERE email = 'cat@x.test'").execute(&h.state.db).await.unwrap();

    // Previews and test sends show a sample and use no code.
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": coupon_email("WELCOME")}))).await;
    assert!(r["html"].as_str().unwrap().contains("SAMPLE-CODE"), "{r}");
    let c1 = campaign(&mut h, &list, coupon_email("WELCOME")).await;
    h.ok("POST", &format!("/api/campaigns/{c1}/test"), Some(json!({"to": ["qa@acme.test"]}))).await;
    assert!(h.state.email.sent().last().unwrap().subject.contains("SAMPLE-CODE"));
    let given: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE status <> 'available'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(given, 0, "previews and tests use no code");

    h.ok("POST", &format!("/api/campaigns/{c1}/send"), None).await;
    h.drain().await;
    let mails = h.state.email.sent();
    let ann = mails.iter().find(|m| m.to == "ann@x.test").unwrap();
    let bob = mails.iter().find(|m| m.to == "bob@x.test").unwrap();
    let (a, b) = (code_in(&ann.subject), code_in(&bob.subject));
    assert!(a.starts_with("W-") && b.starts_with("W-") && a != b, "{a} {b}");
    assert!(ann.html.contains(&a) && ann.text.contains(&a), "code in the body and the coupon block");
    assert!(!ann.html.contains("SAMPLE-CODE"));
    assert!(!mails.iter().any(|m| m.to == "gone@x.test" || m.to == "cat@x.test"));
    let c = h.ok("GET", &format!("/api/coupons/{cid}"), None).await;
    assert_eq!(c["stock"]["assigned"], 2, "unsubscribed and suppressed people don't use up codes: {c}");
    assert_eq!(c["stock"]["available"], 2);

    // A reminder shows the same codes, and its pre-send check passes with codes to spare for no one new.
    let c2 = campaign(&mut h, &list, coupon_email("WELCOME")).await;
    h.ok("POST", &format!("/api/campaigns/{c2}/send"), None).await;
    h.drain().await;
    let again: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "ann@x.test").collect();
    assert_eq!(code_in(&again.last().unwrap().subject), a);
    let p: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'ann@x.test'").fetch_one(&h.state.db).await.unwrap();
    let (status, owner, _) = status_of(&h, &a).await;
    assert_eq!((status.as_str(), owner), ("assigned", Some(p)));

    // An SMS body takes the tag too.
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": "Code: {% coupon_code 'WELCOME' %}"}}))).await;
    assert!(r["body"].as_str().unwrap().contains("Code: SAMPLE-CODE"), "{r}");

    // Unknown coupons are caught before sending.
    let c3 = campaign(&mut h, &list, coupon_email("NOPE")).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{c3}/send"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("no coupon named"), "{v}");
    let _ = acct;
    h.cleanup().await;
}

#[tokio::test]
async fn an_empty_pool_pauses_the_campaign_and_alerts_until_codes_arrive() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let cid = coupon(&mut h, "SPRING", &["S-1"]).await;
    let list = list_with(&mut h, &[("a@x.test", "subscribed"), ("b@x.test", "subscribed"), ("c@x.test", "subscribed")]).await;
    let camp = campaign(&mut h, &list, coupon_email("SPRING")).await;

    // The pre-send check sees the pool can't cover the audience.
    let (s, v) = h.req("POST", &format!("/api/campaigns/{camp}/send"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("has 1 codes left but this campaign may need 3"), "{v}");

    // It runs out mid-send anyway (here: codes taken by a flow since the check).
    h.ok("POST", &format!("/api/coupons/{cid}/codes"), Some(json!({"codes": ["S-2", "S-3"]}))).await;
    h.ok("POST", &format!("/api/campaigns/{camp}/send"), None).await;
    sqlx::query("UPDATE coupon_codes SET status = 'assigned', assign_key = 'other-' || code, assigned_via = 'form', delivered_at = now() WHERE code IN ('S-2', 'S-3')")
        .execute(&h.state.db)
        .await
        .unwrap();
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{camp}"), None).await;
    assert_eq!(c["status"], "sending", "{c}");
    assert!(c["error"].as_str().unwrap().contains("coupon \"SPRING\" has no codes left"), "{c}");
    let sent: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("Your code")).collect();
    assert_eq!(sent.len(), 1, "one went out with the one code; nobody got a blank code");
    assert!(!sent.iter().any(|m| code_in(&m.subject).is_empty()));
    let alerts: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.subject == "Coupon SPRING is out of codes").collect();
    assert_eq!(alerts.len(), 1, "one alert");
    assert_eq!(alerts[0].to, "owner@acme.test");
    let cp = h.ok("GET", &format!("/api/coupons/{cid}"), None).await;
    assert_eq!(cp["alerts"]["out_of_codes"], true);
    assert_eq!(cp["alerts"]["waiting"][0]["type"], "campaign");

    // Retrying while empty doesn't alert again or send.
    h.fast_forward().await;
    assert_eq!(h.state.email.sent().into_iter().filter(|m| m.subject == "Coupon SPRING is out of codes").count(), 1);

    // Codes arrive: the hold lifts and the rest go out.
    h.ok("POST", &format!("/api/coupons/{cid}/codes"), Some(json!({"codes": ["S-4", "S-5"]}))).await;
    h.fast_forward().await;
    let c = h.ok("GET", &format!("/api/campaigns/{camp}"), None).await;
    assert_eq!(c["status"], "sent", "{c}");
    assert!(c["error"].is_null());
    let codes: std::collections::HashSet<String> = h.state.email.sent().into_iter().filter(|m| m.subject.starts_with("Your code")).map(|m| code_in(&m.subject)).collect();
    assert_eq!(codes.len(), 3, "three people, three different codes: {codes:?}");
    assert_eq!(h.ok("GET", &format!("/api/coupons/{cid}"), None).await["alerts"]["out_of_codes"], false);
    h.cleanup().await;
}

#[tokio::test]
async fn flows_wait_for_codes_and_new_code_each_time() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let cid = coupon(&mut h, "FLOWY", &[]).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Joined Club"}, "definition": {"entry": "e1", "nodes": [
        {"id": "e1", "type": "email", "name": "Code", "content": coupon_email("FLOWY"), "next": null}]}}))).await;
    let fid = flow["id"].as_str().unwrap();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "dee@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined Club", "profile": {"email": "dee@x.test"}}))).await;
    h.drain().await;
    assert!(!h.state.email.sent().iter().any(|m| m.to == "dee@x.test"), "held, not sent blank");
    let run: String = sqlx::query_scalar("SELECT status FROM flow_runs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(run, "active", "the flow waits");
    assert_eq!(h.ok("GET", &format!("/api/coupons/{cid}"), None).await["alerts"]["waiting"][0]["type"], "flow");

    h.ok("POST", &format!("/api/coupons/{cid}/codes"), Some(json!({"codes": ["F-1", "F-2", "F-3"]}))).await;
    h.fast_forward().await;
    let got = h.state.email.sent().into_iter().find(|m| m.to == "dee@x.test").expect("sent once codes arrived");
    assert!(code_in(&got.subject).starts_with("F-"));

    // "New code each time": every message gets a different code; otherwise the same one.
    let acct = account_id(&h).await;
    let dee: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'dee@x.test'").fetch_one(&h.state.db).await.unwrap();
    let c = store::load(&h.state.db, acct, cid.parse().unwrap()).await.unwrap();
    let same = coupons::give_code(&h.state.db, &c, dee, Via::Message(Uuid::now_v7()), None).await.unwrap().unwrap();
    assert_eq!(same.code, code_in(&got.subject));
    h.ok("PATCH", &format!("/api/coupons/{cid}"), Some(json!({"new_code_each_time": true}))).await;
    let c = store::load(&h.state.db, acct, c.id).await.unwrap();
    let m = Uuid::now_v7();
    let a = coupons::give_code(&h.state.db, &c, dee, Via::Message(m), None).await.unwrap().unwrap();
    let replay = coupons::give_code(&h.state.db, &c, dee, Via::Message(m), None).await.unwrap().unwrap();
    let b = coupons::give_code(&h.state.db, &c, dee, Via::Message(Uuid::now_v7()), None).await.unwrap().unwrap();
    assert_eq!(a.code, replay.code, "the same message (a replayed job) keeps its code");
    assert_ne!(a.code, b.code);
    let _ = fid;
    h.cleanup().await;
}

#[tokio::test]
async fn concurrent_sends_never_share_a_code_or_give_one_person_two() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let codes: Vec<String> = (0..30).map(|i| format!("RACE-{i}")).collect();
    let id = coupon(&mut h, "RACE", &codes.iter().map(String::as_str).collect::<Vec<_>>()).await;
    let c = store::load(&h.state.db, acct, id.parse().unwrap()).await.unwrap();
    let mut people = Vec::new();
    for i in 0..40 {
        people.push(subscriber(&h, acct, &format!("p{i}@x.test"), "subscribed").await);
    }
    // 40 people at once for 30 codes.
    let mut tasks = Vec::new();
    for p in &people {
        let (db, c, p) = (h.state.db.clone(), c.clone(), *p);
        tasks.push(tokio::spawn(async move { coupons::give_code(&db, &c, p, Via::Message(Uuid::now_v7()), None).await.unwrap() }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    let given: Vec<String> = got.iter().flatten().map(|g| g.code.clone()).collect();
    assert_eq!(given.len(), 30);
    assert_eq!(given.iter().collect::<std::collections::HashSet<_>>().len(), 30, "no two people share a code");
    assert_eq!(got.iter().filter(|g| g.is_none()).count(), 10, "the rest find the pool empty");

    // One person, ten sends at once (and a replayed job): one code.
    let extra = codes.iter().map(|c| format!("{c}-B")).collect::<Vec<_>>();
    store::add_codes(&h.state.db, &c, extra).await.unwrap();
    let solo = subscriber(&h, acct, "solo@x.test", "subscribed").await;
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (db, c) = (h.state.db.clone(), c.clone());
        tasks.push(tokio::spawn(async move { coupons::give_code(&db, &c, solo, Via::Message(Uuid::now_v7()), None).await.unwrap().unwrap().code }));
    }
    let mut seen = std::collections::HashSet::new();
    for t in tasks {
        seen.insert(t.await.unwrap());
    }
    assert_eq!(seen.len(), 1, "{seen:?}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE profile_id = $1").bind(solo).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);

    // A store integration's hook fills in when the pool is empty, with no lock held.
    let empty = store::create(&h.state.db, acct, serde_json::from_value(json!({"name": "STORE"})).unwrap()).await.unwrap();
    let make = |_c: &store::Coupon, p: Uuid| -> coupons::StoreCodeFuture<'_> { Box::pin(async move { Ok(format!("SHOP-{}", &p.simple().to_string()[..8])) }) };
    let g = coupons::give_code(&h.state.db, &empty, solo, Via::Message(Uuid::now_v7()), Some(&make)).await.unwrap().unwrap();
    assert!(g.code.starts_with("SHOP-"));
    let again = coupons::give_code(&h.state.db, &empty, solo, Via::Message(Uuid::now_v7()), Some(&make)).await.unwrap().unwrap();
    assert_eq!(again.code, g.code, "the store isn't asked again for the same person");
    h.cleanup().await;
}

#[tokio::test]
async fn codes_for_messages_that_never_went_out_return_to_the_pool() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let id = coupon(&mut h, "BACK", &["B-1", "B-2", "B-3", "B-4", "B-5"]).await;
    let c = store::load(&h.state.db, acct, id.parse().unwrap()).await.unwrap();
    let mut cases = Vec::new();
    // (message row status, or none; should the code come back?)
    for (i, (status, back)) in [(None, true), (Some("skipped"), true), (Some("sent"), false), (Some("failed"), false), (Some("queued"), false)].into_iter().enumerate() {
        let p = subscriber(&h, acct, &format!("r{i}@x.test"), "subscribed").await;
        let mid = Uuid::now_v7();
        if let Some(s) = status {
            sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address, status) VALUES ($1, $2, $3, 'email', 'x@x.test', $4)")
                .bind(mid)
                .bind(acct)
                .bind(p)
                .bind(s)
                .execute(&h.state.db)
                .await
                .unwrap();
        }
        let g = coupons::give_code(&h.state.db, &c, p, Via::Message(mid), None).await.unwrap().unwrap();
        cases.push((g.code, back));
    }
    // Too soon: a send may still be on its way.
    assert_eq!(coupons::assign::release_unsent(&h.state.db).await.unwrap(), 0);
    sqlx::query("UPDATE coupon_codes SET claimed_at = now() - interval '20 minutes'").execute(&h.state.db).await.unwrap();
    assert_eq!(coupons::assign::release_unsent(&h.state.db).await.unwrap(), 2);
    for (code, back) in cases {
        let (status, owner, _) = status_of(&h, &code).await;
        assert_eq!(status == "available", back, "{code}: {status}");
        assert_eq!(owner.is_none(), back);
    }
    // A delivered code is never taken back, however old.
    let p = subscriber(&h, acct, "kept@x.test", "subscribed").await;
    let g = coupons::give_code(&h.state.db, &c, p, Via::Message(Uuid::now_v7()), None).await.unwrap().unwrap();
    sqlx::query("UPDATE coupon_codes SET delivered_at = now(), claimed_at = now() - interval '1 day' WHERE id = $1").bind(g.id).execute(&h.state.db).await.unwrap();
    assert_eq!(coupons::assign::release_unsent(&h.state.db).await.unwrap(), 0);

    // An expired coupon refuses to hand out codes rather than sending dead ones.
    h.ok("PATCH", &format!("/api/coupons/{id}"), Some(json!({"expires_at": "2020-01-01T00:00:00Z"}))).await;
    let c = store::load(&h.state.db, acct, c.id).await.unwrap();
    assert!(coupons::give_code(&h.state.db, &c, p, Via::Message(Uuid::now_v7()), None).await.is_err());
    h.cleanup().await;
}

#[tokio::test]
async fn sign_up_forms_hand_out_one_code_only_to_new_sign_ups_within_limits() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let codes: Vec<String> = (0..20).map(|i| format!("FORM-{i}")).collect();
    let cid = coupon(&mut h, "POPUP", &codes.iter().map(String::as_str).collect::<Vec<_>>()).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Club"}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list["id"], "config": {"discount_code": "SHARED10", "coupon": "POPUP"}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    // The public config never carries a code.
    let (_, _, forms) = h.raw("GET", &format!("/client/forms?k={pk}"), &[], "").await;
    assert!(!forms.contains("FORM-") && !forms.contains("SHARED10"));
    let sign_up = |email: &str, ip: &str| {
        let body = json!({"form_id": fid, "email": email}).to_string();
        let ip = ip.to_string();
        let path = format!("/client/subscribe?k={pk}");
        (path, body, ip)
    };
    let send = |h: &Harness, (path, body, ip): (String, String, String)| {
        let router = h.router.clone();
        async move {
            let req = axum::http::Request::builder().method("POST").uri(path).header("content-type", "application/json").header("x-real-ip", ip).body(axum::body::Body::from(body)).unwrap();
            let res = tower::ServiceExt::oneshot(router, req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let b = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&b).unwrap()
        }
    };
    let v = send(&h, sign_up("new@x.test", "203.0.113.1")).await;
    let code = v["discount_code"].as_str().unwrap().to_string();
    assert!(code.starts_with("FORM-"), "{v}");
    // Signing up again: no second code, and the first isn't shown again.
    let v = send(&h, sign_up("new@x.test", "203.0.113.2")).await;
    assert!(v["discount_code"].is_null(), "{v}");
    // Someone already in the audience: anyone could type their address, so no code on screen.
    h.ok("POST", "/api/profiles", Some(json!({"email": "known@x.test"}))).await;
    let v = send(&h, sign_up("known@x.test", "203.0.113.3")).await;
    assert!(v["discount_code"].is_null(), "neither a unique nor the shared code: {v}");

    // One visitor making up addresses gets at most 3 codes an hour.
    let mut from_bot = 0;
    for i in 0..6 {
        if send(&h, sign_up(&format!("bot{i}@x.test"), "198.51.100.7")).await["discount_code"].is_string() {
            from_bot += 1;
        }
    }
    assert_eq!(from_bot, 3);
    // And the coupon's hourly form limit holds across visitors.
    h.ok("PATCH", &format!("/api/coupons/{cid}"), Some(json!({"form_codes_per_hour": 6}))).await;
    let mut more = 0;
    for i in 0..5 {
        if send(&h, sign_up(&format!("crowd{i}@x.test"), &format!("192.0.2.{i}"))).await["discount_code"].is_string() {
            more += 1;
        }
    }
    assert_eq!(more, 2, "1 + 3 given already, 6 an hour");
    let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE assigned_via = 'form' AND delivered_at IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(delivered, 6);
    let _ = code;
    h.cleanup().await;
}

#[tokio::test]
async fn double_opt_in_forms_give_the_code_after_confirming() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    coupon(&mut h, "DOI", &["D-1", "D-2"]).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "double_opt_in": true}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list["id"], "config": {"coupon": "DOI"}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    let (_, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": fid, "email": "doi@x.test"}).to_string()).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["confirm"], true);
    assert!(v["discount_code"].is_null(), "no code before confirming: {v}");
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "doi@x.test").unwrap();
    let path = find_link(&mail.html, "https://pl.test/c/").trim_start_matches("https://pl.test").to_string();
    let (_, _, page) = h.raw("GET", &path, &[], "").await;
    assert!(!page.contains("D-"), "opening the link gives nothing");
    let (_, _, page) = h.raw("POST", &path, &[], "").await;
    assert!(page.contains("Your code: <strong>D-1</strong>"), "{page}");
    let (_, _, page) = h.raw("POST", &path, &[], "").await;
    assert!(page.contains("D-1"), "the same code on a second visit");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE status <> 'available'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn orders_redeem_only_the_buyers_own_code_and_browser_orders_stay_unverified() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let acct = account_id(&h).await;
    let id = coupon(&mut h, "ORDERS", &["O-1", "O-2", "O-3"]).await;
    let c = store::load(&h.state.db, acct, id.parse().unwrap()).await.unwrap();
    let ann = subscriber(&h, acct, "ann@x.test", "subscribed").await;
    let bob = subscriber(&h, acct, "bob@x.test", "subscribed").await;
    let deliver = |db: sqlx::PgPool, c: store::Coupon, p: Uuid| async move {
        let g = coupons::give_code(&db, &c, p, Via::Message(Uuid::now_v7()), None).await.unwrap().unwrap();
        sqlx::query("UPDATE coupon_codes SET delivered_at = now() WHERE id = $1").bind(g.id).execute(&db).await.unwrap();
        g.code
    };
    let ann_code = deliver(h.state.db.clone(), c.clone(), ann).await;
    let bob_code = deliver(h.state.db.clone(), c.clone(), bob).await;

    // A forged browser order by "ann" using bob's code doesn't touch bob's code.
    let (s, _, _) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 99999, "properties": {"discount_codes": [bob_code.to_lowercase()]}}).to_string()).await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    assert_eq!(status_of(&h, &bob_code).await.0, "assigned");

    // A browser order with ann's own code: redeemed, but unverified and worth nothing in reports.
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")],
        &json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 5000, "properties": {"discount_codes": [ann_code.clone()]}}).to_string()).await;
    h.drain().await;
    assert_eq!(status_of(&h, &ann_code).await, ("redeemed".to_string(), Some(ann), Some(false)));
    let r = h.ok("GET", &format!("/api/coupons/{id}/report"), None).await;
    assert_eq!((r["redeemed"].as_i64(), r["redeemed_unverified"].as_i64(), r["revenue"].as_f64()), (Some(0), Some(1), Some(0.0)), "{r}");

    // The store's own order (through the API) verifies it and counts its revenue; replays change nothing.
    for _ in 0..2 {
        h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 80.0, "unique_id": "shop-1001",
            "properties": {"discount_codes": [{"code": ann_code.to_lowercase(), "amount": 8}]}}))).await;
        h.drain().await;
    }
    assert_eq!(status_of(&h, &ann_code).await.2, Some(true));
    let r = h.ok("GET", &format!("/api/coupons/{id}/report"), None).await;
    assert_eq!((r["given"].as_i64(), r["redeemed"].as_i64(), r["redeemed_unverified"].as_i64(), r["revenue"].as_f64()), (Some(2), Some(1), Some(0), Some(80.0)), "{r}");
    assert_eq!(r["redemption_rate"], 0.5);

    // Segments: given / redeemed.
    let seg = |action: &str, op: &str| json!({"definition": {"match": "all", "conditions": [{"type": "coupon", "coupon_id": id, "action": action, "op": op}]}});
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg("given", "has"))).await["count"], 2);
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg("redeemed", "has"))).await["count"], 1);
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg("redeemed", "has_not"))).await["count"], 1);
    h.cleanup().await;
}

#[tokio::test]
async fn mcp_tools_cover_coupons_and_delete_needs_confirm() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let ctx = dumbmail::auth::Ctx { account_id: acct, user_id: None, via_api_key: true };
    let names: Vec<String> = dumbmail::features::mcp_tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for n in ["list_coupons", "get_coupon", "create_coupon", "update_coupon", "add_coupon_codes", "generate_coupon_codes", "delete_coupon", "coupon_report"] {
        assert!(names.iter().any(|x| x == n), "{n}");
    }
    let call = |name: &'static str, args: Value| {
        let (state, ctx) = (h.state.clone(), ctx.clone());
        async move { dumbmail::features::mcp_call(&state, &ctx, name, &args).await.unwrap() }
    };
    call("create_coupon", json!({"name": "MCP", "expires_after_days": 14})).await.unwrap();
    let r = call("add_coupon_codes", json!({"id": "mcp", "text": "M-1\nM-2"})).await.unwrap();
    assert_eq!(r["added"], 2);
    let r = call("generate_coupon_codes", json!({"id": "MCP", "count": 3, "pattern": {"prefix": "G", "length": 6}})).await.unwrap();
    assert_eq!(r["added"], 3);
    let g = call("get_coupon", json!({"id": "MCP", "search": "M-"})).await.unwrap();
    assert_eq!(g["codes"]["total"], 2);
    assert_eq!(g["stock"]["total"], 5);
    let dry = call("delete_coupon", json!({"id": "MCP"})).await.unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(call("list_coupons", json!({})).await.unwrap()["total"], 1, "nothing deleted without confirm");
    assert_eq!(call("coupon_report", json!({})).await.unwrap()["data"][0]["name"], "MCP");
    call("delete_coupon", json!({"id": "MCP", "confirm": true})).await.unwrap();
    assert_eq!(call("list_coupons", json!({})).await.unwrap()["total"], 0);
    h.cleanup().await;
}

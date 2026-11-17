//! Unique coupon codes: pools, giving codes out in messages and forms exactly once and for good,
//! holds when a pool runs out, redemptions matched from orders, segments, and the attacks and
//! crashes each rule is there for.

#[macro_use]
mod common;

use axum::http::StatusCode;
use dumbmail::features::coupons::{self, store, Via};
use dumbmail::models::Profile;
use dumbmail::sending::SendSpec;
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
        let p = match sqlx::query_scalar::<_, Uuid>("SELECT id FROM profiles WHERE email = $1").bind(e).fetch_optional(&h.state.db).await.unwrap() {
            Some(p) => p,
            None => subscriber(h, acct, e, consent).await,
        };
        sqlx::query("INSERT INTO list_members (list_id, profile_id) VALUES ($1, $2)").bind(lid).bind(p).execute(&h.state.db).await.unwrap();
    }
    lid.to_string()
}

/// An email that shows the code twice (CODE[...] in a text block and in a coupon block).
fn coupon_email(name: &str) -> Value {
    json!({"subject": "A code for you", "blocks": [
        {"type": "text", "html": format!("<p>Hi! CODE[{{% coupon_code '{name}' %}}]</p>")},
        {"type": "coupon", "coupon": name, "title": "Your code", "button_text": "Shop", "href": "https://acme.test"}
    ]})
}

async fn campaign(h: &mut Harness, list: &str, content: Value) -> String {
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sale", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    c["id"].as_str().unwrap().to_string()
}

/// The first CODE[...] in an email.
fn code_in(html: &str) -> String {
    let i = html.find("CODE[").map(|i| i + 5).unwrap_or(0);
    html[i..].split(']').next().unwrap_or("").to_string()
}

fn mails_to(h: &Harness, to: &str) -> Vec<dumbmail::providers::email::OutgoingEmail> {
    h.state.email.sent().into_iter().filter(|m| m.to == to).collect()
}

async fn holder(h: &Harness, code: &str) -> (String, Option<Uuid>) {
    sqlx::query_as("SELECT status, profile_id FROM coupon_codes WHERE code = $1").bind(code).fetch_one(&h.state.db).await.unwrap()
}

async fn profile(h: &Harness, id: Uuid) -> Profile {
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1", dumbmail::models::PROFILE_COLS);
    sqlx::query_as::<_, Profile>(&q).bind(id).fetch_one(&h.state.db).await.unwrap()
}

fn msg(id: Uuid) -> Via<'static> {
    Via::Message { message_id: id, send: "test" }
}

#[tokio::test]
async fn coupons_crud_uploads_generation_export_and_isolation() {
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

    // Pasted text: header skipped, duplicates in any case skipped, non-ASCII and short codes refused by line.
    let r = h.ok("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"text": "code\nSAVE-1\nsave-1\nSAVE-2\nhas space\nstraße\n-DASHED\n"}))).await;
    assert_eq!((r["added"].as_i64(), r["duplicates"].as_i64(), r["refused"].as_i64()), (Some(3), Some(1), Some(2)), "{r}");
    assert_eq!(r["refused_lines"][0]["line"], 5);
    assert_eq!(r["refused_lines"][1]["line"], 6);
    // A CSV file sent as itself is read as it arrives.
    let csv = "Code,Amount\nSAVE-2,10\nSAVE-4,10\n\"SAVE-5\",10\n";
    let (s, _, body) = h.bytes("POST", &format!("/api/coupons/{id}/codes"), "text/csv", csv.as_bytes().to_vec()).await;
    assert_eq!(s, StatusCode::OK);
    let r: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!((r["added"].as_i64(), r["duplicates"].as_i64()), (Some(2), Some(1)), "{r}");
    assert_eq!(r["coupon"]["stock"]["available"], 5);
    // Past the per-upload limit it stops and says so; what came before stays added.
    let big = coupon(&mut h, "BIG", &[]).await;
    let too_many: Vec<String> = (0..200_001).map(|i| format!("C{i:06}")).collect();
    let (s, _, body) = h.bytes("POST", &format!("/api/coupons/{big}/codes"), "text/plain", too_many.join("\n").into_bytes()).await;
    assert_eq!(s, StatusCode::OK);
    let r: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(r["added"], 200_000);
    assert!(r["stopped"].as_str().unwrap().contains("Stopped at line 200001"), "{r}");
    // One upload at a time per account.
    let acct = account_id(&h).await;
    let lease = store::Lease::take(&h.state.db, acct).await.unwrap().unwrap();
    let (s, _) = h.req("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"codes": ["SAVE-9"]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    lease.release(&h.state.db).await;

    // Export: the plain form round-trips exactly (a leading "-" kept); the CSV is spreadsheet-safe.
    let (s, _, plain) = h.bytes("GET", &format!("/api/coupons/{id}/export?format=codes"), "text/plain", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    let plain = String::from_utf8(plain).unwrap();
    assert_eq!(plain.lines().collect::<Vec<_>>(), vec!["SAVE-1", "SAVE-2", "-DASHED", "SAVE-4", "SAVE-5"]);
    let (_, hd, csv) = h.bytes("GET", &format!("/api/coupons/{id}/export"), "text/plain", vec![]).await;
    assert!(hd.get("content-type").unwrap().to_str().unwrap().starts_with("text/csv"));
    assert!(String::from_utf8(csv).unwrap().contains("\n'-DASHED,available"));

    // Generated codes: at least 8 characters, no look-alikes. Small batches now, bigger ones in a job.
    let g = h.ok("POST", "/api/coupons", Some(json!({"name": "GEN", "source": "generated", "pattern": {"prefix": "VIP-", "length": 8}}))).await;
    let gid = g["id"].as_str().unwrap().to_string();
    let r = h.ok("POST", &format!("/api/coupons/{gid}/generate"), Some(json!({"count": 500}))).await;
    assert_eq!(r["added"], 500);
    let r = h.ok("POST", &format!("/api/coupons/{gid}/generate"), Some(json!({"count": 6000}))).await;
    assert_eq!((r["added"].as_i64(), r["queued"].as_i64()), (Some(0), Some(6000)), "{r}");
    assert_eq!(r["coupon"]["stock"]["generating"], 6000);
    h.drain().await;
    let c = h.ok("GET", &format!("/api/coupons/{gid}"), None).await;
    assert_eq!((c["stock"]["total"].as_i64(), c["stock"]["generating"].as_i64()), (Some(6500), Some(0)), "{c}");
    let (s, _) = h.req("POST", &format!("/api/coupons/{gid}/generate"), Some(json!({"count": 10, "pattern": {"length": 4, "alphabet": "digits"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "four-digit codes are too easy to guess");

    // Paging follows a cursor through every code (no cap).
    let mut seen = std::collections::HashSet::new();
    let mut cursor: Option<i64> = None;
    loop {
        let path = match cursor {
            Some(c) => format!("/api/coupons/{gid}/codes?limit=200&cursor={c}"),
            None => format!("/api/coupons/{gid}/codes?limit=200"),
        };
        let page = h.ok("GET", &path, None).await;
        for c in page["data"].as_array().unwrap() {
            let code = c["code"].as_str().unwrap();
            assert!(code.starts_with("VIP-") && code.len() == 12 && !code[4..].contains(['0', 'O', '1', 'I', 'L']), "{code}");
            seen.insert(code.to_string());
        }
        match page["next_cursor"].as_i64() {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen.len(), 6500);
    assert_eq!(h.ok("GET", &format!("/api/coupons/{gid}/codes?q=vip-&limit=1"), None).await["total"], 6500);

    let list = h.ok("GET", "/api/coupons", None).await;
    assert_eq!(list["total"], 3);
    assert_eq!(list["data"][2]["usage"]["tag"], "{% coupon_code 'WELCOME' %}");

    // Another account sees none of it and can't touch it.
    let mine = h.cookie.take();
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    assert_eq!(h.ok("GET", "/api/coupons", None).await["total"], 0);
    for (m, p, b) in [
        ("GET", format!("/api/coupons/{id}"), None),
        ("GET", format!("/api/coupons/{id}/codes"), None),
        ("POST", format!("/api/coupons/{id}/codes"), Some(json!({"codes": ["XYZ-1"]}))),
        ("DELETE", format!("/api/coupons/{id}?confirm=true"), None),
    ] {
        let (s, _) = h.req(m, &p, b).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {p}");
    }
    h.cookie = mine;

    // Deleting needs confirm=true; then it's gone at once (codes in background batches) and the name is free.
    let (s, v) = h.req("DELETE", &format!("/api/coupons/{gid}"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("confirm=true"), "{v}");
    assert_eq!(h.ok("GET", "/api/coupons", None).await["total"], 3);
    let (s, _) = h.req("DELETE", &format!("/api/coupons/{gid}?confirm=true"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(h.ok("GET", "/api/coupons", None).await["total"], 2);
    h.drain().await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE coupon_id = $1::uuid").bind(&gid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 0);
    h.ok("POST", "/api/coupons", Some(json!({"name": "GEN"}))).await;
    h.cleanup().await;
}

#[tokio::test]
async fn campaigns_give_each_person_one_code_and_previews_use_none() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let cid = coupon(&mut h, "WELCOME", &["WEL-1", "WEL-2", "WEL-3", "WEL-4"]).await;
    let list = list_with(&mut h, &[("ann@x.test", "subscribed"), ("bob@x.test", "subscribed"), ("gone@x.test", "unsubscribed"), ("cat@x.test", "subscribed")]).await;
    sqlx::query("UPDATE profiles SET email_suppressed_reason = 'bounce' WHERE email = 'cat@x.test'").execute(&h.state.db).await.unwrap();

    // Previews and test sends show a sample and use no code.
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": coupon_email("WELCOME")}))).await;
    assert!(r["html"].as_str().unwrap().contains("CODE[SAMPLE-CODE]"), "{r}");
    let c1 = campaign(&mut h, &list, coupon_email("WELCOME")).await;
    h.ok("POST", &format!("/api/campaigns/{c1}/test"), Some(json!({"to": ["qa@acme.test"]}))).await;
    assert_eq!(code_in(&h.state.email.sent().last().unwrap().html), "SAMPLE-CODE");
    let given: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE status <> 'available'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(given, 0, "previews and tests use no code");

    h.ok("POST", &format!("/api/campaigns/{c1}/send"), None).await;
    h.drain().await;
    let (ann, bob) = (mails_to(&h, "ann@x.test").pop().unwrap(), mails_to(&h, "bob@x.test").pop().unwrap());
    let (a, b) = (code_in(&ann.html), code_in(&bob.html));
    assert!(a.starts_with("WEL-") && b.starts_with("WEL-") && a != b, "{a} {b}");
    assert!(ann.text.contains(&a), "the code is in the text part too");
    assert!(!ann.html.contains("SAMPLE-CODE") && !ann.subject.contains(&a));
    assert!(mails_to(&h, "gone@x.test").is_empty() && mails_to(&h, "cat@x.test").is_empty());
    let c = h.ok("GET", &format!("/api/coupons/{cid}"), None).await;
    assert_eq!(c["stock"]["assigned"], 2, "unsubscribed and suppressed people don't use up codes: {c}");
    // The send recorded when each code first reached its person.
    let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE delivered_at IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(delivered, 2);

    // A reminder shows the same codes.
    let c2 = campaign(&mut h, &list, coupon_email("WELCOME")).await;
    h.ok("POST", &format!("/api/campaigns/{c2}/send"), None).await;
    h.drain().await;
    assert_eq!(code_in(&mails_to(&h, "ann@x.test").pop().unwrap().html), a);
    let p: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'ann@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(holder(&h, &a).await, ("assigned".to_string(), Some(p)));

    // An SMS body takes the tag too (preview).
    let r = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": "Code: {% coupon_code 'WELCOME' %}"}}))).await;
    assert!(r["body"].as_str().unwrap().contains("Code: SAMPLE-CODE"), "{r}");

    // Saving content that misuses coupons is refused: in the subject, a name that isn't a literal, the old function form.
    for (content, needle) in [
        (json!({"subject": "Code {% coupon_code 'WELCOME' %}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}), "subject or preheader"),
        (json!({"subject": "x", "preheader": "{% coupon_code 'WELCOME' %}", "blocks": [{"type": "text", "html": "<p>x</p>"}]}), "subject or preheader"),
        (json!({"subject": "x", "blocks": [{"type": "text", "html": "{% coupon_code first_name %}"}]}), "in quotes"),
        (json!({"subject": "x", "blocks": [{"type": "text", "html": "{{ coupon_code('WELCOME') }}"}]}), "{% coupon_code 'NAME' %}"),
    ] {
        let (s, v) = h.req("POST", "/api/campaigns", Some(json!({"name": "Bad", "channel": "email", "content": content}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(needle), "{v}");
    }
    let (s, _) = h.req("POST", "/api/templates", Some(json!({"name": "T", "content": {"subject": "{% coupon_code 'WELCOME' %}", "blocks": []}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "templates too");
    let (s, _) = h.req("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "metric", "metric": "X"}, "definition": {"entry": "e1", "nodes": [
        {"id": "e1", "type": "email", "content": {"subject": "{% coupon_code 'WELCOME' %}", "blocks": []}, "next": null}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "flow steps too");

    // Unknown coupons are caught before sending.
    let c3 = campaign(&mut h, &list, coupon_email("NOPE")).await;
    let (s, v) = h.req("POST", &format!("/api/campaigns/{c3}/send"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("no coupon named"), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_persons_code_is_never_given_to_anyone_else() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    coupon(&mut h, "KEEP", &["KEEP-1", "KEEP-2", "KEEP-3"]).await;
    let ann_list = list_with(&mut h, &[("ann@x.test", "subscribed")]).await;
    let bob_list = list_with(&mut h, &[("bob@x.test", "subscribed")]).await;
    let ann: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'ann@x.test'").fetch_one(&h.state.db).await.unwrap();

    // A first send claims ann's code and is still on its way (not delivered).
    let c = store::by_name(&h.state.db, acct, "KEEP").await.unwrap().unwrap();
    let first = coupons::give_code(&h.state.db, &c, ann, msg(Uuid::now_v7()), None).await.unwrap().unwrap();
    // A second send to ann claims (her same code) and is then skipped by a later check.
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let person = profile(&h, ann).await;
    let content = coupon_email("KEEP");
    let second = Uuid::now_v7();
    let spec = SendSpec { account: &account, profile: &person, channel: "email", content: &content, campaign_id: None, flow_id: None, flow_node_id: None, event: None, is_test: false, message_id: None, to_override: None };
    let coupons::Prepared::Ready(_) = coupons::assign::prepare_send(&h.state, &spec, second).await.unwrap() else { panic!("held") };
    sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address, status, error) VALUES ($1, $2, $3, 'email', 'ann@x.test', 'skipped', 'sending paused')")
        .bind(second)
        .bind(acct)
        .bind(ann)
        .execute(&h.state.db)
        .await
        .unwrap();
    let mine: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE profile_id = $1").bind(ann).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(mine, 1, "the same code, not a second one");
    // Time passes and the scheduler runs: nothing hands ann's code back.
    sqlx::query("UPDATE coupon_codes SET assigned_at = now() - interval '2 days'").execute(&h.state.db).await.unwrap();
    for t in 0..6 {
        dumbmail::jobs::scheduler_tick(&h.state, t).await.unwrap();
    }
    assert_eq!(holder(&h, &first.code).await, ("assigned".to_string(), Some(ann)));
    // Bob gets a different code; ann's later send gets hers.
    h.credit(5.0).await;
    let c3 = campaign(&mut h, &bob_list, coupon_email("KEEP")).await;
    h.ok("POST", &format!("/api/campaigns/{c3}/send"), None).await;
    let c4 = campaign(&mut h, &ann_list, coupon_email("KEEP")).await;
    h.ok("POST", &format!("/api/campaigns/{c4}/send"), None).await;
    h.drain().await;
    let bob_code = code_in(&mails_to(&h, "bob@x.test").pop().unwrap().html);
    assert_ne!(bob_code, first.code);
    assert_eq!(code_in(&mails_to(&h, "ann@x.test").pop().unwrap().html), first.code);
    h.cleanup().await;
}

#[tokio::test]
async fn an_empty_pool_holds_the_campaign_alerts_once_per_spell_and_claims_all_or_nothing() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let cid = coupon(&mut h, "SPRING", &["SPR-1"]).await;
    let list = list_with(&mut h, &[("a@x.test", "subscribed"), ("b@x.test", "subscribed"), ("c@x.test", "subscribed")]).await;
    let camp = campaign(&mut h, &list, coupon_email("SPRING")).await;

    // The pre-send check warns with the numbers but doesn't block (stock and audience can change).
    let check = h.ok("GET", &format!("/api/coupons/campaign-check/{camp}"), None).await;
    assert!(check["problems"].as_array().unwrap().is_empty(), "{check}");
    assert!(check["warnings"][0].as_str().unwrap().contains("has 1 codes left and this campaign may need up to 3"), "{check}");
    h.ok("POST", &format!("/api/campaigns/{camp}/send"), None).await;
    h.drain().await;
    let c = h.ok("GET", &format!("/api/campaigns/{camp}"), None).await;
    assert_eq!(c["status"], "sending", "{c}");
    assert!(c["error"].as_str().unwrap().contains("Coupon \"SPRING\" has no codes left"), "{c}");
    let sent: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.subject == "A code for you").collect();
    assert_eq!(sent.len(), 1, "one went out with the one code");
    assert!(!sent.iter().any(|m| m.html.contains("SAMPLE-CODE") || code_in(&m.html).is_empty()), "nobody got a sample or blank code");
    let email = h.state.email.clone();
    let alerts = move || email.sent().into_iter().filter(|m| m.subject == "Coupon SPRING is out of codes").collect::<Vec<_>>();
    assert_eq!(alerts().len(), 1, "one alert");
    assert_eq!(alerts()[0].to, "owner@acme.test");
    let cp = h.ok("GET", &format!("/api/coupons/{cid}"), None).await;
    assert_eq!(cp["alerts"]["out_of_codes"], true);
    assert_eq!(cp["alerts"]["waiting"][0]["type"], "campaign");

    // Retrying while empty doesn't alert again or send, even when a held person's existing code is reused elsewhere.
    h.fast_forward().await;
    assert_eq!(alerts().len(), 1);

    // Codes arrive: the hold lifts and the rest go out.
    h.ok("POST", &format!("/api/coupons/{cid}/codes"), Some(json!({"codes": ["SPR-4", "SPR-5"]}))).await;
    h.fast_forward().await;
    let c = h.ok("GET", &format!("/api/campaigns/{camp}"), None).await;
    assert_eq!(c["status"], "sent", "{c}");
    assert!(c["error"].is_null());
    let codes: std::collections::HashSet<String> = h.state.email.sent().into_iter().filter(|m| m.subject == "A code for you").map(|m| code_in(&m.html)).collect();
    assert_eq!(codes.len(), 3, "three people, three different codes: {codes:?}");
    assert_eq!(h.ok("GET", &format!("/api/coupons/{cid}"), None).await["alerts"]["out_of_codes"], false);

    // A new empty spell alerts again.
    let list2 = list_with(&mut h, &[("d@x.test", "subscribed")]).await;
    let camp2 = campaign(&mut h, &list2, coupon_email("SPRING")).await;
    h.ok("POST", &format!("/api/campaigns/{camp2}/send"), None).await;
    h.drain().await;
    assert_eq!(alerts().len(), 2);

    // A message needing two coupons, one of them empty: nothing is claimed from the other.
    let full = coupon(&mut h, "FULL", &["FUL-1", "FUL-2"]).await;
    let both = json!({"subject": "Two codes", "blocks": [{"type": "text", "html": "<p>CODE[{% coupon_code 'FULL' %}] and {% coupon_code 'SPRING' %}</p>"}]});
    let list3 = list_with(&mut h, &[("e@x.test", "subscribed")]).await;
    let camp3 = campaign(&mut h, &list3, both).await;
    h.ok("POST", &format!("/api/campaigns/{camp3}/send"), None).await;
    h.drain().await;
    assert!(mails_to(&h, "e@x.test").is_empty());
    assert_eq!(h.ok("GET", &format!("/api/coupons/{full}"), None).await["stock"]["assigned"], 0, "all or nothing");
    h.cleanup().await;
}

#[tokio::test]
async fn ab_tests_are_checked_per_version_and_real_sends_never_show_a_sample() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    coupon(&mut h, "HALF-A", &["HA-01", "HA-02"]).await;
    coupon(&mut h, "HALF-B", &["HB-01", "HB-02"]).await;
    let list = list_with(&mut h, &[("p1@x.test", "subscribed"), ("p2@x.test", "subscribed"), ("p3@x.test", "subscribed"), ("p4@x.test", "subscribed")]).await;
    // A 50/50 test of everyone, each version with its own pool sized for its half.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "AB", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": coupon_email("HALF-A"),
        "ab_test": {"variants": [{"id": "b", "content": coupon_email("HALF-B")}], "test_percent": 100, "wait_hours": 1, "metric": "click_rate"}}))).await;
    let id = c["id"].as_str().unwrap();
    let check = h.ok("GET", &format!("/api/coupons/campaign-check/{id}"), None).await;
    assert_eq!(check, json!({"problems": [], "warnings": []}));
    h.ok("POST", &format!("/api/campaigns/{id}/send"), None).await;
    h.drain().await;
    let codes: Vec<String> = h.state.email.sent().into_iter().filter(|m| m.subject == "A code for you").map(|m| code_in(&m.html)).collect();
    assert_eq!(codes.len(), 4);
    assert_eq!(codes.iter().filter(|c| c.starts_with("HA-")).count(), 2);
    assert_eq!(codes.iter().filter(|c| c.starts_with("HB-")).count(), 2);

    // Content that slipped past the save check (written straight to the database) fails the send instead
    // of rendering a sample or a blank.
    let list2 = list_with(&mut h, &[("q1@x.test", "subscribed")]).await;
    let bad = campaign(&mut h, &list2, json!({"subject": "Hi", "blocks": [{"type": "text", "html": "<p>x</p>"}]})).await;
    let forced = json!({"subject": "Hi", "blocks": [{"type": "text", "html": "<p>CODE[{{ coupon_code('HALF-A') }}]</p>"}]});
    sqlx::query("UPDATE campaigns SET content = $2 WHERE id = $1::uuid").bind(bad.as_str()).bind(&forced).execute(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/campaigns/{bad}/send"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the launch check catches it: {v}");
    // And if it changes after launch, the send itself fails rather than showing a sample.
    sqlx::query("UPDATE campaigns SET content = $2 WHERE id = $1::uuid").bind(bad.as_str()).bind(json!({"subject": "Hi", "blocks": [{"type": "text", "html": "<p>x</p>"}]})).execute(&h.state.db).await.unwrap();
    h.ok("POST", &format!("/api/campaigns/{bad}/send"), None).await;
    sqlx::query("UPDATE campaigns SET content = $2 WHERE id = $1::uuid").bind(bad.as_str()).bind(&forced).execute(&h.state.db).await.unwrap();
    h.drain().await;
    assert!(mails_to(&h, "q1@x.test").is_empty());
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{bad}"), None).await["status"], "failed");

    // A flow whose coupon has no codes waits; it never sends a sample.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "W", "trigger": {"type": "metric", "metric": "Joined"}, "definition": {"entry": "e1", "nodes": [
        {"id": "e1", "type": "email", "name": "Code", "content": coupon_email("HALF-A"), "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined", "profile": {"email": "q2@x.test", "email_consent": "subscribed"}}))).await;
    h.drain().await;
    assert!(mails_to(&h, "q2@x.test").is_empty(), "held, not sent with a sample");
    assert!(!h.state.email.sent().iter().any(|m| !m.subject.starts_with("[Test]") && m.html.contains("SAMPLE-CODE")));
    h.cleanup().await;
}

#[tokio::test]
async fn flows_wait_for_codes_and_a_retried_step_keeps_its_new_code() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let cid = coupon(&mut h, "FLOWY", &[]).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Joined Club"}, "definition": {"entry": "e1", "nodes": [
        {"id": "e1", "type": "email", "name": "Code", "content": coupon_email("FLOWY"), "next": null}]}}))).await;
    let fid: Uuid = flow["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "dee@x.test", "email_consent": "subscribed"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Joined Club", "profile": {"email": "dee@x.test"}}))).await;
    h.drain().await;
    assert!(mails_to(&h, "dee@x.test").is_empty(), "held, not sent blank");
    let run: String = sqlx::query_scalar("SELECT status FROM flow_runs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(run, "active", "the flow waits");
    assert_eq!(h.ok("GET", &format!("/api/coupons/{cid}"), None).await["alerts"]["waiting"][0]["type"], "flow");

    h.ok("POST", &format!("/api/coupons/{cid}/codes"), Some(json!({"codes": ["FLO-1", "FLO-2", "FLO-3"]}))).await;
    h.fast_forward().await;
    let got = mails_to(&h, "dee@x.test").pop().expect("sent once codes arrived");
    assert!(code_in(&got.html).starts_with("FLO-"));

    // "New code each time": a flow step is retried with a new message id each attempt (warm-up, a busy
    // provider). The retries of one run's step share one code; another run gets another.
    h.ok("PATCH", &format!("/api/coupons/{cid}"), Some(json!({"new_code_each_time": true}))).await;
    let acct = account_id(&h).await;
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let dee: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'dee@x.test'").fetch_one(&h.state.db).await.unwrap();
    let person = profile(&h, dee).await;
    let content = coupon_email("FLOWY");
    let spec = || SendSpec {
        account: &account,
        profile: &person,
        channel: "email",
        content: &content,
        campaign_id: None,
        flow_id: Some(fid),
        flow_node_id: Some("e1"),
        event: None,
        is_test: false,
        message_id: None,
        to_override: None,
    };
    let run_a = Uuid::now_v7();
    sqlx::query("INSERT INTO flow_runs (id, flow_id, account_id, profile_id, current_node, context) VALUES ($1, $2, $3, $4, 'e1', '{}')")
        .bind(run_a)
        .bind(fid)
        .bind(acct)
        .bind(dee)
        .execute(&h.state.db)
        .await
        .unwrap();
    let mut codes = Vec::new();
    for _ in 0..3 {
        let coupons::Prepared::Ready(c) = coupons::assign::prepare_send(&h.state, &spec(), Uuid::now_v7()).await.unwrap() else { panic!("held") };
        let mut ctx = json!({});
        c.fill(&mut ctx);
        codes.push(ctx["__dm_coupons"]["codes"]["flowy"]["code"].as_str().unwrap().to_string());
    }
    assert!(codes.windows(2).all(|w| w[0] == w[1]), "one code for the retried step: {codes:?}");
    sqlx::query("UPDATE flow_runs SET status = 'completed' WHERE id = $1").bind(run_a).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO flow_runs (id, flow_id, account_id, profile_id, current_node, context) VALUES ($1, $2, $3, $4, 'e1', '{}')")
        .bind(Uuid::now_v7())
        .bind(fid)
        .bind(acct)
        .bind(dee)
        .execute(&h.state.db)
        .await
        .unwrap();
    let coupons::Prepared::Ready(c) = coupons::assign::prepare_send(&h.state, &spec(), Uuid::now_v7()).await.unwrap() else { panic!("held") };
    let mut ctx = json!({});
    c.fill(&mut ctx);
    assert_ne!(ctx["__dm_coupons"]["codes"]["flowy"]["code"].as_str().unwrap(), codes[0], "a new run is a new send");
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
        tasks.push(tokio::spawn(async move { coupons::give_code(&db, &c, p, msg(Uuid::now_v7()), None).await.unwrap() }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    let given: Vec<String> = got.iter().flatten().map(|g| g.code.clone()).collect();
    assert_eq!(given.len(), 30);
    assert_eq!(given.iter().collect::<std::collections::HashSet<_>>().len(), 30, "no two people share a code");
    assert_eq!(got.iter().filter(|g| g.is_none()).count(), 10, "the rest find the pool empty");

    // One person, ten sends at once: one code.
    let extra: Vec<String> = codes.iter().map(|c| format!("{c}-B")).collect();
    h.ok("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"codes": extra}))).await;
    let solo = subscriber(&h, acct, "solo@x.test", "subscribed").await;
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (db, c) = (h.state.db.clone(), c.clone());
        tasks.push(tokio::spawn(async move { coupons::give_code(&db, &c, solo, msg(Uuid::now_v7()), None).await.unwrap().unwrap().code }));
    }
    let mut seen = std::collections::HashSet::new();
    for t in tasks {
        seen.insert(t.await.unwrap());
    }
    assert_eq!(seen.len(), 1, "{seen:?}");

    // A store integration's hook fills in when the pool is empty, with no lock held.
    let empty = store::create(&h.state.db, acct, serde_json::from_value(json!({"name": "STORE"})).unwrap()).await.unwrap();
    let make = |_c: &store::Coupon, p: Uuid| -> coupons::StoreCodeFuture<'_> { Box::pin(async move { Ok(format!("SHOP-{}", &p.simple().to_string()[..8])) }) };
    let g = coupons::give_code(&h.state.db, &empty, solo, msg(Uuid::now_v7()), Some(&make)).await.unwrap().unwrap();
    assert!(g.code.starts_with("SHOP-"));
    let again = coupons::give_code(&h.state.db, &empty, solo, msg(Uuid::now_v7()), Some(&make)).await.unwrap().unwrap();
    assert_eq!(again.code, g.code, "the store isn't asked again for the same person");

    // An expired coupon refuses to hand out codes rather than sending dead ones.
    h.ok("PATCH", &format!("/api/coupons/{id}"), Some(json!({"expires_at": "2020-01-01T00:00:00Z"}))).await;
    let c = store::load(&h.state.db, acct, c.id).await.unwrap();
    assert!(coupons::give_code(&h.state.db, &c, solo, msg(Uuid::now_v7()), None).await.is_err());
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
    let send = |h: &Harness, email: String, ip: Option<&str>| {
        let router = h.router.clone();
        let body = json!({"form_id": fid, "email": email}).to_string();
        let path = format!("/client/subscribe?k={pk}");
        let ip = ip.map(str::to_string);
        async move {
            let mut req = axum::http::Request::builder().method("POST").uri(path).header("content-type", "application/json");
            if let Some(ip) = ip {
                req = req.header("x-real-ip", ip);
            }
            let res = tower::ServiceExt::oneshot(router, req.body(axum::body::Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let b = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&b).unwrap()
        }
    };
    let v = send(&h, "new@x.test".into(), Some("203.0.113.1")).await;
    assert!(v["discount_code"].as_str().unwrap().starts_with("FORM-"), "{v}");
    // Signing up again: no code shown.
    let v = send(&h, "new@x.test".into(), Some("203.0.113.2")).await;
    assert!(v["discount_code"].is_null(), "{v}");
    // Someone already in the audience: anyone could type their address, so no code on screen.
    h.ok("POST", "/api/profiles", Some(json!({"email": "known@x.test"}))).await;
    let v = send(&h, "known@x.test".into(), Some("203.0.113.3")).await;
    assert!(v["discount_code"].is_null(), "neither a unique nor the shared code: {v}");

    // One visitor making up addresses gets at most 3 codes an hour.
    let mut from_bot = 0;
    for i in 0..6 {
        if send(&h, format!("bot{i}@x.test"), Some("198.51.100.7")).await["discount_code"].is_string() {
            from_bot += 1;
        }
    }
    assert_eq!(from_bot, 3);
    // Sign-ups with no usable address share one allowance instead of skipping the limit.
    let mut no_ip = 0;
    for i in 0..5 {
        if send(&h, format!("anon{i}@x.test"), None).await["discount_code"].is_string() {
            no_ip += 1;
        }
    }
    assert_eq!(no_ip, 3);
    // And the coupon's hourly form limit holds across visitors.
    h.ok("PATCH", &format!("/api/coupons/{cid}"), Some(json!({"form_codes_per_hour": 9}))).await;
    let mut more = 0;
    for i in 0..5 {
        if send(&h, format!("crowd{i}@x.test"), Some(&format!("192.0.2.{i}"))).await["discount_code"].is_string() {
            more += 1;
        }
    }
    assert_eq!(more, 2, "1 + 3 + 3 given already, 9 an hour");
    let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE assigned_via = 'form' AND delivered_at IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(delivered, 9);
    h.cleanup().await;
}

#[tokio::test]
async fn double_opt_in_gives_the_code_earned_at_sign_up_once_and_old_links_show_none() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    coupon(&mut h, "DOI", &["DOI-1", "DOI-2", "DOI-3"]).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "double_opt_in": true}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list["id"], "config": {}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    let sign_up = |h: &Harness, email: &str| {
        let body = json!({"form_id": fid, "email": email}).to_string();
        let path = format!("/client/subscribe?k={pk}");
        let router = h.router.clone();
        async move {
            let req = axum::http::Request::builder().method("POST").uri(path).header("content-type", "application/json").body(axum::body::Body::from(body)).unwrap();
            let res = tower::ServiceExt::oneshot(router, req).await.unwrap();
            let b = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&b).unwrap()
        }
    };
    let link_for = |h: &Harness, to: &str| {
        let mail = h.state.email.sent().into_iter().filter(|m| m.to == to).last().unwrap();
        find_link(&mail.html, "https://pl.test/c/").trim_start_matches("https://pl.test").to_string()
    };

    // Signed up while the form had no coupon; the coupon is added before confirming: no code.
    assert_eq!(sign_up(&h, "early@x.test").await["confirm"], true);
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"config": {"coupon": "DOI"}}))).await;
    let early = link_for(&h, "early@x.test");
    let (_, _, page) = h.raw("POST", &early, &[], "").await;
    assert!(page.contains("You're subscribed") && !page.contains("DOI-"), "{page}");

    // Signed up with the coupon on the form: the code after confirming, the same on a reload.
    let v = sign_up(&h, "doi@x.test").await;
    assert!(v["discount_code"].is_null(), "no code before confirming: {v}");
    let path = link_for(&h, "doi@x.test");
    let (_, _, page) = h.raw("GET", &path, &[], "").await;
    assert!(!page.contains("DOI-"), "opening the link gives nothing");
    let (_, _, page) = h.raw("POST", &path, &[], "").await;
    let code = page.split("Your code: <strong>").nth(1).and_then(|s| s.split('<').next()).unwrap().to_string();
    assert!(code.starts_with("DOI-"), "{page}");
    let (_, _, page) = h.raw("POST", &path, &[], "").await;
    assert!(page.contains(&code), "the same code within a day");
    // A day later the old link shows the normal page with no code.
    sqlx::query("UPDATE optin_confirmations SET confirmed_at = now() - interval '25 hours'").execute(&h.state.db).await.unwrap();
    let (_, _, page) = h.raw("GET", &path, &[], "").await;
    assert!(page.contains("You're subscribed") && !page.contains("DOI-"), "{page}");

    // Someone already in the audience signs up: anyone can type their address, so no code is earned.
    h.ok("POST", "/api/profiles", Some(json!({"email": "known@x.test"}))).await;
    sign_up(&h, "known@x.test").await;
    let (_, _, page) = h.raw("POST", &link_for(&h, "known@x.test"), &[], "").await;
    assert!(!page.contains("DOI-"), "{page}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE status <> 'available'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1, "one code given in all");
    h.cleanup().await;
}

#[tokio::test]
async fn redemptions_are_matched_once_and_browser_orders_stay_unverified() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let acct = account_id(&h).await;
    let id = coupon(&mut h, "ORDERS", &["ORD-1", "ORD-2", "ORD-3"]).await;
    let c = store::load(&h.state.db, acct, id.parse().unwrap()).await.unwrap();
    let ann = subscriber(&h, acct, "ann@x.test", "subscribed").await;
    let bob = subscriber(&h, acct, "bob@x.test", "subscribed").await;
    let deliver = |db: sqlx::PgPool, c: store::Coupon, p: Uuid| async move {
        let g = coupons::give_code(&db, &c, p, msg(Uuid::now_v7()), None).await.unwrap().unwrap();
        sqlx::query("UPDATE coupon_codes SET delivered_at = now() WHERE id = $1").bind(g.id).execute(&db).await.unwrap();
        g.code
    };
    let ann_code = deliver(h.state.db.clone(), c.clone(), ann).await;
    let bob_code = deliver(h.state.db.clone(), c.clone(), bob).await;
    let browser = |h: &Harness, body: Value| {
        let path = format!("/client/events?k={pk}");
        let body = body.to_string();
        let router = h.router.clone();
        async move {
            let req = axum::http::Request::builder().method("POST").uri(path).header("content-type", "application/json").body(axum::body::Body::from(body)).unwrap();
            assert_eq!(tower::ServiceExt::oneshot(router, req).await.unwrap().status(), StatusCode::OK);
        }
    };
    let row = |h: &Harness, code: &str| {
        let (db, code) = (h.state.db.clone(), code.to_string());
        async move {
            let r: (Option<chrono::DateTime<chrono::Utc>>, Option<f64>, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) =
                sqlx::query_as("SELECT redeemed_at, order_value, browser_order_at, delivered_at FROM coupon_codes WHERE code = $1").bind(code).fetch_one(&db).await.unwrap();
            r
        }
    };

    // A forged browser order by "ann" with bob's code touches nothing.
    browser(&h, json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 99999, "properties": {"discount_codes": [bob_code.to_lowercase()]}})).await;
    // A browser order with ann's own code: kept as unverified evidence only.
    browser(&h, json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 5000, "properties": {"discount_codes": [ann_code.clone()]}})).await;
    coupons::redeem::reconcile(&h.state.db).await.unwrap();
    let r = row(&h, &bob_code).await;
    assert!(r.0.is_none() && r.2.is_none(), "{r:?}");
    let r = row(&h, &ann_code).await;
    assert!(r.0.is_none() && r.2.is_some(), "unverified evidence, not a redemption: {r:?}");
    let rep = h.ok("GET", &format!("/api/coupons/{id}/report"), None).await;
    assert_eq!((rep["redeemed"].as_i64(), rep["redeemed_unverified"].as_i64(), rep["revenue"].as_f64()), (Some(0), Some(1), Some(0.0)), "{rep}");

    // The store's own order verifies it once; a second trusted order with the same code, and the
    // reconciler seeing everything again, change nothing.
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 80.0, "unique_id": "shop-1001",
        "properties": {"discount_codes": [{"code": ann_code.to_lowercase(), "amount": 8}]}}))).await;
    coupons::redeem::reconcile(&h.state.db).await.unwrap();
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}, "value": 120.0, "unique_id": "shop-1002",
        "properties": {"discount_code": ann_code.clone()}}))).await;
    coupons::redeem::reconcile(&h.state.db).await.unwrap();
    // The cursor goes back and the memory of seen events is lost: still one redemption.
    sqlx::query("UPDATE coupon_reconcile SET cursor_id = '00000000-0000-7000-8000-000000000000'").execute(&h.state.db).await.unwrap();
    coupons::redeem::reconcile(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM coupon_orders_seen").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE coupon_reconcile SET cursor_id = '00000000-0000-7000-8000-000000000000'").execute(&h.state.db).await.unwrap();
    coupons::redeem::reconcile(&h.state.db).await.unwrap();
    let r = row(&h, &ann_code).await;
    assert!(r.0.is_some() && r.1 == Some(80.0), "{r:?}");
    let rep = h.ok("GET", &format!("/api/coupons/{id}/report"), None).await;
    assert_eq!((rep["given"].as_i64(), rep["redeemed"].as_i64(), rep["redeemed_unverified"].as_i64(), rep["revenue"].as_f64()), (Some(2), Some(1), Some(0), Some(80.0)), "{rep}");
    assert_eq!(rep["redemption_rate"], 0.5);
    assert_eq!(rep["days"].as_array().unwrap().last().unwrap()["redeemed"], 1);
    // An order never makes a code count as given.
    let carl = subscriber(&h, acct, "carl@x.test", "subscribed").await;
    let carl_code = coupons::give_code(&h.state.db, &c, carl, msg(Uuid::now_v7()), None).await.unwrap().unwrap().code;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "carl@x.test"}, "value": 10.0, "properties": {"discount_codes": [carl_code.clone()]}}))).await;
    coupons::redeem::reconcile(&h.state.db).await.unwrap();
    let r = row(&h, &carl_code).await;
    assert!(r.0.is_some() && r.3.is_none(), "redeemed, still not counted as given: {r:?}");

    // Segments: given / redeemed, and a coupon that doesn't exist matches nobody either way round.
    let seg = |cid: &str, action: &str, op: &str| json!({"definition": {"match": "all", "conditions": [{"type": "coupon", "coupon_id": cid, "action": action, "op": op}]}});
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg(&id, "given", "has"))).await["count"], 2);
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg(&id, "redeemed", "has"))).await["count"], 2);
    assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg(&id, "redeemed", "has_not"))).await["count"], 1);
    let nobody = Uuid::now_v7().to_string();
    for op in ["has", "has_not"] {
        assert_eq!(h.ok("POST", "/api/segments/preview", Some(seg(&nobody, "redeemed", op))).await["count"], 0, "{op}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn coupons_in_use_cant_be_deleted_and_deleted_people_lose_their_link() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    let id = coupon(&mut h, "USED", &["USE-1", "USE-2"]).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Not redeemed", "definition": {"match": "all", "conditions": [
        {"type": "coupon", "coupon_id": id, "action": "redeemed", "op": "has_not"}]}}))).await;
    let (s, v) = h.req("DELETE", &format!("/api/coupons/{id}?confirm=true"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("segment \"Not redeemed\""), "{v}");
    h.ok("DELETE", &format!("/api/segments/{}", seg["id"].as_str().unwrap()), None).await;
    // A draft campaign and a flow step using it by name block it too.
    let list = list_with(&mut h, &[("z@x.test", "subscribed")]).await;
    let camp = campaign(&mut h, &list, coupon_email("USED")).await;
    let (s, v) = h.req("DELETE", &format!("/api/coupons/{id}?confirm=true"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("campaign \"Sale\""), "{v}");
    h.ok("DELETE", &format!("/api/campaigns/{camp}"), None).await;

    // A deleted person's code stays used but keeps no link to them.
    let p = subscriber(&h, acct, "gone@x.test", "subscribed").await;
    let c = store::load(&h.state.db, acct, id.parse().unwrap()).await.unwrap();
    let g = coupons::give_code(&h.state.db, &c, p, msg(Uuid::now_v7()), None).await.unwrap().unwrap();
    sqlx::query("DELETE FROM profiles WHERE id = $1").bind(p).execute(&h.state.db).await.unwrap();
    let (status, pid, key, mid): (String, Option<Uuid>, Option<String>, Option<Uuid>) =
        sqlx::query_as("SELECT status, profile_id, send_key, message_id FROM coupon_codes WHERE id = $1").bind(g.id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), pid, key, mid), ("assigned", None, None, None));
    let other = subscriber(&h, acct, "next@x.test", "subscribed").await;
    let next = coupons::give_code(&h.state.db, &c, other, msg(Uuid::now_v7()), None).await.unwrap().unwrap();
    assert_ne!(next.code, g.code, "never back to stock");
    h.cleanup().await;
}

#[tokio::test]
async fn clicks_dont_keep_codes_and_transactional_email_refuses_coupons() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    coupon(&mut h, "LINKY", &["LINK-AB12"]).await;
    let list = list_with(&mut h, &[("ann@x.test", "subscribed")]).await;
    let content = json!({"subject": "Shop", "blocks": [
        {"type": "text", "html": "<p>CODE[{% coupon_code 'LINKY' %}]</p>"},
        {"type": "button", "text": "Apply it", "href": "https://acme.test/discount/{% coupon_code 'LINKY' %}?next=/"}]});
    let camp = campaign(&mut h, &list, content).await;
    h.ok("POST", &format!("/api/campaigns/{camp}/send"), None).await;
    h.drain().await;
    let mail = mails_to(&h, "ann@x.test").pop().unwrap();
    let link = find_link(&mail.html, "https://pl.test/t/c/").replace("&amp;", "&");
    let (s, hd, _) = h.raw("GET", link.trim_start_matches("https://pl.test"), &[], "").await;
    assert!(s.is_redirection(), "{s}");
    assert!(hd.get("location").unwrap().to_str().unwrap().contains("/discount/LINK-AB12"), "the redirect keeps the code");
    let stored: String = sqlx::query_scalar("SELECT url FROM message_clicks").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(stored, "https://acme.test/discount/{coupon_code}?next=/");
    let ev: Value = sqlx::query_scalar("SELECT e.properties FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Clicked Email'").fetch_one(&h.state.db).await.unwrap();
    assert!(!ev.to_string().contains("LINK-AB12"), "{ev}");

    h.ok("PATCH", "/api/transactional/settings", Some(json!({"enabled": true, "acknowledge": true}))).await;
    let (s, v) = h.req("POST", "/api/transactional/templates", Some(json!({"name": "Receipt", "content": {"subject": "Your receipt", "blocks": [
        {"type": "text", "html": "<p>Thanks. {% coupon_code 'LINKY' %}</p>"}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("transactional"), "{v}");
    h.cleanup().await;
}

#[tokio::test]
async fn mcp_tools_cover_coupons_and_delete_needs_confirm() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let ctx = dumbmail::auth::Ctx { account_id: acct, user_id: None, via_api_key: true };
    let names: Vec<String> = dumbmail::features::mcp_tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for n in ["list_coupons", "get_coupon", "create_coupon", "update_coupon", "add_coupon_codes", "generate_coupon_codes", "export_coupon_codes", "delete_coupon", "coupon_report"] {
        assert!(names.iter().any(|x| x == n), "{n}");
    }
    let call = |name: &'static str, args: Value| {
        let (state, ctx) = (h.state.clone(), ctx.clone());
        async move { dumbmail::features::mcp_call(&state, &ctx, name, &args).await.unwrap() }
    };
    call("create_coupon", json!({"name": "MCP", "expires_after_days": 14})).await.unwrap();
    let r = call("add_coupon_codes", json!({"id": "mcp", "text": "MCP-1\nMCP-2\nbad code"})).await.unwrap();
    assert_eq!((r["added"].as_i64(), r["refused"].as_i64()), (Some(2), Some(1)), "{r}");
    let r = call("generate_coupon_codes", json!({"id": "MCP", "count": 3, "pattern": {"prefix": "G", "length": 7}})).await.unwrap();
    assert_eq!(r["added"], 3);
    let g = call("get_coupon", json!({"id": "MCP", "search": "MCP-"})).await.unwrap();
    assert_eq!(g["codes"]["total"], 2);
    assert_eq!(g["stock"]["total"], 5);
    let e = call("export_coupon_codes", json!({"id": "MCP", "format": "codes", "limit": 4})).await.unwrap();
    assert_eq!(e["count"], 4);
    let e2 = call("export_coupon_codes", json!({"id": "MCP", "format": "codes", "after": e["next_after"]})).await.unwrap();
    assert_eq!(e2["count"], 1);
    assert!(e2["next_after"].is_null());
    let dry = call("delete_coupon", json!({"id": "MCP"})).await.unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(call("list_coupons", json!({})).await.unwrap()["total"], 1, "nothing deleted without confirm");
    assert_eq!(call("coupon_report", json!({})).await.unwrap()["data"][0]["name"], "MCP");
    call("delete_coupon", json!({"id": "MCP", "confirm": true})).await.unwrap();
    assert_eq!(call("list_coupons", json!({})).await.unwrap()["total"], 0);
    h.cleanup().await;
}

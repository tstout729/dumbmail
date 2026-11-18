//! Reviews: asking (flows, campaigns, the API), the hosted page, moderation and its honesty rules,
//! the public widget endpoints, photos, public reviews, import and export, segments, and the
//! attacks and races each rule is there for.

#[macro_use]
mod common;

use axum::http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn site_key(h: &Harness, acct: Uuid) -> String {
    sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap()
}

async fn person(h: &Harness, acct: Uuid, email: &str, consent: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent, first_name, last_name) VALUES ($1, $2, $3, $4, 'Jane', 'doe')")
        .bind(id)
        .bind(acct)
        .bind(email)
        .bind(consent)
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

async fn product(h: &mut Harness, id: &str, title: &str) {
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": id, "title": title, "url": format!("https://acme.test/products/{id}"), "image_url": "https://acme.test/i.png", "price": 20}))).await;
}

async fn order(h: &mut Harness, email: &str, order_id: &str, items: Value, unique: &str) {
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": email}, "properties": {"order_id": order_id, "items": items}, "unique_id": unique}))).await;
}

fn mails_to(h: &Harness, to: &str) -> Vec<dumbmail::providers::email::OutgoingEmail> {
    h.state.email.sent().into_iter().filter(|m| m.to == to).collect()
}

/// The review link tokens in an email, in order (each item once).
fn tokens(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in html.split("https://pl.test/e/review/").skip(1) {
        let t: String = part.chars().take_while(|c| !matches!(c, '"' | '?' | '<' | ' ' | '&')).collect();
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

async fn live_review_flow(h: &mut Harness) -> String {
    let f = h.ok("POST", "/api/reviews/flows", None).await;
    let id = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
    id
}

async fn submit(h: &Harness, token: &str, body: Value) -> (StatusCode, Value) {
    let (s, _, b) = h.raw("POST", &format!("/e/review/{token}"), &[("content-type", "application/json"), ("x-real-ip", "203.0.113.9")], &body.to_string()).await;
    (s, serde_json::from_str(&b).unwrap_or(json!({"_raw": b})))
}

async fn widget(h: &Harness, path: &str) -> Value {
    let (s, _, b) = h.raw("GET", path, &[], "").await;
    assert_eq!(s, StatusCode::OK, "{path}: {b}");
    serde_json::from_str(&b).unwrap()
}

async fn summary_row(h: &Harness, acct: Uuid, key: &str) -> (i32, i64, Vec<i32>) {
    sqlx::query_as("SELECT review_count, rating_sum, rating_counts FROM review_subjects WHERE account_id = $1 AND external_id = $2").bind(acct).bind(key).fetch_one(&h.state.db).await.unwrap()
}

/// A review link for `email` and `product` through the API.
async fn api_link(h: &mut Harness, email: &str, product: &str, verified: bool) -> String {
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": email, "product_id": product, "verified": verified}))).await;
    let url = v["requests"][0]["url"].as_str().unwrap_or_else(|| panic!("no link: {v}")).to_string();
    url.rsplit('/').next().unwrap().to_string()
}

#[tokio::test]
async fn the_flow_asks_once_and_a_verified_review_goes_live() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    product(&mut h, "CUP", "Red cup").await;
    let jane = person(&h, acct, "jane@x.test", "subscribed").await;
    person(&h, acct, "bob@x.test", "never_subscribed").await;
    live_review_flow(&mut h).await;

    order(&mut h, "jane@x.test", "1001", json!([{"product_id": "MUG", "name": "Blue mug"}, {"product_id": "CUP", "name": "Red cup"}]), "o1").await;
    order(&mut h, "bob@x.test", "1002", json!([{"product_id": "MUG"}]), "o2").await;
    h.drain().await;
    assert!(mails_to(&h, "jane@x.test").is_empty(), "the ask waits for the delay");
    h.fast_forward().await;
    let asks = mails_to(&h, "jane@x.test");
    assert_eq!(asks.len(), 1, "one email asks about both items");
    assert!(mails_to(&h, "bob@x.test").is_empty(), "a review request is marketing: never_subscribed gets none");
    let ts = tokens(&asks[0].html);
    assert_eq!(ts.len(), 2, "one link per item: {}", asks[0].html);
    assert!(asks[0].html.contains("?rating=5"), "a link per star");
    let ready: i64 = sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Ready To Review' AND e.profile_id = $1").bind(jane).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ready, 2);

    // Opening a star link saves nothing.
    let (s, _, page) = h.raw("GET", &format!("/e/review/{}?rating=4", ts[0]), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("Blue mug") || page.contains("Red cup"));
    assert!(!page.contains("jane@x.test"), "the bearer page never shows the address");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);

    // Two submissions with one link at once: one review.
    let body = json!({"rating": 5, "title": "Great <b>mug</b>", "body": "Love it {{ 7*7 }} <script>alert(1)</script>"});
    let (a, b) = tokio::join!(submit(&h, &ts[0], body.clone()), submit(&h, &ts[0], body.clone()));
    assert!(a.0.is_success() || b.0.is_success(), "{a:?} {b:?}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1, "one link, one review");
    let first = if a.0.is_success() { &a.1 } else { &b.1 };
    assert_eq!(first["status"], "published", "verified, 5 stars, nothing flagged: {first}");
    assert_eq!(first["others"].as_array().unwrap().len(), 1, "the page offers the other item");
    h.drain().await;

    // The widget shows it with its summary, and nothing about who wrote it.
    let mugs = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG")).await;
    let cups = widget(&h, &format!("/client/reviews?k={pk}&product_id=CUP")).await;
    let (w, other) = if mugs["summary"]["count"] == 1 { (mugs, cups) } else { (cups, mugs) };
    assert_eq!(w["summary"]["count"], 1);
    assert_eq!(w["summary"]["average"], 5.0);
    assert_eq!(other["summary"]["count"], 0);
    let r = &w["reviews"][0];
    assert_eq!(r["verified"], true);
    assert_eq!(r["name"], "Jane D.");
    assert_eq!(r["title"], "Great <b>mug</b>", "stored as plain text; the widget escapes it");
    let raw = w.to_string();
    assert!(!raw.contains("jane@x.test") && !raw.contains(&jane.to_string()), "no email or profile id in public answers");

    // Submitted Review was recorded once, and a browser can't forge it (or Ready To Review).
    let submitted: i64 = sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Submitted Review' AND e.profile_id = $1").bind(jane).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(submitted, 1);
    for metric in ["Submitted Review", "Ready To Review", " submitted review "] {
        let body = json!({"metric": metric, "profile": {"email": "jane@x.test"}, "properties": {"rating": 1, "product_id": "MUG"}}).to_string();
        let (st, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{metric:?} from a browser: {b}");
        let (st, _) = h.req("POST", "/api/events", Some(json!({"metric": metric, "profile": {"email": "jane@x.test"}}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{metric:?} only DumbMail records");
    }

    // The reminder goes only about the item she hasn't reviewed, once.
    h.fast_forward().await;
    h.fast_forward().await;
    let mails = mails_to(&h, "jane@x.test");
    assert_eq!(mails.len(), 2, "the ask and one reminder");
    assert_eq!(tokens(&mails[1].html), vec![ts[1].clone()], "the reminder is about the other item only");

    // The same order again never asks again.
    order(&mut h, "jane@x.test", "1001", json!([{"product_id": "MUG"}, {"product_id": "CUP"}]), "o3").await;
    h.drain().await;
    for _ in 0..3 {
        h.fast_forward().await;
    }
    assert_eq!(mails_to(&h, "jane@x.test").len(), 2, "one ask per order line, ever");
    h.cleanup().await;
}

#[tokio::test]
async fn moderation_keeps_summaries_right_and_follows_the_honesty_rules() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    for e in ["a@x.test", "b@x.test", "c@x.test"] {
        person(&h, acct, e, "subscribed").await;
    }
    let low = api_link(&mut h, "a@x.test", "MUG", true).await;
    let unverified = api_link(&mut h, "b@x.test", "MUG", false).await;
    let flagged = api_link(&mut h, "c@x.test", "MUG", true).await;
    let (_, v1) = submit(&h, &low, json!({"rating": 2, "body": "Chipped"})).await;
    let (_, v2) = submit(&h, &unverified, json!({"rating": 5, "body": "Nice"})).await;
    let (_, v3) = submit(&h, &flagged, json!({"rating": 5, "body": "Buy cheaper at https://spam.test"})).await;
    for v in [&v1, &v2, &v3] {
        assert_eq!(v["status"], "pending", "held: low rating, unverified, flagged: {v}");
    }
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0);
    let q = h.ok("GET", "/api/reviews?status=pending", None).await;
    assert_eq!(q["total"], 3);
    assert!(q["note"].as_str().unwrap().contains("unlawful"));
    let flagged_row = q["data"].as_array().unwrap().iter().find(|r| r["body"].as_str().unwrap().contains("spam")).unwrap();
    assert_eq!(flagged_row["flags"], json!(["link"]));
    let ids: Vec<String> = q["data"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();

    // Bulk publishing is a dry run without confirm, and says how many are low ratings.
    let dry = h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": ids, "action": "publish"}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["low_ratings"], 1);
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0);
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": ids, "action": "publish", "confirm": true}))).await;
    assert_eq!(summary_row(&h, acct, "MUG").await, (3, 12, vec![0, 1, 0, 0, 2]));
    let w = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG")).await;
    assert_eq!(w["summary"]["average"], 4.0);
    assert_eq!(w["summary"]["histogram"]["2"], 1);

    // Rejecting needs a reason, and never deletes.
    let spam = flagged_row["id"].as_str().unwrap().to_string();
    let (st, v) = h.req("POST", "/api/reviews/moderate", Some(json!({"ids": [spam], "action": "reject"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, _) = h.req("POST", "/api/reviews/moderate", Some(json!({"ids": [spam], "action": "reject", "reason": "low_rating"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "the rating is never a reason");
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": [spam], "action": "reject", "reason": "spam"}))).await;
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": [spam], "action": "reject", "reason": "spam"}))).await;
    assert_eq!(summary_row(&h, acct, "MUG").await, (2, 7, vec![0, 1, 0, 0, 1]), "rejecting twice takes it out once");
    assert_eq!(h.ok("GET", &format!("/api/reviews/{spam}"), None).await["status"], "rejected");

    // A reply shows under the review; the business can't change what it says.
    let low_id = v1["review_id"].as_str().unwrap();
    h.ok("POST", &format!("/api/reviews/{low_id}/reply"), Some(json!({"text": "Sorry! We've sent a new one."}))).await;
    let (st, _) = h.req("PATCH", &format!("/api/reviews/{low_id}"), Some(json!({"body": "Perfect"}))).await;
    assert!(st == StatusCode::METHOD_NOT_ALLOWED || st == StatusCode::NOT_FOUND || st == StatusCode::FORBIDDEN, "no edit route: {st}");
    let w = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG&rating=2")).await;
    assert_eq!(w["reviews"][0]["reply"]["text"], "Sorry! We've sent a new one.");
    assert_eq!(w["reviews"][0]["body"], "Chipped");

    // Featured reviews lead the carousel.
    let v2_id = v2["review_id"].as_str().unwrap();
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": [v2_id], "action": "feature"}))).await;
    let car = widget(&h, &format!("/client/reviews/all?k={pk}&featured=true")).await;
    assert_eq!(car["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(car["reviews"][0]["id"], v2_id);
    assert_eq!(car["reviews"][0]["product"]["name"], "Blue mug");

    // Deleting is separate and confirmed.
    let dry = h.ok("DELETE", &format!("/api/reviews/{low_id}"), None).await;
    assert_eq!(dry["dry_run"], true);
    h.ok("DELETE", &format!("/api/reviews/{low_id}?confirm=true"), None).await;
    assert_eq!(summary_row(&h, acct, "MUG").await, (1, 5, vec![0, 0, 0, 0, 1]));

    // A summary knocked out of line is recounted by the rebuild job.
    sqlx::query("UPDATE review_subjects SET review_count = 40, rating_sum = 3, rating_counts = '{9,9,9,9,4}'").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/reviews/products/rebuild", None).await;
    h.drain().await;
    assert_eq!(summary_row(&h, acct, "MUG").await, (1, 5, vec![0, 0, 0, 0, 1]));

    // Every moderation action is in the audit log.
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log WHERE account_id = $1 AND action LIKE 'review%' ORDER BY created_at").bind(acct).fetch_all(&h.state.db).await.unwrap();
    for a in ["review.publish", "review.reject", "review.reply", "review.feature", "review.delete", "reviews.rebuild"] {
        assert!(actions.iter().any(|x| x == a), "{a} missing from {actions:?}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn edits_deletes_and_expired_links() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    person(&h, acct, "a@x.test", "subscribed").await;
    let t = api_link(&mut h, "a@x.test", "MUG", true).await;
    submit(&h, &t, json!({"rating": 5, "body": "Great"})).await;
    assert_eq!(summary_row(&h, acct, "MUG").await, (1, 5, vec![0, 0, 0, 0, 1]));
    // An edit within a day re-runs the rules: a 2-star edit goes back to the queue.
    let (s, v) = submit(&h, &t, json!({"rating": 2, "body": "Broke after a week"})).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0, "the edited review waits for moderation");
    // After a day it can't be changed any more.
    sqlx::query("UPDATE reviews SET created_at = now() - interval '2 days'").execute(&h.state.db).await.unwrap();
    let (s, _) = submit(&h, &t, json!({"rating": 5})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // The author can still delete it: words and photos are erased, the business sees "removed".
    let (s, _, _) = h.raw("POST", &format!("/e/review/{t}/delete"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    let (status, body): (String, String) = sqlx::query_as("SELECT status, body FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), body.as_str()), ("removed", ""));
    // Expired links take nothing; forged tokens find nothing.
    person(&h, acct, "b@x.test", "subscribed").await;
    let t2 = api_link(&mut h, "b@x.test", "MUG", true).await;
    sqlx::query("UPDATE review_requests SET expires_at = now() - interval '1 minute' WHERE review_id IS NULL").execute(&h.state.db).await.unwrap();
    let (s, _) = submit(&h, &t2, json!({"rating": 5})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = submit(&h, "not-a-token", json!({"rating": 5})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Someone who reviewed it in the last year isn't asked again.
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "a@x.test", "product_id": "MUG"}))).await;
    assert_eq!(v["requests"][0]["skipped"], "reviewed");
    h.cleanup().await;
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([10, 120, 200]))).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

#[tokio::test]
async fn photos_are_reencoded_and_only_served_while_published() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    person(&h, acct, "a@x.test", "subscribed").await;
    let t = api_link(&mut h, "a@x.test", "MUG", true).await;
    let (s, _, b) = h.bytes("POST", &format!("/e/review/{t}/photos"), "image/svg+xml", b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
    let mut ids = Vec::new();
    for _ in 0..5 {
        let (s, _, b) = h.bytes("POST", &format!("/e/review/{t}/photos"), "image/png", png(2000, 1000)).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        ids.push(serde_json::from_slice::<Value>(&b).unwrap()["id"].as_str().unwrap().to_string());
    }
    let (s, _, _) = h.bytes("POST", &format!("/e/review/{t}/photos"), "image/png", png(10, 10)).await;
    assert_eq!(s, StatusCode::CONFLICT, "at most 5 photos");
    let file = |id: &str| format!("/i/rv/{}.jpg", id.replace('-', ""));
    let (s, _, _) = h.raw("GET", &file(&ids[0]), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "not served before the review is published");
    // A 2-star review is held: its photos stay private.
    let (_, v) = submit(&h, &t, json!({"rating": 2, "photos": ids})).await;
    let rid = v["review_id"].as_str().unwrap().to_string();
    let (s, _, _) = h.raw("GET", &file(&ids[0]), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // The dashboard sees them through signed addresses.
    let r = h.ok("GET", &format!("/api/reviews/{rid}"), None).await;
    let signed = r["photos"][0]["url"].as_str().unwrap().trim_start_matches("https://pl.test").to_string();
    let (s, hd, body) = h.bytes("GET", &signed, "", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("content-type").unwrap(), "image/jpeg");
    assert!(body.starts_with(&[0xff, 0xd8, 0xff]) && !body.windows(4).any(|w| w == b"Exif"));
    let tampered = signed.replace("sig=", "sig=x");
    assert_eq!(h.bytes("GET", &tampered, "", vec![]).await.0, StatusCode::NOT_FOUND);
    // Published: public. A removed photo is gone at once.
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": [rid], "action": "publish"}))).await;
    let w = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG")).await;
    assert_eq!(w["reviews"][0]["photos"].as_array().unwrap().len(), 5);
    assert_eq!(w["reviews"][0]["photos"][0]["width"], 1600, "shrunk to 1600 pixels");
    let (s, _, _) = h.raw("GET", &file(&ids[0]), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    h.ok("DELETE", &format!("/api/reviews/{rid}/photos/{}", ids[0]), None).await;
    let (s, _, _) = h.raw("GET", &file(&ids[0]), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": [rid], "action": "hold"}))).await;
    let (s, _, _) = h.raw("GET", &file(&ids[1]), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unpublishing hides its photos");
    h.cleanup().await;
}

#[tokio::test]
async fn other_accounts_and_weaker_keys_are_refused() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    person(&h, acct, "a@x.test", "subscribed").await;
    let t = api_link(&mut h, "a@x.test", "MUG", true).await;
    let (_, v) = submit(&h, &t, json!({"rating": 5, "body": "Lovely"})).await;
    let rid = v["review_id"].as_str().unwrap().to_string();
    let read_key = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["content:read"]}))).await["key"].as_str().unwrap().to_string();
    let owner_cookie = h.cookie.clone();

    // A second account sees and changes nothing of the first's.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "correct horse", "company": "Other"}))).await;
    let other = account_id(&h).await;
    assert_ne!(other, acct);
    let other_pk = site_key(&h, other).await;
    assert_eq!(h.req("GET", &format!("/api/reviews/{rid}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.req("POST", "/api/reviews/moderate", Some(json!({"ids": [rid], "action": "reject", "reason": "spam"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.req("DELETE", &format!("/api/reviews/{rid}?confirm=true"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.req("POST", &format!("/api/reviews/{rid}/reply"), Some(json!({"text": "hi"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.ok("GET", "/api/reviews", None).await["total"], 0);
    let w = widget(&h, &format!("/client/reviews?k={other_pk}&product_id=MUG")).await;
    assert_eq!(w["summary"]["count"], 0, "the site key scopes everything");
    let (s, _, _) = h.raw("GET", "/client/reviews?k=nope&product_id=MUG", &[], "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // A read-only key reads but can't moderate, delete or export.
    h.cookie = owner_cookie;
    let call = |method: &'static str, path: String, body: Option<Value>| {
        let h = &h;
        let key = read_key.clone();
        async move {
            let b = axum::http::Request::builder().method(method).uri(path).header("authorization", format!("Bearer {key}"));
            let req = match body {
                Some(v) => b.header("content-type", "application/json").body(axum::body::Body::from(v.to_string())).unwrap(),
                None => b.body(axum::body::Body::empty()).unwrap(),
            };
            tower::ServiceExt::oneshot(h.router.clone(), req).await.unwrap().status()
        }
    };
    assert_eq!(call("GET", "/api/reviews".into(), None).await, StatusCode::OK);
    assert_eq!(call("POST", "/api/reviews/moderate".into(), Some(json!({"ids": [rid], "action": "hold"}))).await, StatusCode::FORBIDDEN);
    assert_eq!(call("DELETE", format!("/api/reviews/{rid}?confirm=true"), None).await, StatusCode::FORBIDDEN);
    assert_eq!(call("GET", "/api/reviews/export".into(), None).await, StatusCode::FORBIDDEN);
    // Content read alone sees review text but not who wrote it.
    let b = axum::http::Request::builder().uri("/api/reviews").header("authorization", format!("Bearer {read_key}")).body(axum::body::Body::empty()).unwrap();
    let res = tower::ServiceExt::oneshot(h.router.clone(), b).await.unwrap();
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["data"][0]["body"], "Lovely");
    assert!(v["data"][0].get("author").is_none(), "{v}");
    assert!(h.ok("GET", "/api/reviews", None).await["data"][0]["author"]["email"] == "a@x.test", "the owner may read profiles");
    h.cleanup().await;
}

async fn post(h: &Harness, pk: &str, body: String, ip: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    h.raw("POST", &format!("/client/reviews/write?k={pk}"), &[("content-type", "application/json"), ("x-real-ip", ip)], &body).await
}

async fn matches(h: &Harness, acct: Uuid, profile: Uuid, cond: Value) -> bool {
    let d = dumbmail::segments::load(&h.state.db, acct, &json!({"match": "all", "conditions": [cond]}), None).await.unwrap();
    dumbmail::segments::profile_matches(&h.state.db, profile, &d).await.unwrap()
}

#[tokio::test]
async fn public_reviews_are_off_by_default_confirmed_by_email_and_always_held() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    let write = |n: u8| json!({"product_id": "MUG", "email": format!("v{n}@x.test"), "rating": 5, "body": "Ok"}).to_string();
    let (s, _, _) = post(&h, &pk, write(1), "198.51.100.1").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "off by default");
    h.ok("PATCH", "/api/reviews/settings", Some(json!({"public_reviews": true}))).await;
    let (s, _, b) = post(&h, &pk, json!({"product_id": "NOPE", "email": "v1@x.test", "rating": 5}).to_string(), "198.51.100.1").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "only products the store has: {b}");
    let (s, _, b) = post(&h, &pk, write(1), "198.51.100.1").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mail = mails_to(&h, "v1@x.test");
    assert_eq!(mail.len(), 1, "a confirmation email");
    let link = find_link(&mail[0].html, "https://pl.test/e/review-confirm/").trim_start_matches("https://pl.test").to_string();
    let (n, attached): (i64, Option<Uuid>) = sqlx::query_as("SELECT count(*), max(profile_id::text)::uuid FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((n, attached), (1, None), "not attached to anyone until confirmed");
    assert_eq!(h.ok("GET", "/api/reviews?unconfirmed=true", None).await["total"], 1);
    let (s, _, page) = h.raw("GET", &link, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("<form method=\"post\""), "opening the link confirms nothing");
    assert!(sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>("SELECT confirmed_at FROM reviews").fetch_one(&h.state.db).await.unwrap().is_none());
    let (s, _, _) = h.raw("POST", &link, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let r = &h.ok("GET", "/api/reviews?status=pending", None).await["data"][0];
    assert_eq!(r["verified"], false);
    assert_eq!(r["awaiting_confirmation"], false);
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0, "unverified reviews are always held");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'v1@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed", "a review never subscribes anyone");
    // Limits: 5 an hour from one visitor.
    for n in 2..=5 {
        let (s, _, b) = post(&h, &pk, write(n), "198.51.100.1").await;
        assert_eq!(s, StatusCode::OK, "{n}: {b}");
    }
    let (s, _, _) = post(&h, &pk, write(6), "198.51.100.1").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    // ... and 3 a day for one address, from anywhere.
    for ip in ["198.51.100.2", "198.51.100.3"] {
        let (s, _, _) = post(&h, &pk, write(1), ip).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _, _) = post(&h, &pk, write(1), "198.51.100.4").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    h.cleanup().await;
}

#[tokio::test]
async fn emails_carry_review_blocks_only_for_subscribers() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    let a = person(&h, acct, "sub@x.test", "subscribed").await;
    let b = person(&h, acct, "never@x.test", "never_subscribed").await;
    // A published review to show.
    let t = api_link(&mut h, "sub@x.test", "MUG", true).await;
    submit(&h, &t, json!({"rating": 5, "title": "Best mug {% if 1 %}x{% endif %}", "body": "Use it daily"})).await;
    sqlx::query("UPDATE reviews SET incentivized = true").execute(&h.state.db).await.unwrap();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await["id"].as_str().unwrap().to_string();
    for p in [a, b] {
        sqlx::query("INSERT INTO list_members (list_id, profile_id) VALUES ($1::uuid, $2)").bind(&list).bind(p).execute(&h.state.db).await.unwrap();
    }
    let content = json!({"subject": "News", "blocks": [
        {"type": "text", "html": "<p>Tell us: <a href=\"{% review_link 'CUP' %}\">review</a></p>"},
        {"type": "reviews", "product_id": "MUG", "count": 3},
        {"type": "review_request", "product_id": "CUP", "heading": "Got a cup?"}
    ]});
    product(&mut h, "CUP", "Red cup").await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "News", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let sub = &mails_to(&h, "sub@x.test")[0];
    let never = &mails_to(&h, "never@x.test")[0];
    assert!(sub.html.contains("Best mug") && sub.html.contains("Received a discount for this review"), "{}", sub.html);
    assert!(sub.html.contains("&#123;&#37;"), "review text can't open a template tag");
    assert_eq!(tokens(&sub.html).len(), 1, "the tag and the block share one link for the cup");
    assert!(sub.html.contains("Got a cup?"));
    assert!(never.html.contains("Best mug"), "showing reviews is fine for everyone the campaign reaches");
    assert!(tokens(&never.html).is_empty() && !never.html.contains("Got a cup?"), "no review ask for people who aren't subscribed");
    let asked: i64 = sqlx::query_scalar("SELECT count(*) FROM review_requests WHERE profile_id = $1").bind(b).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(asked, 0);
    let verified: bool = sqlx::query_scalar("SELECT verified FROM review_requests rq JOIN review_subjects s ON s.id = rq.subject_id WHERE s.external_id = 'CUP'").fetch_one(&h.state.db).await.unwrap();
    assert!(!verified, "a campaign naming a product doesn't prove a purchase");
    // A preview makes nothing.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM review_requests").fetch_one(&h.state.db).await.unwrap();
    let p = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": content}))).await;
    assert!(p.to_string().contains("/e/review/preview"), "{p}");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM review_requests").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(before, after);
    h.cleanup().await;
}

#[tokio::test]
async fn import_export_segments_and_profile_deletion() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "777", "title": "Blue mug", "url": "https://acme.test/products/blue-mug"}))).await;
    let a = person(&h, acct, "a@x.test", "subscribed").await;
    let csv = "product_handle,rating,review_title,review_content,reviewer_name,reviewer_email,review_date,verified_purchase\n\
               blue-mug,5,Great,Love it,Jane D.,a@x.test,2025-03-01,yes\n\
               blue-mug,4,=HYPERLINK(\"x\"),Fine,Sam,,2025-04-01,no\n\
               blue-mug,9,Bad,,x,,,no\n";
    let dry = h.ok("POST", "/api/reviews/import", Some(json!({"csv": csv}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!((dry["importable"].as_i64(), dry["skipped"].as_i64()), (Some(2), Some(1)));
    assert_eq!(dry["mapping"]["product_handle"], "product_handle");
    assert_eq!(h.ok("GET", "/api/reviews", None).await["total"], 0, "a dry run imports nothing");
    let run = h.ok("POST", "/api/reviews/import", Some(json!({"csv": csv, "confirm": true}))).await;
    let (st, _) = h.req("POST", "/api/reviews/import", Some(json!({"csv": csv, "confirm": true}))).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "one import at a time");
    h.drain().await;
    // A replayed batch adds nothing.
    sqlx::query("UPDATE review_imports SET status = 'running', next_row = 0").execute(&h.state.db).await.unwrap();
    dumbmail::features::reviews::io::run_import(&h.state, &json!({"import_id": run["id"]})).await.unwrap();
    let done = h.ok("GET", &format!("/api/reviews/imports/{}", run["id"].as_str().unwrap()), None).await;
    assert_eq!(done["status"], "done");
    let list = h.ok("GET", "/api/reviews?source=import", None).await;
    assert_eq!(list["total"], 2, "{list}");
    assert!(list["data"].as_array().unwrap().iter().all(|r| r["imported"] == true && r["status"] == "published" && r["product"]["product_id"] == "777"));
    let w = widget(&h, &format!("/client/reviews?k={pk}&product_id=777")).await;
    assert_eq!(w["summary"]["count"], 2);
    assert_eq!(w["reviews"].as_array().unwrap().iter().filter(|r| r["verified"] == true).count(), 1, "verified as the file says");
    let ld = widget(&h, &format!("/client/reviews/jsonld?k={pk}&product_id=777")).await;
    assert_eq!(ld["@type"], "Product");
    assert_eq!(ld["aggregateRating"]["reviewCount"], 2);

    // Export: CSV with formulas made harmless, and emails for people who may read profiles.
    let (s, _, csv_out) = {
        let b = h.page_headers("GET", axum::http::Request::builder().uri("/api/reviews/export")).await;
        let res = tower::ServiceExt::oneshot(h.router.clone(), b.body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let st = res.status();
        let hd = res.headers().clone();
        (st, hd, String::from_utf8(http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes().to_vec()).unwrap())
    };
    assert_eq!(s, StatusCode::OK);
    assert!(csv_out.contains("'=HYPERLINK"), "{csv_out}");
    assert!(csv_out.contains("a@x.test"));

    // Segments: reviewed, rating, product, count, pending link.
    assert!(matches(&h, acct, a, json!({"type": "review"})).await);
    assert!(matches(&h, acct, a, json!({"type": "review", "product_id": "777", "min_rating": 5})).await);
    assert!(!matches(&h, acct, a, json!({"type": "review", "max_rating": 3})).await);
    assert!(!matches(&h, acct, a, json!({"type": "review", "count": 2})).await);
    assert!(matches(&h, acct, a, json!({"type": "review", "action": "pending_request", "op": "has_not"})).await);
    assert!(dumbmail::segments::load(&h.state.db, acct, &json!({"match": "all", "conditions": [{"type": "review", "min_rating": 9}]}), None).await.is_err());

    // Deleting the profile deletes their reviews and corrects the summary.
    h.ok("DELETE", &format!("/api/profiles/{a}"), None).await;
    assert_eq!(summary_row(&h, acct, "777").await, (1, 4, vec![0, 0, 0, 1, 0]));
    h.cleanup().await;
}

#[tokio::test]
async fn the_stay_flow_asks_after_check_out_about_the_listing() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    person(&h, acct, "guest@x.test", "subscribed").await;
    person(&h, acct, "held@x.test", "never_subscribed").await;
    sqlx::query("UPDATE profiles SET marketing_hold = true WHERE email = 'held@x.test'").execute(&h.state.db).await.unwrap();
    let f = h.ok("POST", "/api/reviews/flows", Some(json!({"kind": "stay"}))).await;
    assert_eq!(f["trigger"]["type"], "event_date");
    assert_eq!(f["trigger"]["field"], "check_out");
    assert_eq!(f["trigger"]["cancelled_by"], json!(["Reservation Cancelled"]));
    let id = f["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{id}/status"), Some(json!({"status": "live"}))).await;
    // Start it the way the check-out date would, straight from a Reservation Confirmed event.
    sqlx::query("UPDATE flows SET trigger = '{\"type\": \"metric\", \"metric\": \"Reservation Confirmed\"}' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    for who in ["guest@x.test", "held@x.test"] {
        h.ok("POST", "/api/events", Some(json!({"metric": "Reservation Confirmed", "profile": {"email": who}, "properties": {"reservation_id": format!("R-{who}"), "listing_id": "L1", "check_out": "2026-11-10"}}))).await;
    }
    h.drain().await;
    let mails = mails_to(&h, "guest@x.test");
    assert_eq!(mails.len(), 1);
    assert!(mails[0].html.contains("How was your stay?") && mails[0].html.contains("your stay"), "{}", mails[0].html);
    assert!(mails_to(&h, "held@x.test").is_empty(), "people held back from marketing are never asked");
    let (verified, kind): (bool, String) = sqlx::query_as("SELECT rq.verified, s.kind FROM review_requests rq JOIN review_subjects s ON s.id = rq.subject_id").fetch_one(&h.state.db).await.unwrap();
    assert!(verified, "a booking from the API is a real stay");
    assert_eq!(kind, "listing");
    // Listings get no Google review markup.
    let t = tokens(&mails[0].html);
    submit(&h, &t[0], json!({"rating": 5})).await;
    let pk = site_key(&h, acct).await;
    let (s, _, _) = h.raw("GET", &format!("/client/reviews/jsonld?k={pk}&product_id=L1"), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

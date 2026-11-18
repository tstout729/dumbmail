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
    let (s, v, _) = submit_as(h, token, body, None).await;
    (s, v)
}

/// Submit with this browser's edit cookie (if any); returns the cookie the answer sets.
async fn submit_as(h: &Harness, token: &str, body: Value, cookie: Option<&str>) -> (StatusCode, Value, Option<String>) {
    let mut hd = vec![("content-type", "application/json"), ("x-real-ip", "203.0.113.9")];
    if let Some(c) = cookie {
        hd.push(("cookie", c));
    }
    let (s, headers, b) = h.raw("POST", &format!("/e/review/{token}"), &hd, &body.to_string()).await;
    let set = headers.get("set-cookie").and_then(|v| v.to_str().ok()).map(|v| v.split(';').next().unwrap_or("").to_string());
    (s, serde_json::from_str(&b).unwrap_or(json!({"_raw": b})), set)
}

async fn widget(h: &Harness, path: &str) -> Value {
    let (s, _, b) = h.raw("GET", path, &[], "").await;
    assert_eq!(s, StatusCode::OK, "{path}: {b}");
    serde_json::from_str(&b).unwrap()
}

async fn summary_row(h: &Harness, acct: Uuid, key: &str) -> (i32, i64, Vec<i32>) {
    sqlx::query_as("SELECT review_count, rating_sum, rating_counts FROM review_subjects WHERE account_id = $1 AND external_id = $2").bind(acct).bind(key).fetch_one(&h.state.db).await.unwrap()
}

/// A review link for `email` and `product` through the API: verified with an order reference.
async fn api_link(h: &mut Harness, email: &str, product: &str, verified: bool) -> String {
    let order = verified.then(|| format!("O-{}", Uuid::now_v7().simple()));
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": email, "product_id": product, "order_id": order}))).await;
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
async fn the_link_shows_nothing_about_the_person_and_edits_need_the_writers_browser() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    person(&h, acct, "a@x.test", "subscribed").await;
    let t = api_link(&mut h, "a@x.test", "MUG", true).await;
    // The page names the item only: no name prefilled from the profile, nothing about consent.
    let (s, hd, page) = h.raw("GET", &format!("/e/review/{t}"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("cache-control").unwrap(), "no-store");
    assert_eq!(hd.get("referrer-policy").unwrap(), "no-referrer");
    assert!(page.contains("Blue mug"));
    assert!(!page.contains("Jane") && !page.contains("a@x.test"), "nothing about the person");
    let (s, v, cookie) = submit_as(&h, &t, json!({"rating": 5, "body": "Great secret thoughts"}), None).await;
    assert!(s.is_success(), "{v}");
    let cookie = cookie.expect("the writer's browser gets an edit cookie");
    assert_eq!(summary_row(&h, acct, "MUG").await, (1, 5, vec![0, 0, 0, 0, 1]));
    // Someone else holding the link sees that it was reviewed, not what was written.
    let (_, hd, other) = h.raw("GET", &format!("/e/review/{t}"), &[], "").await;
    assert!(!other.contains("Great secret thoughts"), "the review's words only to the browser that wrote it");
    assert_eq!(hd.get("cache-control").unwrap(), "no-store");
    let (_, _, mine) = h.raw("GET", &format!("/e/review/{t}"), &[("cookie", &cookie)], "").await;
    assert!(mine.contains("Great secret thoughts"));
    // ... and can't change it.
    let (s, _, _) = submit_as(&h, &t, json!({"rating": 1, "body": "hijacked"}), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // The writer can, within a day: a 2-star edit goes back to the queue.
    let (s, v, _) = submit_as(&h, &t, json!({"rating": 2, "body": "Broke after a week"}), Some(&cookie)).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0, "the edited review waits for moderation");
    // After a day it can't be changed any more.
    sqlx::query("UPDATE reviews SET created_at = now() - interval '2 days'").execute(&h.state.db).await.unwrap();
    let (s, _, _) = submit_as(&h, &t, json!({"rating": 5}), Some(&cookie)).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Deleted by the business: the link is still used up for good.
    let rid: Uuid = sqlx::query_scalar("SELECT id FROM reviews").fetch_one(&h.state.db).await.unwrap();
    h.ok("DELETE", &format!("/api/reviews/{rid}?confirm=true"), None).await;
    let (s, v) = submit(&h, &t, json!({"rating": 5, "body": "again"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);
    // The author's own delete erases the words; the link can't write another either.
    person(&h, acct, "c@x.test", "subscribed").await;
    let t3 = api_link(&mut h, "c@x.test", "MUG", true).await;
    submit(&h, &t3, json!({"rating": 4, "body": "Fine"})).await;
    let (s, hd, _) = h.raw("POST", &format!("/e/review/{t3}/delete"), &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("referrer-policy").unwrap(), "no-referrer");
    let (status, body): (String, String) = sqlx::query_as("SELECT status, body FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), body.as_str()), ("removed", ""));
    assert_eq!(submit(&h, &t3, json!({"rating": 5})).await.0, StatusCode::CONFLICT);
    // Expired links take nothing; forged tokens find nothing.
    person(&h, acct, "b@x.test", "subscribed").await;
    let t2 = api_link(&mut h, "b@x.test", "MUG", true).await;
    sqlx::query("UPDATE review_requests SET expires_at = now() - interval '1 minute' WHERE review_id IS NULL AND submitted_at IS NULL").execute(&h.state.db).await.unwrap();
    let (s, _) = submit(&h, &t2, json!({"rating": 5})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = submit(&h, "not-a-token", json!({"rating": 5})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Someone who reviewed it in the last year isn't asked again.
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "c@x.test", "product_id": "MUG"}))).await;
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
    let write = |email: &str| json!({"product_id": "MUG", "email": email, "rating": 5, "body": "Ok"}).to_string();
    let (s, _, _) = post(&h, &pk, write("v1@x.test"), "198.51.100.1").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "off by default");
    h.ok("PATCH", "/api/reviews/settings", Some(json!({"public_reviews": true}))).await;
    let (s, _, b) = post(&h, &pk, json!({"product_id": "NOPE", "email": "v1@x.test", "rating": 5}).to_string(), "198.51.100.1").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "only products the store has: {b}");
    let profiles = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiles").fetch_one(&db).await.unwrap() }
    };
    let before = profiles(&h).await;
    let (s, _, first) = post(&h, &pk, write("v1@x.test"), "198.51.100.1").await;
    assert_eq!(s, StatusCode::OK, "{first}");
    h.drain().await;
    assert_eq!(profiles(&h).await, before, "no profile is made before the address is confirmed");
    let mail = mails_to(&h, "v1@x.test");
    assert_eq!(mail.len(), 1, "a confirmation email, from a job");
    let link = find_link(&mail[0].html, "https://pl.test/e/review-confirm/").trim_start_matches("https://pl.test").to_string();
    assert_eq!(h.ok("GET", "/api/reviews?unconfirmed=true", None).await["total"], 1);
    let (s, hd, page) = h.raw("GET", &link, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("cache-control").unwrap(), "no-store");
    assert!(page.contains("<form method=\"post\""), "opening the link confirms nothing");
    let (s, _, _) = h.raw("POST", &link, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    h.drain().await;
    let r = &h.ok("GET", "/api/reviews?status=pending", None).await["data"][0];
    assert_eq!(r["verified"], false);
    assert_eq!(r["awaiting_confirmation"], false);
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0, "unverified reviews are always held");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'v1@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed", "a review never subscribes anyone");
    // Limit: 5 an hour from one visitor.
    for n in 2..=5 {
        let (s, _, b) = post(&h, &pk, write(&format!("v{n}@x.test")), "198.51.100.1").await;
        assert_eq!(s, StatusCode::OK, "{n}: {b}");
    }
    let (s, _, _) = post(&h, &pk, write("v6@x.test"), "198.51.100.1").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    h.cleanup().await;
}

#[tokio::test]
async fn the_public_entry_cant_be_used_to_pester_or_probe() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    h.ok("PATCH", "/api/reviews/settings", Some(json!({"public_reviews": true}))).await;
    let write = |email: &str| json!({"product_id": "MUG", "email": email, "rating": 4}).to_string();
    let mut ip = 0u8;
    let mut next_ip = || {
        ip += 1;
        Box::leak(format!("203.0.113.{ip}").into_boxed_str()) as &'static str
    };
    // The same answer whatever the address: unknown, a suppressed customer, or with sending paused.
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent, email_suppressed_reason) VALUES ($1, $2, 'gone@x.test', 'subscribed', 'hard_bounce')")
        .bind(Uuid::now_v7())
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    let a = post(&h, &pk, write("new@x.test"), next_ip()).await;
    let b = post(&h, &pk, write("gone@x.test"), next_ip()).await;
    sqlx::query("UPDATE accounts SET sending_paused_at = now() WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    let c = post(&h, &pk, write("paused@x.test"), next_ip()).await;
    assert_eq!((a.0, &a.2), (b.0, &b.2));
    assert_eq!((a.0, &a.2), (c.0, &c.2));
    h.drain().await;
    assert!(mails_to(&h, "gone@x.test").is_empty() && mails_to(&h, "paused@x.test").is_empty(), "nothing to a suppressed address or from a paused account");
    sqlx::query("UPDATE accounts SET sending_paused_at = NULL WHERE id = $1").bind(acct).execute(&h.state.db).await.unwrap();
    // One review waiting per address (dots and plus tags don't make a new one).
    let first = post(&h, &pk, write("Jane.Doe+promo@gmail.com"), next_ip()).await;
    let again = post(&h, &pk, write("janedoe@googlemail.com"), next_ip()).await;
    assert_eq!((first.0, &first.2), (again.0, &again.2), "the same answer");
    h.drain().await;
    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews WHERE email_key = 'janedoe@gmail.com' AND confirmed_at IS NULL").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(waiting, 1);
    let sent = h.state.email.sent().into_iter().filter(|m| m.to.to_lowercase().contains("doe")).count();
    assert_eq!(sent, 1, "one confirmation, not one per spelling");
    // A daily cap per address across accounts: confirming frees the address here, but the 4th
    // confirmation in a day doesn't go.
    for _ in 0..4 {
        let mail: Vec<_> = h.state.email.sent().into_iter().filter(|m| m.to == "repeat@x.test").collect();
        if let Some(m) = mail.last() {
            let link = find_link(&m.html, "https://pl.test/e/review-confirm/").trim_start_matches("https://pl.test").to_string();
            h.raw("POST", &link, &[], "").await;
        }
        let (s, _, body) = post(&h, &pk, write("repeat@x.test"), next_ip()).await;
        assert_eq!((s, body), (a.0, a.2.clone()), "the same answer every time");
        h.drain().await;
    }
    assert_eq!(mails_to(&h, "repeat@x.test").len(), 3, "at most 3 confirmations a day for one address");
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
    // Imported reviews never carry the public badge; the dashboard says what the source claimed.
    assert!(w["reviews"].as_array().unwrap().iter().all(|r| r["verified"] == false), "{w}");
    assert_eq!(list["data"].as_array().unwrap().iter().filter(|r| r["verified_by_source"] == true).count(), 1, "the source's claim is kept");
    assert!(list["data"].as_array().unwrap().iter().all(|r| r["verified"] == false));
    // Reviews gathered elsewhere are never marked up for Google.
    assert!(w["structured_data"].is_null(), "{w}");
    let (s, _, _) = h.raw("GET", &format!("/client/reviews/jsonld?k={pk}&product_id=777"), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Export: CSV with formulas made harmless, and emails for people who may read profiles.
    let (s, _, csv_out) = {
        let b = h.page_headers("GET", axum::http::Request::builder().uri("/api/reviews/export?with_people=true")).await;
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
    // Bookings are the account's own stay metric here.
    h.ok("PATCH", "/api/reviews/settings", Some(json!({"purchase_metrics": ["Reservation Confirmed"]}))).await;
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

// ---------- Fix round: one test (or more) per review finding ----------

async fn browser_event(h: &Harness, pk: &str, email: &str, metric: &str, props: Value) {
    let body = json!({"metric": metric, "profile": {"email": email}, "properties": props}).to_string();
    let (s, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Item 1 and 2: nothing a browser supplied is loaded or linked; a browser order proves nothing, and
/// a trusted order for the same line replaces its request.
#[tokio::test]
async fn browser_details_are_never_loaded_and_a_trusted_order_takes_over_the_line() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    person(&h, acct, "jane@x.test", "subscribed").await;
    live_review_flow(&mut h).await;
    browser_event(&h, &pk, "jane@x.test", "Placed Order", json!({"order_id": "77", "items": [{"product_id": "TEE", "name": "Comfy tee", "image_url": "https://evil.test/pixel.gif", "url": "https://evil.test/phish"}]})).await;
    h.drain().await;
    h.fast_forward().await;
    let ask = mails_to(&h, "jane@x.test");
    assert_eq!(ask.len(), 1);
    assert!(!ask[0].html.contains("evil.test"), "a browser's image or link never reaches an email");
    assert!(ask[0].html.contains("Comfy tee"), "its name shows as text, to the person who supplied it");
    let old = tokens(&ask[0].html)[0].clone();
    let (_, _, page) = h.raw("GET", &format!("/e/review/{old}"), &[], "").await;
    assert!(!page.contains("evil.test"));
    let (verified,): (bool,) = sqlx::query_as("SELECT verified FROM review_requests").fetch_one(&h.state.db).await.unwrap();
    assert!(!verified, "a browser-recorded order proves no purchase");
    // The store's own system records the same order (through the API, with its order reference) and
    // names the product, with no image: the name becomes public, the browser's image never does.
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "jane@x.test", "product_id": "TEE", "name": "Comfy tee", "order_id": "77"}))).await;
    let url = v["requests"][0]["url"].as_str().unwrap_or_else(|| panic!("the verified order takes the line: {v}"));
    let new_links = [url.rsplit('/').next().unwrap().to_string()];
    assert_ne!(new_links[0], old);
    let (s, _, _) = h.raw("GET", &format!("/e/review/{old}"), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the old link stops working");
    let (verified,): (bool,) = sqlx::query_as("SELECT verified FROM review_requests").fetch_one(&h.state.db).await.unwrap();
    assert!(verified);
    let (name_t, image_t, url_t, image): (bool, bool, bool, Option<String>) =
        sqlx::query_as("SELECT name_trusted, image_trusted, url_trusted, image_url FROM review_subjects WHERE external_id = 'TEE'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((name_t, image_t, url_t), (true, false, false), "a trusted upsert only vouches for the fields it supplied");
    assert_eq!(image.as_deref(), Some("https://evil.test/pixel.gif"), "kept, but never shown");
    submit(&h, &new_links[0], json!({"rating": 5, "body": "Soft"})).await;
    let all = widget(&h, &format!("/client/reviews/all?k={pk}")).await;
    let card = &all["reviews"][0]["product"];
    assert_eq!(card["name"], "Comfy tee");
    assert!(card["image_url"].is_null() && card["url"].is_null(), "{card}");
    assert!(!all.to_string().contains("evil.test"));
    let ld = widget(&h, &format!("/client/reviews/jsonld?k={pk}&product_id=TEE")).await;
    assert!(ld.get("image").is_none() && ld.get("url").is_none(), "{ld}");
    let dash = h.ok("GET", "/api/reviews", None).await;
    assert!(dash["data"][0]["product"]["image_url"].is_null(), "the dashboard doesn't load it either");
    h.cleanup().await;
}

/// Item 2: verified needs a purchase or stay a trusted source recorded for that person and item.
#[tokio::test]
async fn verified_needs_a_recorded_purchase() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    person(&h, acct, "a@x.test", "subscribed").await;
    person(&h, acct, "b@x.test", "subscribed").await;
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "a@x.test", "product_id": "MUG"}))).await;
    assert_eq!(v["requests"][0]["verified"], false, "no order reference: unverified");
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "b@x.test", "product_id": "MUG", "order_id": "A-1"}))).await;
    assert_eq!(v["requests"][0]["verified"], true);
    // A server event of a metric that isn't a purchase proves nothing until the account lists it.
    let flow = json!({"name": "Visit", "trigger": {"type": "metric", "metric": "Visited Store"}, "definition": {"entry": "a", "nodes": [
        {"id": "a", "type": "review_request", "channel": "email", "next": null, "content": {"subject": "How was it?", "blocks": [{"type": "review_request"}]}}]}});
    let f = h.ok("POST", "/api/flows", Some(flow)).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    person(&h, acct, "c@x.test", "subscribed").await;
    person(&h, acct, "d@x.test", "subscribed").await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Visited Store", "profile": {"email": "c@x.test"}, "properties": {"product_id": "MUG"}}))).await;
    h.drain().await;
    h.ok("PATCH", "/api/reviews/settings", Some(json!({"purchase_metrics": ["Visited Store"]}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Visited Store", "profile": {"email": "d@x.test"}, "properties": {"product_id": "MUG"}}))).await;
    h.drain().await;
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as("SELECT p.email, q.verified, q.verified_ref FROM review_requests q JOIN profiles p ON p.id = q.profile_id WHERE p.email IN ('c@x.test', 'd@x.test') ORDER BY p.email")
        .fetch_all(&h.state.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[0].1 && rows[0].2.is_none(), "not a purchase metric");
    assert!(rows[1].1 && rows[1].2.as_deref().is_some_and(|r| r.starts_with("event:")), "{rows:?}");
    h.cleanup().await;
}

/// Item 3: one ask, one message, even when the step runs again after the send, or a worker stops
/// between its claim and the hand-off.
#[tokio::test]
async fn one_ask_one_message_across_retries_and_crashes() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    person(&h, acct, "jane@x.test", "subscribed").await;
    person(&h, acct, "sam@x.test", "subscribed").await;
    live_review_flow(&mut h).await;
    order(&mut h, "jane@x.test", "1", json!([{"product_id": "MUG", "name": "Mug"}]), "o1").await;
    h.drain().await;
    h.fast_forward().await;
    assert_eq!(mails_to(&h, "jane@x.test").len(), 1);
    // The run is put back on the ask (as if its worker died after the send, before moving on).
    let run: Uuid = sqlx::query_scalar("SELECT r.id FROM flow_runs r JOIN profiles p ON p.id = r.profile_id WHERE p.email = 'jane@x.test'").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE flow_runs SET current_node = 'v1', status = 'active', context = context - '_waited', waiting_until = NULL, claim_id = NULL, claim_until = NULL WHERE id = $1")
        .bind(run)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM jobs WHERE kind = 'flow_step'").execute(&h.state.db).await.unwrap();
    dumbmail::jobs::enqueue(&h.state.db, "flow_step", json!({"run_id": run}), None).await.unwrap();
    h.drain().await;
    assert_eq!(mails_to(&h, "jane@x.test").len(), 1, "the step finds the message it already sent");
    let (node, waiting): (Option<String>, bool) = sqlx::query_as("SELECT current_node, waiting_until IS NOT NULL FROM flow_runs WHERE id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((node.as_deref(), waiting), (Some("v2"), true), "the ask step really ran again and moved on to the reminder's wait");
    let flow_msgs: i64 = sqlx::query_scalar("SELECT count(*) FROM messages m JOIN profiles p ON p.id = m.profile_id WHERE p.email = 'jane@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(flow_msgs, 1);
    // A worker that stops between claiming the ask and handing it over: sent once, later.
    sqlx::query("DELETE FROM jobs WHERE kind = 'flow_step'").execute(&h.state.db).await.unwrap();
    dumbmail::features::reviews::hooks::fail("review_send", acct, 0);
    order(&mut h, "sam@x.test", "2", json!([{"product_id": "MUG", "name": "Mug"}]), "o2").await;
    h.drain().await;
    h.fast_forward().await;
    assert!(mails_to(&h, "sam@x.test").is_empty(), "the worker stopped before the hand-off");
    let (status, claimed): (String, bool) = sqlx::query_as("SELECT rs.status, rs.claimed_at IS NOT NULL FROM review_sends rs JOIN profiles p ON p.id = rs.profile_id WHERE p.email = 'sam@x.test'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!((status.as_str(), claimed), ("pending", true));
    h.fast_forward().await;
    assert!(mails_to(&h, "sam@x.test").is_empty(), "nobody else takes over while the claim is fresh");
    // Both the ask's claim and the run's own (the flow engine's) lapse, as they would in time.
    sqlx::query("UPDATE review_sends SET claimed_at = now() - interval '1 hour'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE flow_runs SET claim_until = now() - interval '1 minute' WHERE claim_until IS NOT NULL").execute(&h.state.db).await.unwrap();
    // The flow engine takes the run over (as its own tests do once a failed step's claim ran out).
    let sam_run: Uuid = sqlx::query_scalar("SELECT r.id FROM flow_runs r JOIN profiles p ON p.id = r.profile_id WHERE p.email = 'sam@x.test'").fetch_one(&h.state.db).await.unwrap();
    dumbmail::flows::step(&h.state, sam_run).await.unwrap();
    h.fast_forward().await;
    assert_eq!(mails_to(&h, "sam@x.test").len(), 1, "taken over once the claim lapsed, and sent once");
    dumbmail::flows::step(&h.state, sam_run).await.unwrap();
    assert_eq!(mails_to(&h, "sam@x.test").len(), 1, "and never again");
    h.cleanup().await;
}

/// Item 3: a block or tag doesn't use up the order line when its message never went.
#[tokio::test]
async fn an_ask_that_never_went_leaves_the_order_line_free() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    person(&h, acct, "jane@x.test", "subscribed").await;
    // An ordinary flow email with a Review request block.
    let flow = json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "a", "nodes": [
        {"id": "a", "type": "email", "next": null, "content": {"subject": "Thanks", "blocks": [{"type": "review_request"}]}}]}});
    let f = h.ok("POST", "/api/flows", Some(flow)).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    order(&mut h, "jane@x.test", "55", json!([{"product_id": "MUG", "name": "Mug"}]), "o55").await;
    // The email is rendered with its ask, then stopped at the sender's last check (an opt-out
    // landing in between, say): nothing reached her.
    h.drain().await;
    let (sent, mid): (Option<chrono::DateTime<chrono::Utc>>, Option<Uuid>) = sqlx::query_as("SELECT sent_at, message_id FROM review_requests").fetch_one(&h.state.db).await.unwrap();
    assert!(sent.is_none(), "rendering a block doesn't mark the request sent");
    let mid = mid.expect("the request knows its message");
    assert_eq!(mails_to(&h, "jane@x.test").len(), 1);
    sqlx::query("UPDATE messages SET status = 'skipped', sent_at = NULL, error = 'not emailable' WHERE id = $1").bind(mid).execute(&h.state.db).await.unwrap();
    // The scheduler settles it: a message that didn't go frees its request.
    dumbmail::features::reviews::requests::settle_carried(&h.state).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM review_requests").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 0, "an ask that never went doesn't hold the order line");
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "jane@x.test", "product_id": "MUG", "order_id": "55"}))).await;
    assert!(v["requests"][0]["url"].is_string(), "the order line is free again: {v}");
    h.cleanup().await;
}

/// Item 4: an ask needs marketing consent at dispatch, with no exception for service-message flows;
/// the API's send-now path applies smart sending.
#[tokio::test]
async fn asks_are_marketing_strictly() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    h.claim_number().await;
    // Subscribed to texts but held back from marketing (an integration's guest), in a service-message flow.
    let held = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, phone_number, sms_consent, marketing_hold) VALUES ($1, $2, 'held@x.test', '+15035550111', 'subscribed', true)")
        .bind(held)
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    let flow = json!({"name": "Service", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "a", "settings": {"service_messages": true}, "nodes": [
        {"id": "a", "type": "review_request", "channel": "sms", "next": null, "content": {"body": "How was it? {% review_link %}"}}]}});
    let f = h.ok("POST", "/api/flows", Some(flow)).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    order(&mut h, "held@x.test", "9", json!([{"product_id": "MUG", "name": "Mug"}]), "o9").await;
    h.drain().await;
    assert!(h.state.sms.sent().into_iter().all(|m| m.to != "+15035550111"), "no ask to someone held back from marketing");
    let asked: i64 = sqlx::query_scalar("SELECT count(*) FROM review_requests WHERE profile_id = $1").bind(held).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(asked, 0);
    // Send now through the API: smart sending applies.
    person(&h, acct, "recent@x.test", "subscribed").await;
    sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address, status, sent_at) SELECT $1, $2, id, 'email', email, 'sent', now() FROM profiles WHERE email = 'recent@x.test'")
        .bind(Uuid::now_v7())
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "recent@x.test", "product_id": "MUG", "order_id": "R1", "send": "email", "confirm": true}))).await;
    assert_eq!(v["sent"], "smart sending", "{v}");
    assert!(mails_to(&h, "recent@x.test").is_empty());
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "recent@x.test", "product_id": "MUG", "order_id": "R1", "send": "email", "confirm": true, "smart_sending_hours": 0}))).await;
    assert_eq!(v["sent"], "sent", "{v}");
    assert_eq!(mails_to(&h, "recent@x.test").len(), 1);
    h.cleanup().await;
}

/// Item 5: a review that arrives after the reminder was claimed (between its checks and the
/// hand-off) stops it entirely.
#[tokio::test]
async fn no_reminder_after_a_review() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    person(&h, acct, "jane@x.test", "subscribed").await;
    live_review_flow(&mut h).await;
    order(&mut h, "jane@x.test", "3", json!([{"product_id": "MUG", "name": "Mug"}]), "o3").await;
    h.drain().await;
    h.fast_forward().await;
    let t = tokens(&mails_to(&h, "jane@x.test")[0].html)[0].clone();
    let pause = dumbmail::features::reviews::hooks::hold("review_send", acct);
    sqlx::query("UPDATE jobs SET run_at = now() WHERE run_at <> 'infinity'").execute(&h.state.db).await.unwrap();
    let (_, review) = tokio::join!(h.drain(), async {
        pause.arrived().await;
        let out = submit(&h, &t, json!({"rating": 4})).await;
        pause.release("review_send", acct);
        out
    });
    assert!(review.0.is_success(), "{review:?}");
    assert_eq!(mails_to(&h, "jane@x.test").len(), 1, "no reminder at all, not one without its link");
    let (status,): (String,) = sqlx::query_as("SELECT status FROM review_sends WHERE kind = 'reminder'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(status, "skipped");
    let (m_status, err): (String, Option<String>) = sqlx::query_as("SELECT m.status, m.error FROM messages m JOIN review_sends rs ON rs.id = m.id WHERE rs.kind = 'reminder'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((m_status.as_str(), err.as_deref()), ("skipped", Some("already reviewed")));
    h.cleanup().await;
}

/// Item 8: a review published while the person is being deleted can't leave the summary wrong.
#[tokio::test]
async fn deleting_a_person_keeps_ratings_right() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    let p = person(&h, acct, "a@x.test", "subscribed").await;
    let first = api_link(&mut h, "a@x.test", "MUG", true).await;
    submit(&h, &first, json!({"rating": 5})).await;
    let second = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "a@x.test", "product_id": "CUP", "order_id": "Z"}))).await["requests"][0]["url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let pause = dumbmail::features::reviews::hooks::hold("delete_person", acct);
    let db = h.state.db.clone();
    let (gone, late) = tokio::join!(dumbmail::features::reviews::store::delete_person(&db, acct, p), async {
        pause.arrived().await;
        // A new review lands while the delete holds the profile: it waits, then can't attach.
        let fut = submit(&h, &second, json!({"rating": 5}));
        let out = tokio::time::timeout(std::time::Duration::from_millis(400), fut).await;
        pause.release("delete_person", acct);
        out
    });
    assert!(gone.unwrap().is_some());
    let late = match late {
        Ok(r) => r,
        Err(_) => submit(&h, &second, json!({"rating": 5})).await,
    };
    assert!(!late.0.is_success(), "{late:?}");
    for key in ["MUG", "CUP"] {
        let published: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews r JOIN review_subjects s ON s.id = r.subject_id WHERE s.external_id = $1 AND r.status = 'published'")
            .bind(key)
            .fetch_one(&h.state.db)
            .await
            .unwrap();
        let row: Option<(i32,)> = sqlx::query_as("SELECT review_count FROM review_subjects WHERE external_id = $1").bind(key).fetch_optional(&h.state.db).await.unwrap();
        assert_eq!(row.map(|r| i64::from(r.0)).unwrap_or(0), published, "{key}");
    }
    assert_eq!(summary_row(&h, acct, "MUG").await.0, 0);
    h.cleanup().await;
}

/// Item 9: every typed cell is safe to open in a spreadsheet, the reviewer's email included.
#[tokio::test]
async fn exports_are_safe_to_open() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent) VALUES ($1, $2, '=cmd@x.test', 'subscribed')").bind(Uuid::now_v7()).bind(acct).execute(&h.state.db).await.unwrap();
    let t = api_link(&mut h, "=cmd@x.test", "MUG", true).await;
    submit(&h, &t, json!({"rating": 5, "name": "@SUM(1)", "body": "+1"})).await;
    let b = h.page_headers("GET", axum::http::Request::builder().uri("/api/reviews/export?with_people=true")).await;
    let res = tower::ServiceExt::oneshot(h.router.clone(), b.body(axum::body::Body::empty()).unwrap()).await.unwrap();
    let csv = String::from_utf8(http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(csv.contains("'=cmd@x.test") && csv.contains("'@SUM(1)") && csv.contains("'+1"), "{csv}");
    assert!(!csv.contains(",=cmd@x.test"));
    h.cleanup().await;
}

#[derive(Clone, Default)]
struct Statements(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Statements {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct F<'a>(&'a mut String);
        impl tracing::field::Visit for F<'_> {
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{value:?} "));
            }
            fn record_str(&mut self, _: &tracing::field::Field, value: &str) {
                self.0.push_str(value);
                self.0.push(' ');
            }
        }
        if event.metadata().target() == "sqlx::query" {
            let mut s = String::new();
            event.record(&mut F(&mut s));
            self.0.lock().unwrap().push(s);
        }
    }
}

/// Item 10: photo caps before decoding, keyset pages, a matching ETag answered without the list
/// query, and reads throttled per address.
#[tokio::test]
async fn public_reads_and_photos_are_bounded() {
    use tracing_subscriber::layer::SubscriberExt;
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    for i in 0..7 {
        let e = format!("p{i}@x.test");
        person(&h, acct, &e, "subscribed").await;
        let t = api_link(&mut h, &e, "MUG", true).await;
        if i == 0 {
            for _ in 0..5 {
                assert_eq!(h.bytes("POST", &format!("/e/review/{t}/photos"), "image/png", png(40, 30)).await.0, StatusCode::OK);
            }
            let (s, _, b) = h.bytes("POST", &format!("/e/review/{t}/photos"), "image/png", b"not even an image".to_vec()).await;
            assert_eq!(s, StatusCode::CONFLICT, "the cap is checked before decoding: {}", String::from_utf8_lossy(&b));
        }
        submit(&h, &t, json!({"rating": 5 - (i % 2), "body": format!("review {i}")})).await;
    }
    // Keyset pages: 5, then the remaining 2, with no repeats.
    let first = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG")).await;
    assert_eq!(first["reviews"].as_array().unwrap().len(), 5);
    let next = first["next"].as_str().unwrap().to_string();
    let second = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG&after={next}")).await;
    assert_eq!(second["reviews"].as_array().unwrap().len(), 2);
    assert!(second["next"].is_null());
    let ids: std::collections::HashSet<String> = first["reviews"].as_array().unwrap().iter().chain(second["reviews"].as_array().unwrap()).map(|r| r["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 7);
    let lowest = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG&sort=lowest")).await;
    assert_eq!(lowest["reviews"][0]["rating"], 4);
    // A matching ETag is answered from the summary's version, without the list query.
    let (_, hd, _) = h.raw("GET", &format!("/client/reviews?k={pk}&product_id=MUG"), &[], "").await;
    let tag = hd.get("etag").unwrap().to_str().unwrap().to_string();
    let seen = Statements::default();
    let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(seen.clone()));
    let (s, _, _) = h.raw("GET", &format!("/client/reviews?k={pk}&product_id=MUG"), &[("if-none-match", &tag)], "").await;
    drop(guard);
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    let sql = seen.0.lock().unwrap().clone();
    assert!(!sql.iter().any(|q| q.contains("FROM reviews r")), "no list query for a cached answer: {sql:#?}");
    assert!(!sql.iter().any(|q| q.contains("count(")), "no row counts: {sql:#?}");
    // A reply changes what's shown: the ETag changes.
    let rid = h.ok("GET", "/api/reviews?status=published&limit=1", None).await["data"][0]["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/reviews/{rid}/reply"), Some(json!({"text": "Thanks!"}))).await;
    let (s, hd, _) = h.raw("GET", &format!("/client/reviews?k={pk}&product_id=MUG"), &[("if-none-match", &tag)], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_ne!(hd.get("etag").unwrap().to_str().unwrap(), tag);
    // Reads are throttled per visitor address.
    let mut last = StatusCode::OK;
    for _ in 0..310 {
        last = h.raw("GET", &format!("/client/reviews/summary?k={pk}&ids=MUG"), &[("x-real-ip", "198.51.100.77")], "").await.0;
        if last == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(h.raw("GET", &format!("/client/reviews/summary?k={pk}&ids=MUG"), &[("x-real-ip", "198.51.100.78")], "").await.0, StatusCode::OK);
    h.cleanup().await;
}

/// Item 11: answers and names are scanned too, and moderation is on the record with the ids changed.
#[tokio::test]
async fn answers_are_flagged_and_moderation_is_audited_with_its_ids() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    h.ok("PATCH", "/api/reviews/settings", Some(json!({"questions": [{"id": "why", "label": "Why?", "kind": "text"}]}))).await;
    person(&h, acct, "a@x.test", "subscribed").await;
    person(&h, acct, "b@x.test", "subscribed").await;
    let t = api_link(&mut h, "a@x.test", "MUG", true).await;
    let (_, v) = submit(&h, &t, json!({"rating": 5, "body": "Nice", "answers": {"why": "cheaper at www.spam.test"}})).await;
    assert_eq!(v["status"], "pending", "a link in an answer holds it: {v}");
    let t2 = api_link(&mut h, "b@x.test", "MUG", true).await;
    let (_, v2) = submit(&h, &t2, json!({"rating": 5, "body": "Nice", "name": "call 503 555 0100 now"})).await;
    assert_eq!(v2["status"], "pending", "so does a phone number in the name");
    let ids = vec![v["review_id"].clone(), v2["review_id"].clone()];
    h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": ids, "action": "publish", "confirm": true}))).await;
    let details: Value = sqlx::query_scalar("SELECT details FROM audit_log WHERE account_id = $1 AND action = 'review.publish'").bind(acct).fetch_one(&h.state.db).await.unwrap();
    let mut got: Vec<String> = details["changed_ids"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    let mut want: Vec<String> = ids.iter().map(|x| x.as_str().unwrap().to_string()).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    h.cleanup().await;
}

/// Item 12: a link for a named person needs profiles read; exporting reviewers' emails needs an
/// Admin-or-Owner permission on top of export.
#[tokio::test]
async fn who_can_make_links_and_export_people() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    product(&mut h, "MUG", "Blue mug").await;
    person(&h, acct, "a@x.test", "subscribed").await;
    let content_only = h.ok("POST", "/api/api-keys", Some(json!({"name": "c", "scopes": ["content"]}))).await["key"].as_str().unwrap().to_string();
    let exporter = h.ok("POST", "/api/api-keys", Some(json!({"name": "e", "scopes": ["audience", "content:read"]}))).await["key"].as_str().unwrap().to_string();
    let call = |key: String, method: &'static str, path: &'static str, body: Option<Value>| {
        let router = h.router.clone();
        async move {
            let b = axum::http::Request::builder().method(method).uri(path).header("authorization", format!("Bearer {key}"));
            let req = match body {
                Some(v) => b.header("content-type", "application/json").body(axum::body::Body::from(v.to_string())).unwrap(),
                None => b.body(axum::body::Body::empty()).unwrap(),
            };
            let res = tower::ServiceExt::oneshot(router, req).await.unwrap();
            let s = res.status();
            (s, String::from_utf8(http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    let (s, b) = call(content_only.clone(), "POST", "/api/reviews/requests", Some(json!({"email": "a@x.test", "product_id": "MUG"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = call(exporter.clone(), "GET", "/api/reviews/export?with_people=true", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = call(exporter, "GET", "/api/reviews/export", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!b.lines().next().unwrap().contains("email"), "no reviewer details without the stronger permission");
    h.cleanup().await;
}

/// Item 13: the structured data marks up the reviews the widget shows there, without imported ones,
/// and its rating counts only reviews collected here.
#[tokio::test]
async fn structured_data_matches_the_page() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pk = site_key(&h, acct).await;
    product(&mut h, "MUG", "Blue mug").await;
    for (i, r) in [5, 3].iter().enumerate() {
        let e = format!("n{i}@x.test");
        person(&h, acct, &e, "subscribed").await;
        let t = api_link(&mut h, &e, "MUG", true).await;
        let (_, v) = submit(&h, &t, json!({"rating": r, "body": "here"})).await;
        h.ok("POST", "/api/reviews/moderate", Some(json!({"ids": [v["review_id"]], "action": "publish"}))).await;
    }
    let csv = "product_id,rating,review_content,reviewer_name\nMUG,1,elsewhere,Old\n";
    h.ok("POST", "/api/reviews/import", Some(json!({"csv": csv, "confirm": true}))).await;
    h.drain().await;
    let w = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG")).await;
    assert_eq!(w["summary"]["count"], 3, "the widget shows all three");
    let ld = &w["structured_data"];
    assert_eq!(ld["aggregateRating"]["reviewCount"], 2, "{ld}");
    assert_eq!(ld["aggregateRating"]["ratingValue"], 4.0);
    let shown: Vec<&str> = w["reviews"].as_array().unwrap().iter().map(|r| r["body"].as_str().unwrap()).collect();
    let marked: Vec<&str> = ld["review"].as_array().unwrap().iter().map(|r| r["reviewBody"].as_str().unwrap()).collect();
    assert_eq!(marked.len(), 2);
    assert!(marked.iter().all(|b| shown.contains(b) && *b != "elsewhere"));
    let same = widget(&h, &format!("/client/reviews/jsonld?k={pk}&product_id=MUG")).await;
    assert_eq!(&same, ld);
    let filtered = widget(&h, &format!("/client/reviews?k={pk}&product_id=MUG&rating=5")).await;
    assert!(filtered["structured_data"].is_null(), "only the first page with the default order carries it");
    h.cleanup().await;
}

/// Item 14: an import that stops mid-batch finishes each row together with its photos when it runs again.
#[tokio::test]
async fn imports_resume_without_losing_photos() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    dumbmail::features::webhooks::net::allow_localhost_for_tests();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let img = png(30, 20);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let img = img.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!("HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", img.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&img).await;
            });
        }
    });
    let csv = format!("product_id,rating,review_content,reviewer_name,photo_urls\nMUG,5,one,A,http://127.0.0.1:{port}/a.png\nMUG,4,two,B,http://127.0.0.1:{port}/b.png\nMUG,3,three,C,http://127.0.0.1:{port}/c.png\n");
    // The worker stops after the first row is saved.
    dumbmail::features::reviews::hooks::fail("import_row", acct, 1);
    h.ok("POST", "/api/reviews/import", Some(json!({"csv": csv, "confirm": true}))).await;
    h.drain().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1, "one row in before the stop");
    h.fast_forward().await;
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT r.body, (SELECT count(*) FROM review_photos ph WHERE ph.review_id = r.id) FROM reviews r ORDER BY r.import_row").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(rows, vec![("one".to_string(), 1), ("two".to_string(), 1), ("three".to_string(), 1)], "every row once, each with its photo");
    let (status, imported): (String, i32) = sqlx::query_as("SELECT status, imported FROM review_imports").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), imported), ("done", 3));
    h.cleanup().await;
}

/// Item 15: a text ask without the tag still carries its link.
#[tokio::test]
async fn a_text_ask_always_has_its_link() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let pid = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, phone_number, sms_consent) VALUES ($1, $2, 't@x.test', '+15035550122', 'subscribed')")
        .bind(pid)
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    let v = h.ok("POST", "/api/reviews/requests", Some(json!({"email": "t@x.test", "product_id": "MUG", "order_id": "4"}))).await;
    let rid = v["requests"][0]["request_id"].as_str().unwrap().to_string();
    let link = v["requests"][0]["url"].as_str().unwrap().to_string();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1", dumbmail::models::PROFILE_COLS);
    let profile: dumbmail::models::Profile = sqlx::query_as(&q).bind(pid).fetch_one(&h.state.db).await.unwrap();
    // What the review step hands the sender: a text with no tag, and its requests.
    let content = json!({"body": "Thanks for your order! Tell us how it went."});
    let event = json!({"_review_requests": [rid]});
    let out = dumbmail::features::reviews::personalize(&h.state, &account, Some(&profile), Some(&event), &content, false, None).await.unwrap();
    assert_eq!(out["body"], format!("Thanks for your order! Tell us how it went. {link}"));
    // A text that has the tag gets the link in its place, once.
    let tagged = json!({"body": "How was it? {% review_link %} Thanks!"});
    let out = dumbmail::features::reviews::personalize(&h.state, &account, Some(&profile), Some(&event), &tagged, false, None).await.unwrap();
    assert_eq!(out["body"], format!("How was it? {link} Thanks!"));
    h.cleanup().await;
}

/// A write through the API with the dashboard's session, usable from several tasks at once.
async fn api_post(h: &Harness, path: &str, body: Value) -> (StatusCode, Value) {
    let b = h.page_headers("POST", axum::http::Request::builder().method("POST").uri(path).header("content-type", "application/json")).await;
    let res = tower::ServiceExt::oneshot(h.router.clone(), b.body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Round 3, item 1: one intent per ask however many sends race for it, and the job that
/// dispatches it is queued with it, so an API call that stops right after recording still sends.
#[tokio::test]
async fn one_intent_per_ask_and_always_dispatched() {
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    person(&h, acct, "jane@x.test", "subscribed").await;
    let body = json!({"email": "jane@x.test", "product_id": "MUG", "order_id": "A1", "send": "email", "confirm": true, "smart_sending_hours": 0});
    let (a, b) = tokio::join!(api_post(&h, "/api/reviews/requests", body.clone()), api_post(&h, "/api/reviews/requests", body.clone()));
    assert!(a.0.is_success() && b.0.is_success(), "{a:?} {b:?}");
    let sent: Vec<&str> = [a.1["sent"].as_str().unwrap_or(""), b.1["sent"].as_str().unwrap_or("")].to_vec();
    assert_eq!(sent.iter().filter(|s| **s == "sent").count(), 1, "{a:?} {b:?}");
    let links: std::collections::HashSet<&str> = [&a.1, &b.1].iter().filter_map(|v| v["requests"][0]["url"].as_str()).collect();
    assert_eq!(links.len(), 1, "both calls hand back the same link, or the second none: {a:?} {b:?}");
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM review_sends").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(intents, 1, "one intent for the one request");
    h.fast_forward().await;
    assert_eq!(mails_to(&h, "jane@x.test").len(), 1, "one email, also after the queued job runs");
    // Asked again later: nothing new.
    let v = h.ok("POST", "/api/reviews/requests", Some(body)).await;
    assert_ne!(v["sent"], "sent", "{v}");
    assert_eq!(mails_to(&h, "jane@x.test").len(), 1);

    // The API call stops right after its intent commits, before dispatching it.
    person(&h, acct, "sam@x.test", "subscribed").await;
    dumbmail::features::reviews::hooks::fail("api_after_intent", acct, 0);
    let (s, _) = h.req("POST", "/api/reviews/requests", Some(json!({"email": "sam@x.test", "product_id": "MUG", "order_id": "A2", "send": "email", "confirm": true, "smart_sending_hours": 0}))).await;
    assert!(!s.is_success(), "the call stopped");
    assert!(mails_to(&h, "sam@x.test").is_empty());
    h.fast_forward().await;
    assert_eq!(mails_to(&h, "sam@x.test").len(), 1, "the job queued with the intent sends it");
    h.fast_forward().await;
    assert_eq!(mails_to(&h, "sam@x.test").len(), 1, "once");
    h.cleanup().await;
}

/// A phone number whose time zone is in the middle of the day now (texts' quiet hours never apply).
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

async fn texter(h: &Harness, acct: Uuid, phone: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, phone_number, sms_consent) VALUES ($1, $2, $3, 'subscribed')").bind(id).bind(acct).bind(phone).execute(&h.state.db).await.unwrap();
    id
}

fn texts_to(h: &Harness, to: &str) -> Vec<String> {
    h.state.sms.sent().into_iter().filter(|m| m.to == to).map(|m| m.body).collect()
}

/// Round 3, item 2: the strict marketing rules hold at the hand-off itself, in the sender's last
/// check: a change that lands after the reviews module's own checks (and, for texts, after the
/// message was rendered) still stops the ask, and a reminder stops once its request is answered.
#[tokio::test]
async fn strict_rules_hold_at_the_hand_off() {
    use dumbmail::features::reviews::hooks;
    use dumbmail::sending::LimiterPause;
    let mut h = harness!();
    h.signup().await;
    h.credit(10.0).await;
    let acct = account_id(&h).await;
    h.claim_number().await;

    // An email ask: the person stops being subscribed (never asked) after the module's checks.
    person(&h, acct, "jane@x.test", "subscribed").await;
    live_review_flow(&mut h).await;
    order(&mut h, "jane@x.test", "1", json!([{"product_id": "MUG", "name": "Mug"}]), "o1").await;
    h.drain().await;
    let pause = hooks::hold("review_send", acct);
    let db = h.state.db.clone();
    tokio::join!(h.fast_forward(), async {
        pause.arrived().await;
        sqlx::query("UPDATE profiles SET email_consent = 'never_subscribed' WHERE email = 'jane@x.test'").execute(&db).await.unwrap();
        pause.release("review_send", acct);
    });
    assert!(mails_to(&h, "jane@x.test").is_empty(), "an email ask needs the person subscribed at the hand-off");
    let (status, err): (String, Option<String>) = sqlx::query_as("SELECT m.status, m.error FROM messages m JOIN profiles p ON p.id = m.profile_id WHERE p.email = 'jane@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), err.as_deref()), ("skipped", Some("no marketing consent")));

    // Texts: an ask, then a reminder a day later.
    let flow = json!({"name": "Text asks", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "a", "nodes": [
        {"id": "a", "type": "review_request", "channel": "sms", "next": "d", "content": {"body": "How was it? {% review_link %}"}},
        {"id": "d", "type": "delay", "amount": 1, "unit": "days", "next": "r"},
        {"id": "r", "type": "review_request", "channel": "sms", "reminder": true, "next": null, "content": {"body": "Still keen to hear: {% review_link %}"}}]}});
    let f = h.ok("POST", "/api/flows", Some(flow)).await;
    h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;

    // A marketing hold lands after the text was rendered: held back, no exception.
    let held = daytime_number(1);
    texter(&h, acct, &held).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"phone_number": held}, "properties": {"order_id": "2", "items": [{"product_id": "MUG", "name": "Mug"}]}, "unique_id": "o2"}))).await;
    let pause = LimiterPause::hold(acct);
    let st = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&st).await.unwrap() });
    pause.arrived().await;
    sqlx::query("UPDATE profiles SET marketing_hold = true WHERE phone_number = $1").bind(&held).execute(&h.state.db).await.unwrap();
    pause.release(acct);
    worker.await.unwrap();
    assert!(texts_to(&h, &held).is_empty(), "{:?}", texts_to(&h, &held));
    let (status, err): (String, Option<String>) = sqlx::query_as("SELECT status, error FROM messages WHERE channel = 'sms' AND to_address = $1").bind(&held).fetch_one(&h.state.db).await.unwrap();
    assert_eq!((status.as_str(), err.as_deref()), ("skipped", Some("no marketing consent")));

    // A review lands after the reminder was rendered: the reminder doesn't go.
    let tom = daytime_number(2);
    texter(&h, acct, &tom).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"phone_number": tom}, "properties": {"order_id": "3", "items": [{"product_id": "MUG", "name": "Mug"}]}, "unique_id": "o3"}))).await;
    h.drain().await;
    let asks = texts_to(&h, &tom);
    assert_eq!(asks.len(), 1, "the ask went");
    let t = asks[0].split("%2Fe%2Freview%2F").nth(1).unwrap_or_else(|| panic!("{}", asks[0])).split('&').next().unwrap().to_string();
    let pause = LimiterPause::hold(acct);
    sqlx::query("UPDATE jobs SET run_at = now() WHERE run_at <> 'infinity'").execute(&h.state.db).await.unwrap();
    let st = h.state.clone();
    let worker = tokio::spawn(async move { dumbmail::jobs::drain(&st).await.unwrap() });
    pause.arrived().await;
    let (s, v) = submit(&h, &t, json!({"rating": 5})).await;
    assert!(s.is_success(), "{v}");
    pause.release(acct);
    worker.await.unwrap();
    assert_eq!(texts_to(&h, &tom).len(), 1, "no reminder after the review: {:?}", texts_to(&h, &tom));
    let (status, err): (String, Option<String>) = sqlx::query_as("SELECT m.status, m.error FROM messages m JOIN review_sends rs ON rs.id = m.id WHERE rs.kind = 'reminder' AND m.to_address = $1")
        .bind(&tom)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!((status.as_str(), err.as_deref()), ("skipped", Some("already reviewed")));
    h.cleanup().await;
}

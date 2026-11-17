//! Product catalog: products, feeds, events, product blocks, recommendations, alerts and segments.

#[macro_use]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dumbmail::features::catalog::{feeds, learn};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use common::*;

async fn event(h: &Harness, pk: &str, email: &str, metric: &str, props: Value) {
    let body = json!({"metric": metric, "profile": {"email": email}, "properties": props}).to_string();
    let (s, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn metric_count(h: &Harness, metric: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = $1")
        .bind(metric)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn events_for(h: &Harness, metric: &str) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT p.email, e.properties FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id WHERE m.name = $1 ORDER BY e.id")
        .bind(metric)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

async fn profile_id(h: &mut Harness, email: &str) -> String {
    let p = h.ok("GET", &format!("/api/profiles?q={email}"), None).await;
    p["data"][0]["id"].as_str().unwrap().to_string()
}

fn tee(inventory_small: i64) -> Value {
    json!({"external_id": "TEE", "title": "Organic tee", "url": "https://acme.test/products/tee", "image_url": "https://cdn.acme.test/tee.jpg",
           "currency": "USD", "categories": ["Shirts"], "tags": ["organic"], "brand": "Acme",
           "variants": [{"external_id": "TEE-S", "title": "Small", "sku": "T-S", "price": 30, "inventory": inventory_small},
                        {"external_id": "TEE-M", "title": "Medium", "sku": "T-M", "price": 30, "inventory": 0}]})
}

#[tokio::test]
async fn products_alerts_and_account_isolation() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();

    // Bulk upsert: good products save, a bad one is reported by id, and stock comes from variants.
    let s = h.ok("POST", "/api/catalog/products", Some(json!({"products": [
        tee(0),
        {"external_id": "MUG", "title": "Mug", "price": "12.00", "compare_at_price": 15, "url": "https://acme.test/products/mug", "categories": ["Kitchen"]},
        {"external_id": "BAD", "title": "Bad", "url": "javascript:alert(1)"}
    ]}))).await;
    assert_eq!((s["created"].as_u64(), s["failed"].as_u64()), (Some(2), Some(1)), "{s}");
    assert!(s["errors"][0].as_str().unwrap().contains("BAD"));
    let t = h.ok("GET", "/api/catalog/products/TEE", None).await;
    assert_eq!((t["in_stock"].as_bool(), t["price"].as_f64()), (Some(false), Some(30.0)), "{t}");
    assert_eq!(t["variants"].as_array().unwrap().len(), 2);
    let same = h.ok("POST", "/api/catalog/products", Some(tee(0))).await;
    assert_eq!(same["unchanged"], 1, "the same data again is skipped");
    let found = h.ok("GET", "/api/catalog/products?q=T-M", None).await;
    assert_eq!(found["total"], 1, "search finds a product by variant SKU");
    let too_many: Vec<Value> = (0..1001).map(|i| json!({"external_id": format!("p{i}"), "title": "x"})).collect();
    let (st, _) = h.req("POST", "/api/catalog/products", Some(json!({"products": too_many}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // The widget asks whether the page's product is sold out, then takes an email.
    let (st, _, body) = h.raw("GET", &format!("/client/products/stock?k={pk}&product_id=TEE&variant_id=TEE-S"), &[], "").await;
    assert_eq!(st, StatusCode::OK);
    let stock: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((stock["found"].as_bool(), stock["in_stock"].as_bool(), stock["variant"]["in_stock"].as_bool()), (Some(true), Some(false), Some(false)));
    let (_, _, js) = h.raw("GET", &format!("/client/back-in-stock.js?k={pk}"), &[], "").await;
    assert!(js.contains("/client/back-in-stock") && js.contains(&pk) && !js.contains("__KEY__"));
    let sub = |email: &str, variant: Option<&str>| json!({"email": email, "product_id": "TEE", "variant_id": variant}).to_string();
    for (email, variant) in [("first@x.test", Some("TEE-S")), ("second@x.test", None), ("third@x.test", Some("TEE-M"))] {
        let (st, _, b) = h.raw("POST", &format!("/client/back-in-stock?k={pk}"), &[("content-type", "application/json")], &sub(email, variant)).await;
        assert_eq!(st, StatusCode::OK, "{b}");
    }
    let (st, _, _) = h.raw("POST", &format!("/client/back-in-stock?k={pk}"), &[("content-type", "application/json")], &sub("first@x.test", Some("TEE-S"))).await;
    assert_eq!(st, StatusCode::OK, "asking twice is fine");
    let (st, _, _) = h.raw("POST", &format!("/client/back-in-stock?k={pk}"), &[("content-type", "application/json")], &json!({"email": "x@x.test", "product_id": "MUG"}).to_string()).await;
    assert_eq!(st, StatusCode::CONFLICT, "in stock: nothing to wait for");
    let (st, _, _) = h.raw("POST", "/client/back-in-stock?k=pk_wrong", &[("content-type", "application/json")], &sub("y@x.test", None)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(metric_count(&h, "Subscribed to Back in Stock").await, 3);
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'first@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "never_subscribed", "asking for an alert isn't consent to marketing");
    assert_eq!(h.ok("GET", "/api/catalog/products/TEE", None).await["waiting_total"], 3);

    // Small comes back: the Small request and the any-variant request are told; Medium keeps waiting.
    h.ok("POST", "/api/catalog/products", Some(tee(5))).await;
    h.drain().await;
    let told: Vec<String> = events_for(&h, "Back in Stock").await.into_iter().map(|e| e.0).collect();
    assert_eq!(told, vec!["first@x.test".to_string(), "second@x.test".to_string()]);
    let ev = &events_for(&h, "Back in Stock").await[0].1;
    assert_eq!((ev["product_id"].as_str(), ev["variant_id"].as_str(), ev["items"][0]["name"].as_str()), (Some("TEE"), Some("TEE-S"), Some("Organic tee")));
    // Sold out and back again: nobody is told twice without asking again.
    h.ok("POST", "/api/catalog/products", Some(tee(0))).await;
    h.ok("POST", "/api/catalog/products", Some(tee(2))).await;
    h.drain().await;
    assert_eq!(metric_count(&h, "Back in Stock").await, 2);
    let hist = h.ok("GET", "/api/catalog/products/TEE", None).await;
    assert!(hist["history"].as_array().unwrap().len() >= 4, "stock changes are kept: {}", hist["history"]);

    // A restock limit tells only the oldest requests.
    h.ok("PATCH", "/api/catalog/settings", Some(json!({"back_in_stock_limit": 1}))).await;
    for email in ["m1@x.test", "m2@x.test"] {
        let (st, _, _) = h.raw("POST", &format!("/client/back-in-stock?k={pk}"), &[("content-type", "application/json")], &sub(email, Some("TEE-M"))).await;
        assert_eq!(st, StatusCode::OK);
    }
    let mut medium_back = tee(2);
    medium_back["variants"][1]["inventory"] = json!(4);
    h.ok("POST", "/api/catalog/products", Some(medium_back)).await;
    h.drain().await;
    let told: Vec<String> = events_for(&h, "Back in Stock").await.into_iter().map(|e| e.0).collect();
    assert_eq!(told.len(), 3);
    assert_eq!(told[2], "third@x.test", "oldest first, capped at 1");

    // Price drops go to recent viewers who didn't buy, once per cooldown.
    for email in ["viewer@x.test", "buyer@x.test"] {
        event(&h, &pk, email, "Viewed Product", json!({"name": "Mug", "product_id": "MUG", "price": 12})).await;
    }
    event(&h, &pk, "buyer@x.test", "Placed Order", json!({"items": [{"name": "Mug", "product_id": "MUG", "price": 12, "quantity": 1}]})).await;
    learn::catch_up(&h.state).await.unwrap();
    let (st, v) = h.req("PATCH", "/api/catalog/settings", Some(json!({"price_drop_percent": null, "price_drop_amount": null}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "price drops need a threshold: {v}");
    h.ok("PATCH", "/api/catalog/settings", Some(json!({"price_drop_percent": 10}))).await;
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "MUG", "price": 11.5}))).await;
    h.drain().await;
    assert_eq!(metric_count(&h, "Price Dropped").await, 0, "a 4% drop is under the 10% threshold");
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "MUG", "price": 9}))).await;
    h.drain().await;
    let drops = events_for(&h, "Price Dropped").await;
    assert_eq!(drops.len(), 1, "{drops:?}");
    assert_eq!(drops[0].0, "viewer@x.test");
    assert_eq!((drops[0].1["old_price"].as_f64(), drops[0].1["new_price"].as_f64(), drops[0].1["new_price_text"].as_str()), (Some(11.5), Some(9.0), Some("$9.00")));
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "MUG", "price": 7}))).await;
    h.drain().await;
    assert_eq!(metric_count(&h, "Price Dropped").await, 1, "the cooldown holds the second drop");

    // Another account sees none of this.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    let them = other.ok("POST", "/api/auth/signup", Some(json!({"email": "other@b.test", "password": "another pass", "company": "Other"}))).await;
    assert_eq!(other.ok("GET", "/api/catalog/products", None).await["total"], 0);
    let (st, _) = other.req("GET", "/api/catalog/products/TEE", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let mug_id = h.ok("GET", "/api/catalog/products/MUG", None).await["id"].as_str().unwrap().to_string();
    let (st, _) = other.req("DELETE", &format!("/api/catalog/products/{mug_id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let other_pk = them["account"]["public_key"].as_str().unwrap();
    let (_, _, b) = h.raw("GET", &format!("/client/products/stock?k={other_pk}&product_id=TEE"), &[], "").await;
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap()["found"], false);
    other.ok("POST", "/api/catalog/products", Some(json!({"external_id": "MUG", "title": "Their mug", "price": 1}))).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/MUG", None).await["title"], "Mug", "same id in another account is a different product");

    // CSV import.
    let csv = "id,title,price,variant_id,variant title,inventory,material\nHAT,Wool hat,25,HAT-1,One size,3,wool\nSCARF,Scarf,40,,,,cotton\n";
    let r = h.ok("POST", "/api/catalog/products/import", Some(json!({"csv": csv}))).await;
    assert_eq!(r["created"], 2, "{r}");
    let hat = h.ok("GET", "/api/catalog/products/HAT", None).await;
    assert_eq!((hat["source"].as_str(), hat["custom"]["material"].as_str(), hat["variants"][0]["inventory"].as_i64()), (Some("csv"), Some("wool"), Some(3)));
    h.ok("DELETE", "/api/catalog/products/SCARF", None).await;
    let (st, _) = h.req("GET", "/api/catalog/products/SCARF", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn events_build_the_catalog_and_fill_emails() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();

    // An official product, then events that call it something else: events never overwrite it.
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "TEE", "title": "Organic tee", "price": 30, "url": "https://acme.test/products/tee",
        "image_url": "https://cdn.acme.test/tee.jpg", "categories": ["Shirts"]}))).await;
    let socks = json!({"name": "Wool socks", "product_id": "SOCKS", "price": 12, "url": "https://acme.test/products/socks", "image_url": "https://cdn.acme.test/socks.jpg", "categories": ["Socks"]});
    let cap = json!({"name": "Cap", "product_id": "CAP", "price": 20, "url": "https://acme.test/products/cap", "categories": ["Shirts"]});
    for who in ["ann@x.test", "bob@x.test", "cy@x.test"] {
        event(&h, &pk, who, "Viewed Product", json!({"name": "Renamed tee", "product_id": "TEE", "price": 1})).await;
        event(&h, &pk, who, "Placed Order", json!({"order_id": who, "items": [
            {"name": "Renamed tee", "product_id": "TEE", "price": 30, "quantity": 1}, socks.clone()]})).await;
    }
    event(&h, &pk, "dee@x.test", "Viewed Product", cap.clone()).await;
    event(&h, &pk, "dee@x.test", "Added to Cart", json!({"name": "Organic tee", "product_id": "TEE", "price": 30, "quantity": 2,
        "items": [{"name": "Organic tee", "product_id": "TEE", "price": 30, "quantity": 2}], "checkout_url": "https://acme.test/cart"})).await;
    event(&h, &pk, "dee@x.test", "Viewed Product", json!({"name": "No id, no url"})).await;
    learn::catch_up(&h.state).await.unwrap();
    let tee = h.ok("GET", "/api/catalog/products/TEE", None).await;
    assert_eq!((tee["title"].as_str(), tee["price"].as_f64()), (Some("Organic tee"), Some(30.0)), "events don't change API products");
    let s = h.ok("GET", "/api/catalog/products/SOCKS", None).await;
    assert_eq!((s["source"].as_str(), s["image_url"].as_str()), (Some("events"), Some("https://cdn.acme.test/socks.jpg")));
    let summary = h.ok("GET", "/api/catalog/summary", None).await;
    assert_eq!((summary["products"].as_i64(), summary["learned_from_events"].as_i64()), (Some(3), Some(2)), "{summary}");
    learn::catch_up(&h.state).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_interactions").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 3 + 6 + 2, "views, order items and the cart add, each read once");

    // Recommendations: bought together, and picks for a person that leave out what they bought.
    let r = h.ok("GET", "/api/catalog/recommendations?product_id=TEE", None).await;
    assert_eq!(r["bought_together"][0]["id"], "SOCKS", "{r}");
    let dee = profile_id(&mut h, "dee@x.test").await;
    let r = h.ok("GET", &format!("/api/catalog/recommendations?profile_id={dee}"), None).await;
    let picks: Vec<&str> = r["for_person"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
    assert!(picks.contains(&"SOCKS"), "carted the tee, so socks (bought with it): {picks:?}");
    assert_eq!(r["recently_viewed"][0]["id"], "CAP");
    let ann = profile_id(&mut h, "ann@x.test").await;
    let r = h.ok("GET", &format!("/api/catalog/recommendations?profile_id={ann}"), None).await;
    let picks: Vec<&str> = r["for_person"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
    assert!(!picks.contains(&"TEE") && !picks.contains(&"SOCKS"), "never what they bought: {picks:?}");

    // A preview as Dee: event items fall back to her latest cart; recommended, best sellers and Jinja lookups fill in.
    let content = json!({"subject": "Hi {{ catalog.products[\"TEE\"].title }}", "blocks": [
        {"type": "products", "source": "event_items", "count": 4, "columns": 2},
        {"type": "products", "source": "best_sellers", "count": 1, "columns": 1},
        {"type": "products", "source": "recently_viewed", "count": 1, "columns": 1, "button_text": "Back to it"},
        {"type": "products", "source": "handpicked", "product_ids": ["NOPE"], "fallback": "newest", "count": 1},
        {"type": "text", "html": "{% for p in recommendations[:1] %}<p>Pick: {{ p.title }}</p>{% endfor %}"}
    ]});
    let out = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": content, "profile_id": dee}))).await;
    let html = out["html"].as_str().unwrap();
    assert_eq!(out["subject"], "Hi Organic tee");
    assert!(html.contains("https://cdn.acme.test/tee.jpg"), "the cart's tee, with the catalog's image");
    assert!(html.contains(">Back to it</a>") && html.contains("https://acme.test/products/cap"));
    assert!(html.contains("Pick: Wool socks"), "{html}");
    let sms = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": "Back in stock: {{ catalog.products['SOCKS'].title }} {{ catalog.best_sellers[0].url }}"}}))).await;
    assert!(sms["body"].as_str().unwrap().contains("Wool socks https://acme.test/products/"), "{sms}");

    // Abandoned checkout from the blueprint shows the cart's products in the email.
    let flow = h.ok("POST", "/api/flows/from-blueprint", Some(json!({"key": "abandoned_checkout"}))).await;
    let fid = flow["id"].as_str().unwrap();
    // Just the first email (the blueprint's text step needs an SMS number).
    let mut def = flow["definition"].clone();
    let nodes: Vec<Value> = def["nodes"].as_array().unwrap().iter().filter(|n| n["id"] == "a0" || n["id"] == "a1").cloned().collect();
    def["nodes"] = json!(nodes);
    def["nodes"][1]["next"] = Value::Null;
    h.ok("PATCH", &format!("/api/flows/{fid}"), Some(json!({"definition": def}))).await;
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    event(&h, &pk, "eve@x.test", "Started Checkout", json!({"checkout_url": "https://acme.test/cart/1", "items": [
        {"name": "Organic tee", "product_id": "TEE", "price": 30, "quantity": 1}, {"name": "Mystery item", "price": 5, "url": "https://acme.test/m"}]})).await;
    h.drain().await;
    h.fast_forward().await;
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "eve@x.test").expect("reminder sent");
    assert!(mail.html.contains("Organic tee") && mail.html.contains("Mystery item"), "{}", mail.html);
    assert!(mail.text.contains("Organic tee $30.00"));
    let bps = h.ok("GET", "/api/flows/blueprints", None).await;
    for key in ["back_in_stock", "price_drop"] {
        assert!(bps["data"].as_array().unwrap().iter().any(|b| b["key"] == key), "{key}");
        h.ok("POST", "/api/flows/from-blueprint", Some(json!({"key": key}))).await;
    }

    // Segments on product activity.
    let seg = |c: Value| json!({"definition": {"match": "all", "conditions": [c]}});
    let v = h.ok("POST", "/api/segments/preview", Some(seg(json!({"type": "product", "action": "ordered", "product_id": "SOCKS", "within_days": 30})))).await;
    assert_eq!(v["count"], 3, "{v}");
    let v = h.ok("POST", "/api/segments/preview", Some(seg(json!({"type": "product", "action": "viewed", "category": "Shirts", "count": 2})))).await;
    assert_eq!(v["count"], 0);
    let v = h.ok("POST", "/api/segments/preview", Some(seg(json!({"type": "product", "action": "carted", "category": "Shirts"})))).await;
    assert_eq!(v["count"], 1);
    let v = h.ok("POST", "/api/segments/preview", Some(seg(json!({"type": "product", "action": "ordered", "op": "zero"})))).await;
    assert!(v["count"].as_i64().unwrap() >= 2, "dee and eve ordered nothing: {v}");
    h.cleanup().await;
}

#[derive(Default)]
struct FeedState {
    body: String,
    etag: String,
    hits: u32,
    conditional_hits: u32,
}

async fn feed_server(state: Arc<Mutex<FeedState>>) -> String {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    let app = axum::Router::new()
        .route(
            "/feed",
            axum::routing::get(|State(s): State<Arc<Mutex<FeedState>>>, headers: HeaderMap| async move {
                let mut s = s.lock().unwrap();
                s.hits += 1;
                if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(s.etag.as_str()) {
                    s.conditional_hits += 1;
                    return StatusCode::NOT_MODIFIED.into_response();
                }
                ([("etag", s.etag.clone()), ("content-type", "application/rss+xml".to_string())], s.body.clone()).into_response()
            }),
        )
        .route("/moved", axum::routing::get(|| async { axum::response::Redirect::temporary("/feed") }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    feeds::TEST_LOOPBACK_PORTS.lock().unwrap().push(port);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

fn rss(items: &[(&str, &str, &str, &str)]) -> String {
    let body: String = items
        .iter()
        .map(|(id, title, price, avail)| format!("<item><g:id>{id}</g:id><g:title>{title}</g:title><g:link>https://acme.test/{id}</g:link><g:price>{price} USD</g:price><g:availability>{avail}</g:availability><g:brand>Acme</g:brand></item>"))
        .collect();
    format!(r#"<?xml version="1.0"?><rss version="2.0" xmlns:g="http://base.google.com/ns/1.0"><channel><title>Acme</title>{body}</channel></rss>"#)
}

#[tokio::test]
async fn feeds_sync_safely_and_mcp_tools() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();

    // Private and local addresses are refused when the feed is fetched.
    let bad = h.ok("POST", "/api/catalog/feeds", Some(json!({"name": "Internal", "url": "http://127.0.0.1:9/feed.json", "format": "json"}))).await;
    h.drain().await;
    let f = h.ok("GET", &format!("/api/catalog/feeds/{}", bad["id"].as_str().unwrap()), None).await;
    assert_eq!(f["last_status"], "error");
    assert!(f["last_error"].as_str().unwrap().contains("public"), "{f}");
    let (st, _) = h.req("POST", "/api/catalog/feeds", Some(json!({"url": "ftp://x.test/feed", "format": "json"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = h.req("POST", "/api/catalog/feeds", Some(json!({"url": "https://x.test/feed", "field_map": {"password": "x"}}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    h.ok("DELETE", &format!("/api/catalog/feeds/{}", bad["id"].as_str().unwrap()), None).await;

    // A Google Shopping feed behind a redirect, synced, then fetched conditionally.
    let fs = Arc::new(Mutex::new(FeedState { body: rss(&[("A1", "Lamp", "40.00", "in_stock"), ("B2", "Rug", "100.00", "out_of_stock")]), etag: "\"v1\"".into(), ..Default::default() }));
    let base = feed_server(fs.clone()).await;
    let feed = h.ok("POST", "/api/catalog/feeds", Some(json!({"name": "Google", "url": format!("{base}/moved"), "format": "google_xml"}))).await;
    let fid = feed["id"].as_str().unwrap().to_string();
    h.drain().await;
    let list = h.ok("GET", "/api/catalog/feeds", None).await;
    let f = &list["data"][0];
    assert_eq!((f["last_status"].as_str(), f["products"].as_i64(), f["last_counts"]["created"].as_i64()), (Some("ok"), Some(2), Some(2)), "{f}");
    let rug = h.ok("GET", "/api/catalog/products/B2", None).await;
    assert_eq!((rug["in_stock"].as_bool(), rug["source"].as_str(), rug["brand"].as_str()), (Some(false), Some("feed"), Some("Acme")));
    h.ok("POST", &format!("/api/catalog/feeds/{fid}/sync"), None).await;
    h.drain().await;
    // A scheduled run sends the saved ETag; nothing changed, so nothing is re-read.
    sqlx::query("UPDATE catalog_feeds SET next_sync_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    feeds::queue_due(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(fs.lock().unwrap().conditional_hits, 1);
    assert_eq!(h.ok("GET", &format!("/api/catalog/feeds/{fid}"), None).await["last_status"], "not_modified");

    // Someone waits for the rug; the feed brings it back, drops the lamp's price and stops listing nothing else.
    let (st, _, _) = h.raw("POST", &format!("/client/back-in-stock?k={pk}"), &[("content-type", "application/json")], &json!({"email": "w@x.test", "product_id": "B2"}).to_string()).await;
    assert_eq!(st, StatusCode::OK);
    {
        let mut s = fs.lock().unwrap();
        s.body = rss(&[("B2", "Rug", "100.00", "in stock")]);
        s.etag = "\"v2\"".into();
    }
    sqlx::query("UPDATE catalog_feeds SET next_sync_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    feeds::queue_due(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(metric_count(&h, "Back in Stock").await, 1);
    let lamp = h.ok("GET", "/api/catalog/products/A1", None).await;
    assert_eq!(lamp["published"], false, "products that leave the feed are unpublished");
    // With a feed, site events no longer add products (but still count activity).
    event(&h, &pk, "v@x.test", "Viewed Product", json!({"name": "Unknown", "product_id": "ZZZ"})).await;
    event(&h, &pk, "v@x.test", "Viewed Product", json!({"name": "Rug", "product_id": "B2"})).await;
    learn::catch_up(&h.state).await.unwrap();
    let (st, _) = h.req("GET", "/api/catalog/products/ZZZ", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(h.ok("GET", "/api/catalog/products/B2", None).await["activity_30d"]["viewed"], 1);

    // MCP: the same catalog, and deleting needs confirm.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "Claude"}))).await["key"].as_str().unwrap().to_string();
    let mcp_router = h.router.clone();
    let call = |name: &str, args: Value| {
        let router = mcp_router.clone();
        let auth = format!("Bearer {key}");
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
        async move {
            let req = Request::builder().method("POST").uri("/mcp").header("authorization", auth).header("content-type", "application/json").body(Body::from(body)).unwrap();
            let res = router.oneshot(req).await.unwrap();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()["result"].clone()
        }
    };
    let up = call("upsert_products", json!({"products": [{"external_id": "VASE", "title": "Vase", "price": 18, "tags": ["home"]}]})).await;
    assert_eq!(up["structuredContent"]["created"], 1, "{up}");
    let found = call("list_products", json!({"tag": "home"})).await;
    assert_eq!(found["structuredContent"]["total"], 1);
    let dry = call("delete_product", json!({"product_id": "VASE"})).await;
    assert_eq!(dry["structuredContent"]["dry_run"], true);
    assert_eq!(h.ok("GET", "/api/catalog/products/VASE", None).await["title"], "Vase");
    call("delete_product", json!({"product_id": "VASE", "confirm": true})).await;
    let (st, _) = h.req("GET", "/api/catalog/products/VASE", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let feeds_list = call("list_product_feeds", json!({})).await;
    assert_eq!(feeds_list["structuredContent"]["data"].as_array().unwrap().len(), 1);
    let dry = call("delete_product_feed", json!({"id": fid})).await;
    assert_eq!(dry["structuredContent"]["dry_run"], true);
    let settings = call("update_catalog_settings", json!({"price_drop_percent": 25, "widget": {"button_text": "Tell me"}})).await;
    assert_eq!(settings["structuredContent"]["price_drop_percent"], 25.0);
    assert!(settings["structuredContent"]["widget_snippet"].as_str().unwrap().contains(&pk));
    let bad = call("update_catalog_settings", json!({"widget": {"color": "red; background:url(x)"}})).await;
    assert_eq!(bad["isError"], true);
    let recs = call("get_recommendations", json!({"product_id": "B2"})).await;
    assert_eq!(recs["structuredContent"]["product"]["id"], "B2");
    h.cleanup().await;
}

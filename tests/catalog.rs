//! Product catalog: products, feeds, events, product blocks, recommendations, alerts and segments.

#[macro_use]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dumbmail::features::catalog::{alerts, feeds, learn};
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

/// An order recorded with an API key-backed session (trusted), not from a browser.
async fn api_event(h: &mut Harness, email: &str, metric: &str, props: Value) {
    h.ok("POST", "/api/events", Some(json!({"metric": metric, "profile": {"email": email}, "properties": props}))).await;
}

/// Ask to be told when a product is back, as the widget does.
async fn ask(h: &Harness, pk: &str, body: Value, ip: Option<&str>) -> (StatusCode, Value) {
    let mut headers = vec![("content-type", "application/json")];
    if let Some(ip) = ip {
        headers.push(("x-real-ip", ip));
    }
    let (st, _, b) = h.raw("POST", &format!("/client/back-in-stock?k={pk}"), &headers, &body.to_string()).await;
    (st, serde_json::from_str(&b).unwrap_or(json!({"_raw": b})))
}

/// Tap every back-in-stock confirmation link emailed to `email` (opening the page, then the button).
async fn confirm(h: &Harness, email: &str) -> usize {
    let links: Vec<String> = h
        .state
        .email
        .sent()
        .into_iter()
        .filter(|m| m.to == email && m.html.contains("/client/back-in-stock/confirm/"))
        .map(|m| find_link(&m.html, "/client/back-in-stock/confirm/"))
        .collect();
    for l in &links {
        let (st, _, page) = h.raw("GET", l, &[], "").await;
        assert_eq!(st, StatusCode::OK);
        assert!(page.contains("<form method=\"post\">") || page.contains("all set") || page.contains("back in stock"), "{page}");
        let (st, _, _) = h.raw("POST", l, &[], "").await;
        assert_eq!(st, StatusCode::OK);
    }
    links.len()
}

async fn subscribe_to_email(h: &Harness, email: &str) {
    sqlx::query("UPDATE profiles SET email_consent = 'subscribed' WHERE email = $1").bind(email).execute(&h.state.db).await.unwrap();
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
    let sub = |email: &str, variant: Option<&str>| json!({"email": email, "product_id": "TEE", "variant_id": variant});
    for (email, variant) in [("first@x.test", Some("TEE-S")), ("second@x.test", None), ("third@x.test", Some("TEE-M"))] {
        let (st, b) = ask(&h, &pk, sub(email, variant), None).await;
        assert_eq!((st, b["confirm"].as_bool()), (StatusCode::OK, Some(true)), "{b}");
    }
    let (st, _) = ask(&h, &pk, sub("first@x.test", Some("TEE-S")), None).await;
    assert_eq!(st, StatusCode::OK, "asking twice is fine");
    let (st, _) = ask(&h, &pk, json!({"email": "x@x.test", "product_id": "MUG"}), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "in stock: nothing to wait for");
    let (st, _, _) = h.raw("POST", "/client/back-in-stock?k=pk_wrong", &[("content-type", "application/json")], &sub("y@x.test", None).to_string()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // Nothing counts until each person confirms by email.
    assert_eq!(metric_count(&h, "Subscribed to Back in Stock").await, 0);
    assert_eq!(h.ok("GET", "/api/catalog/products/TEE", None).await["unconfirmed_total"], 3);
    for email in ["first@x.test", "second@x.test", "third@x.test"] {
        assert!(confirm(&h, email).await >= 1, "{email} got a confirmation email");
    }
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
        let (st, _) = ask(&h, &pk, sub(email, Some("TEE-M")), None).await;
        assert_eq!(st, StatusCode::OK);
        confirm(&h, email).await;
    }
    let mut medium_back = tee(2);
    medium_back["variants"][1]["inventory"] = json!(4);
    h.ok("POST", "/api/catalog/products", Some(medium_back)).await;
    h.drain().await;
    let told: Vec<String> = events_for(&h, "Back in Stock").await.into_iter().map(|e| e.0).collect();
    assert_eq!(told.len(), 3);
    assert_eq!(told[2], "third@x.test", "oldest first, capped at 1");

    // Price drops go to recent viewers who didn't buy and are subscribed to email, once per cooldown.
    for email in ["viewer@x.test", "buyer@x.test", "browser@x.test"] {
        event(&h, &pk, email, "Viewed Product", json!({"name": "Mug", "product_id": "MUG", "price": 12})).await;
    }
    event(&h, &pk, "buyer@x.test", "Placed Order", json!({"items": [{"name": "Mug", "product_id": "MUG", "price": 12, "quantity": 1}]})).await;
    learn::catch_up(&h.state).await.unwrap();
    // browser@x.test never opted in to marketing: a price drop is marketing, so they aren't told.
    subscribe_to_email(&h, "viewer@x.test").await;
    subscribe_to_email(&h, "buyer@x.test").await;
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

    // One visitor can only ask so many times an hour, counted in the database.
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "GONE", "title": "Gone", "inventory": 0}))).await;
    let mut codes = Vec::new();
    for i in 0..31 {
        let (st, _) = ask(&h, &pk, json!({"email": format!("flood{i}@x.test"), "product_id": "GONE"}), Some("203.0.113.9")).await;
        codes.push(st);
    }
    assert!(codes[..30].iter().all(|c| *c == StatusCode::OK), "{codes:?}");
    assert_eq!(codes[30], StatusCode::CONFLICT, "the 31st request from one address is refused");
    let from_ip: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions WHERE ip_hash IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(from_ip, 30);

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

/// A stand-in store website: `/products/<handle>` serves the page set for it (404 otherwise).
async fn site_server(pages: Arc<Mutex<std::collections::HashMap<String, String>>>) -> String {
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    let app = axum::Router::new()
        .route(
            "/products/{handle}",
            axum::routing::get(|State(p): State<Arc<Mutex<std::collections::HashMap<String, String>>>>, Path(handle): Path<String>| async move {
                match p.lock().unwrap().get(&handle).cloned() {
                    Some(html) => ([("content-type", "text/html")], html).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .with_state(pages);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    feeds::TEST_LOOPBACK_PORTS.lock().unwrap().push(port);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

fn product_page(name: &str, price: &str, image: &str) -> String {
    format!(
        r#"<html><head><script type="application/ld+json">{{"@context":"https://schema.org","@type":"Product","name":"{name}","image":"{image}",
        "offers":{{"@type":"Offer","price":"{price}","priceCurrency":"USD","availability":"https://schema.org/InStock"}}}}</script></head><body>{name}</body></html>"#
    )
}

/// Run the verification the scheduler hands out, until nothing is due.
async fn verify_all(h: &Harness) {
    for _ in 0..5 {
        dumbmail::features::catalog::verify::queue_due(&h.state.db).await.unwrap();
        h.drain().await;
    }
}

#[tokio::test]
async fn events_build_the_catalog_and_fill_emails() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let pages = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let site = site_server(pages.clone()).await;
    sqlx::query("UPDATE accounts SET website = $1").bind(&site).execute(&h.state.db).await.unwrap();

    // An official product, then events that call it something else: events never overwrite it.
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "TEE", "title": "Organic tee", "price": 30, "url": format!("{site}/products/tee"),
        "image_url": "https://cdn.acme.test/tee.jpg", "categories": ["Shirts"]}))).await;
    let socks = json!({"name": "Socks (from the browser)", "product_id": "SOCKS", "price": 1, "url": format!("{site}/products/socks"), "image_url": "https://evil.example/x.jpg", "categories": ["Socks"]});
    let cap = json!({"name": "Cap", "product_id": "CAP", "price": 20, "url": format!("{site}/products/cap"), "categories": ["Shirts"]});
    for who in ["ann@x.test", "bob@x.test", "cy@x.test"] {
        event(&h, &pk, who, "Viewed Product", json!({"name": "Renamed tee", "product_id": "TEE", "price": 1})).await;
        // Orders the store records (the API, an integration) count as sales.
        api_event(&mut h, who, "Placed Order", json!({"order_id": who, "items": [
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
    assert_eq!((s["source"].as_str(), s["verified"].as_bool()), (Some("events"), Some(false)));
    let summary = h.ok("GET", "/api/catalog/summary", None).await;
    assert_eq!((summary["products"].as_i64(), summary["learned_from_events"].as_i64(), summary["unverified"].as_i64()), (Some(3), Some(2), Some(2)), "{summary}");
    learn::catch_up(&h.state).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_interactions").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 3 + 6 + 2, "views, order items and the cart add, each read once");

    // The store's pages confirm the learned products; what the page says replaces what the browser said.
    pages.lock().unwrap().insert("socks".into(), product_page("Wool socks", "12.00", "https://cdn.acme.test/socks.jpg"));
    pages.lock().unwrap().insert("cap".into(), product_page("Cap", "20.00", "https://cdn.acme.test/cap.jpg"));
    verify_all(&h).await;
    let s = h.ok("GET", "/api/catalog/products/SOCKS", None).await;
    assert_eq!((s["verified"].as_bool(), s["title"].as_str(), s["price"].as_f64(), s["image_url"].as_str()), (Some(true), Some("Wool socks"), Some(12.0), Some("https://cdn.acme.test/socks.jpg")), "{s}");

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
    let content = json!({"subject": "Hi {{ catalog.products[\"TEE\"].title }} / {{ organization.catalog.products[\"TEE\"].title }}", "blocks": [
        {"type": "products", "source": "event_items", "count": 4, "columns": 2},
        {"type": "products", "source": "best_sellers", "count": 1, "columns": 1},
        {"type": "products", "source": "recently_viewed", "count": 1, "columns": 1, "button_text": "Back to it"},
        {"type": "products", "source": "handpicked", "product_ids": ["NOPE"], "fallback": "newest", "count": 1},
        {"type": "text", "html": "{% for p in recommendations[:1] %}<p>Pick: {{ p.title }}</p>{% endfor %}{% for p in person.recommendations[:1] %}<p>Again: {{ p.title }}</p>{% endfor %}"}
    ]});
    let out = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": content, "profile_id": dee}))).await;
    let html = out["html"].as_str().unwrap();
    assert_eq!(out["subject"], "Hi Organic tee / Organic tee");
    assert!(html.contains("https://cdn.acme.test/tee.jpg"), "the cart's tee, with the catalog's image");
    assert!(html.contains(">Back to it</a>") && html.contains(&format!("{site}/products/cap")));
    assert!(html.contains("Pick: Wool socks") && html.contains("Again: Wool socks"), "{html}");
    let sms = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": {"body": "Back in stock: {{ catalog.products['SOCKS'].title }} {{ catalog.best_sellers[0].url }}"}}))).await;
    assert!(sms["body"].as_str().unwrap().contains(&format!("Wool socks {site}/products/")), "{sms}");

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
        {"name": "Organic tee", "product_id": "TEE", "price": 30, "quantity": 1}, {"name": "Mystery item", "price": 5, "url": format!("{site}/m")}]})).await;
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
    content_type: String,
    hits: u32,
    conditional_hits: u32,
    /// Requests to /moved that carried a validator (they belong to /feed, not /moved).
    moved_conditional: u32,
    delay_ms: u64,
}

async fn feed_server(state: Arc<Mutex<FeedState>>) -> String {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    let app = axum::Router::new()
        .route(
            "/feed",
            axum::routing::get(|State(st): State<Arc<Mutex<FeedState>>>, headers: HeaderMap| async move {
                let delay = {
                    let mut s = st.lock().unwrap();
                    s.hits += 1;
                    s.delay_ms
                };
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                let mut s = st.lock().unwrap();
                if !s.etag.is_empty() && headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(s.etag.as_str()) {
                    s.conditional_hits += 1;
                    return StatusCode::NOT_MODIFIED.into_response();
                }
                let ct = if s.content_type.is_empty() { "application/rss+xml".to_string() } else { s.content_type.clone() };
                let mut res = ([("content-type", ct)], s.body.clone()).into_response();
                if !s.etag.is_empty() {
                    res.headers_mut().insert("etag", s.etag.parse().unwrap());
                }
                res
            }),
        )
        .route(
            "/moved",
            axum::routing::get(|State(s): State<Arc<Mutex<FeedState>>>, headers: HeaderMap| async move {
                if headers.contains_key("if-none-match") || headers.contains_key("if-modified-since") {
                    s.lock().unwrap().moved_conditional += 1;
                }
                axum::response::Redirect::temporary("/feed")
            }),
        )
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
    assert_eq!(fs.lock().unwrap().moved_conditional, 0, "the ETag goes only to the address it came from");
    assert_eq!(h.ok("GET", &format!("/api/catalog/feeds/{fid}"), None).await["last_status"], "not_modified");

    // Someone waits for the rug; the feed brings it back, drops the lamp's price and stops listing nothing else.
    let (st, _) = ask(&h, &pk, json!({"email": "w@x.test", "product_id": "B2"}), None).await;
    assert_eq!(st, StatusCode::OK);
    confirm(&h, "w@x.test").await;
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

async fn feed_row(h: &Harness, fid: &str) -> (Option<String>, i32, Option<String>) {
    sqlx::query_as("SELECT etag, config_version, last_status FROM catalog_feeds WHERE id = $1::uuid").bind(fid).fetch_one(&h.state.db).await.unwrap()
}

async fn jobs_of(h: &Harness, kind: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1 AND run_at <> 'infinity'").bind(kind).fetch_one(&h.state.db).await.unwrap()
}

fn json_feed(items: &[(&str, &str, f64)]) -> String {
    json!({"products": items.iter().map(|(id, title, price)| json!({"id": id, "title": title, "price": price, "url": format!("https://acme.test/{id}")})).collect::<Vec<_>>()}).to_string()
}

/// Finding 4: one sync per feed at a time, and a sync whose feed changed meanwhile saves nothing.
#[tokio::test]
async fn feed_syncs_run_one_at_a_time_and_drop_stale_results() {
    let mut h = harness!();
    h.signup().await;
    let old = Arc::new(Mutex::new(FeedState { body: json_feed(&[("P", "Lamp", 50.0)]), etag: "\"v1\"".into(), content_type: "application/json".into(), ..Default::default() }));
    let old_base = feed_server(old.clone()).await;
    let feed = h.ok("POST", "/api/catalog/feeds", Some(json!({"url": format!("{old_base}/feed"), "format": "json"}))).await;
    let fid = feed["id"].as_str().unwrap().to_string();
    h.drain().await;
    assert_eq!(h.ok("GET", "/api/catalog/products/P", None).await["price"], 50.0);
    sqlx::query("DELETE FROM jobs").execute(&h.state.db).await.unwrap();

    // Two syncs of the same feed at once: one fetches, the other waits its turn.
    old.lock().unwrap().delay_ms = 300;
    let hits = old.lock().unwrap().hits;
    let job = json!({"feed_id": fid});
    let (a, b) = tokio::join!(feeds::run_sync(&h.state, &job), feeds::run_sync(&h.state, &job));
    a.unwrap();
    b.unwrap();
    assert_eq!(old.lock().unwrap().hits, hits + 1, "only one of them fetched");
    let later: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'catalog_feed_sync' AND run_at > now()").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(later, 1, "the other is queued to run after it");
    sqlx::query("DELETE FROM jobs").execute(&h.state.db).await.unwrap();

    // A slow sync of the old address finishes after the feed moved to a new one: its (cheaper,
    // stale) prices and ETag are not saved, and no price drop comes of them.
    {
        let mut s = old.lock().unwrap();
        s.body = json_feed(&[("P", "Lamp", 10.0)]);
        s.etag = "\"stale\"".into();
        s.delay_ms = 400;
    }
    let new = Arc::new(Mutex::new(FeedState { body: json_feed(&[("P", "Lamp", 45.0)]), etag: "\"n1\"".into(), content_type: "application/json".into(), ..Default::default() }));
    let new_base = feed_server(new.clone()).await;
    let st = h.state.clone();
    let slow = tokio::spawn(async move { feeds::run_sync(&st, &json!({"feed_id": fid.clone()})).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let fid = feed["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/catalog/feeds/{fid}"), Some(json!({"url": format!("{new_base}/feed")}))).await;
    slow.await.unwrap().unwrap();
    assert_eq!(h.ok("GET", "/api/catalog/products/P", None).await["price"], 50.0, "the stale read wasn't applied");
    assert_ne!(feed_row(&h, &fid).await.0.as_deref(), Some("\"stale\""));
    assert_eq!(jobs_of(&h, "catalog_price_drop").await, 0, "no price drop from stale data");
    h.drain().await;
    assert_eq!(h.ok("GET", "/api/catalog/products/P", None).await["price"], 45.0, "the new address's sync ran");
    assert_eq!(feed_row(&h, &fid).await.0.as_deref(), Some("\"n1\""));

    // A turned-off feed isn't fetched, even by a job already queued.
    h.ok("PATCH", &format!("/api/catalog/feeds/{fid}"), Some(json!({"enabled": false}))).await;
    let hits = new.lock().unwrap().hits;
    feeds::run_sync(&h.state, &json!({"feed_id": fid})).await.unwrap();
    assert_eq!(new.lock().unwrap().hits, hits);
    h.cleanup().await;
}

/// Findings 8 and 11: products leave a feed only after a complete read and come back when it lists
/// them again; a 200 without validators clears the saved ones.
#[tokio::test]
async fn feeds_unpublish_only_after_a_clean_read() {
    let mut h = harness!();
    h.signup().await;
    let (st, _) = h.req("POST", "/api/catalog/feeds", Some(json!({"url": "https://user:pw@shop.test/feed.json"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no credentials in feed addresses");
    let fs = Arc::new(Mutex::new(FeedState { body: json_feed(&[("A", "Lamp", 10.0), ("B", "Rug", 20.0)]), etag: "\"v1\"".into(), content_type: "application/json".into(), ..Default::default() }));
    let base = feed_server(fs.clone()).await;
    let feed = h.ok("POST", "/api/catalog/feeds", Some(json!({"url": format!("{base}/feed"), "format": "json"}))).await;
    let fid = feed["id"].as_str().unwrap().to_string();
    h.drain().await;
    let sync = |h: &Harness| {
        let db = h.state.db.clone();
        let fid = fid.clone();
        async move {
            sqlx::query("UPDATE catalog_feeds SET sync_full = false").execute(&db).await.unwrap();
            feeds::queue_sync(&db, fid.parse().unwrap(), true).await.unwrap();
        }
    };

    // B is missing, but one item couldn't be read: nothing is unpublished.
    fs.lock().unwrap().body = json!({"products": [{"id": "A", "title": "Lamp", "price": 10}, {"title": "no id"}]}).to_string();
    fs.lock().unwrap().etag = "\"v2\"".into();
    sync(&h).await;
    h.drain().await;
    assert_eq!(h.ok("GET", "/api/catalog/products/B", None).await["published"], true, "a partial read hides nothing");
    let f = h.ok("GET", &format!("/api/catalog/feeds/{fid}"), None).await;
    assert!(f["last_error"].as_str().unwrap().contains("left as they were"), "{f}");

    // A clean read without B unpublishes it; when B comes back unchanged, it's published again.
    fs.lock().unwrap().body = json_feed(&[("A", "Lamp", 10.0)]);
    sync(&h).await;
    h.drain().await;
    let b = h.ok("GET", "/api/catalog/products/B", None).await;
    assert_eq!((b["published"].as_bool(), b["missing_since"].is_string()), (Some(false), true));
    fs.lock().unwrap().body = json_feed(&[("A", "Lamp", 10.0), ("B", "Rug", 20.0)]);
    sync(&h).await;
    h.drain().await;
    let b = h.ok("GET", "/api/catalog/products/B", None).await;
    assert_eq!((b["published"].as_bool(), b["missing_since"].is_null()), (Some(true), true), "{b}");
    // The store's own "unpublished" stays, whatever the feed's presence.
    fs.lock().unwrap().body = json!({"products": [{"id": "A", "title": "Lamp", "price": 10}, {"id": "B", "title": "Rug", "price": 20, "published": false}]}).to_string();
    sync(&h).await;
    h.drain().await;
    assert_eq!(h.ok("GET", "/api/catalog/products/B", None).await["published"], false);

    // A full response without an ETag clears the saved one.
    assert!(feed_row(&h, &fid).await.0.is_some());
    fs.lock().unwrap().etag = String::new();
    fs.lock().unwrap().body = json_feed(&[("A", "Lamp", 11.0)]);
    sqlx::query("UPDATE catalog_feeds SET next_sync_at = now() - interval '1 minute'").execute(&h.state.db).await.unwrap();
    feeds::queue_due(&h.state.db).await.unwrap();
    h.drain().await;
    assert_eq!(feed_row(&h, &fid).await.0, None);
    h.cleanup().await;
}

/// Finding 1: alert, message and consent events can't be sent from a browser; alert events can't be
/// sent through the API either; and a browser-marked one never starts a flow.
#[tokio::test]
async fn forged_alert_events_are_refused() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    for key in ["back_in_stock", "price_drop"] {
        let f = h.ok("POST", "/api/flows/from-blueprint", Some(json!({"key": key}))).await;
        h.ok("POST", &format!("/api/flows/{}/status", f["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    }
    h.ok("POST", "/api/profiles", Some(json!({"email": "victim@x.test", "email_consent": "subscribed"}))).await;
    let forged = json!({"name": "FREE phone", "product_id": "X", "url": "https://evil.example/", "items": [{"name": "FREE phone", "url": "https://evil.example/"}]});
    for metric in ["Back in Stock", "Price Dropped", "Subscribed to Back in Stock", " back in stock ", "Subscribed to List", "Opened Email", "Unsubscribed"] {
        let body = json!({"metric": metric, "profile": {"email": "victim@x.test"}, "properties": forged, "unique_id": "bis:0190"}).to_string();
        let (st, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{metric:?} from a browser: {b}");
    }
    let (st, v) = h.req("POST", "/api/events", Some(json!({"metric": "Price Dropped", "profile": {"email": "victim@x.test"}, "properties": forged}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "only a real price change makes a Price Dropped event: {v}");
    h.ok("POST", "/api/events", Some(json!({"metric": "Opened Email", "profile": {"email": "victim@x.test"}}))).await;
    event(&h, &pk, "victim@x.test", "Viewed Product", json!({"name": "Mug", "product_id": "MUG"})).await;
    h.drain().await;
    h.fast_forward().await;
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(runs, 0);
    assert!(h.state.email.sent().iter().all(|m| m.to != "victim@x.test"));
    let from_browser: Vec<Option<bool>> = sqlx::query_scalar("SELECT e.from_browser FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name IN ('Viewed Product', 'Opened Email') ORDER BY m.name").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(from_browser, vec![Some(false), Some(true)], "API events are trusted, browser ones marked");

    // Even if one got in, an alert event marked as from a browser starts no flow.
    let (account, profile): (uuid::Uuid, uuid::Uuid) = sqlx::query_as("SELECT account_id, id FROM profiles WHERE email = 'victim@x.test'").fetch_one(&h.state.db).await.unwrap();
    let metric = dumbmail::audience::metric_id(&h.state.db, account, "Back in Stock").await.unwrap();
    let id = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO events (id, account_id, profile_id, metric_id, properties, occurred_at, from_browser) VALUES ($1, $2, $3, $4, $5, now(), true)")
        .bind(id).bind(account).bind(profile).bind(metric).bind(&forged).execute(&h.state.db).await.unwrap();
    dumbmail::flows::on_event(&h.state, id).await.unwrap();
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(runs, 0);
    h.cleanup().await;
}

/// Finding 3: what a browser says about a product never reaches anyone else's email, and browser
/// orders don't make best sellers.
#[tokio::test]
async fn unverified_products_only_reach_their_own_viewer() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let pages = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let site = site_server(pages.clone()).await;
    sqlx::query("UPDATE accounts SET website = $1").bind(&site).execute(&h.state.db).await.unwrap();
    // Two real products: browsers "buy" B a lot, the store's own records show one sale of C.
    h.ok("POST", "/api/catalog/products", Some(json!({"products": [
        {"external_id": "B", "title": "Bowl", "price": 20, "categories": ["Kitchen"], "url": format!("{site}/products/b")},
        {"external_id": "C", "title": "Cup", "price": 8, "categories": ["Kitchen"], "url": format!("{site}/products/c"), "published_at": "2020-01-01T00:00:00Z"}]}))).await;
    for who in ["x1@evil.test", "x2@evil.test", "x3@evil.test"] {
        event(&h, &pk, who, "Placed Order", json!({"items": [{"name": "Bowl", "product_id": "B", "price": 20}]})).await;
    }
    api_event(&mut h, "real@x.test", "Placed Order", json!({"items": [{"name": "Cup", "product_id": "C", "price": 8}]})).await;
    // The victim views a product the catalog doesn't have yet; an attacker then describes the same
    // product (and a made-up one) with their own words and pictures.
    event(&h, &pk, "victim@x.test", "Viewed Product", json!({"name": "Linen apron", "product_id": "APRON", "price": 30, "url": format!("{site}/products/apron"), "categories": ["Kitchen"]})).await;
    let evil = |id: &str, name: &str| json!({"name": name, "product_id": id, "price": 0.01, "url": format!("{site}/products/{}", id.to_lowercase()), "image_url": "https://evil.example/x.jpg", "categories": ["Kitchen"]});
    for who in ["x1@evil.test", "x2@evil.test", "x3@evil.test"] {
        event(&h, &pk, who, "Viewed Product", evil("APRON", "Scam apron")).await;
        event(&h, &pk, who, "Viewed Product", evil("FREEBIE", "FREE phone")).await;
        event(&h, &pk, who, "Placed Order", json!({"items": [evil("FREEBIE", "FREE phone")]})).await;
    }
    learn::catch_up(&h.state).await.unwrap();
    verify_all(&h).await; // the pages don't exist: nothing gets verified
    let summary = h.ok("GET", "/api/catalog/summary", None).await;
    assert_eq!(summary["unverified"], 2, "{summary}");
    let apron = h.ok("GET", "/api/catalog/products/APRON", None).await;
    assert!(apron["verify_error"].is_string(), "{apron}");

    let content = json!({"subject": "{% for p in catalog.best_sellers %}{{ p.title }},{% endfor %}", "blocks": [
        {"type": "products", "source": "best_sellers", "count": 6},
        {"type": "products", "source": "newest", "count": 6},
        {"type": "products", "source": "on_sale", "count": 6, "fallback": "newest"},
        {"type": "products", "source": "collection", "category": "Kitchen", "count": 6},
        {"type": "products", "source": "handpicked", "product_ids": ["FREEBIE", "APRON"], "count": 6},
        {"type": "products", "source": "recommended", "count": 6},
        {"type": "products", "source": "recently_viewed", "count": 6},
        {"type": "text", "html": "{% for p in recommendations %}{{ p.title }};{% endfor %} {{ catalog.newest[0].title }}"}
    ]});
    let victim = profile_id(&mut h, "victim@x.test").await;
    let out = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": content, "profile_id": victim}))).await;
    let html = out["html"].as_str().unwrap();
    for bad in ["FREE phone", "Scam apron", "evil.example", "0.01"] {
        assert!(!html.contains(bad) && !out["subject"].as_str().unwrap().contains(bad), "{bad} reached someone else: {html}");
    }
    assert!(html.contains("Linen apron"), "the victim's own view, as they saw it");
    assert!(out["subject"].as_str().unwrap().starts_with("Cup,Bowl"), "only the store's own orders make best sellers: {}", out["subject"]);
    // The attacker sees their own made-up product in their own cart-style block (their own data).
    let x1 = profile_id(&mut h, "x1@evil.test").await;
    let own = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": {"subject": "s", "blocks": [{"type": "products", "source": "event_items"}]}, "profile_id": x1}))).await;
    assert!(own["html"].as_str().unwrap().contains("FREE phone"));

    // Once the store's page confirms APRON, the page's details are what everyone sees.
    pages.lock().unwrap().insert("apron".into(), product_page("Linen apron", "30.00", "https://cdn.acme.test/apron.jpg"));
    sqlx::query("UPDATE catalog_products SET verify_after = now(), verify_attempts = 0 WHERE external_id = 'APRON'").execute(&h.state.db).await.unwrap();
    verify_all(&h).await;
    let apron = h.ok("GET", "/api/catalog/products/APRON", None).await;
    assert_eq!((apron["verified"].as_bool(), apron["title"].as_str(), apron["image_url"].as_str()), (Some(true), Some("Linen apron"), Some("https://cdn.acme.test/apron.jpg")));
    event(&h, &pk, "x1@evil.test", "Viewed Product", evil("APRON", "Scam apron")).await;
    learn::catch_up(&h.state).await.unwrap();
    assert_eq!(h.ok("GET", "/api/catalog/products/APRON", None).await["title"], "Linen apron", "a verified product isn't rewritten by events");
    h.cleanup().await;
}

/// Finding 5: price drops and restocks claim each person atomically, and a restock's limit holds
/// across jobs running at once and retries.
#[tokio::test]
async fn alert_jobs_tell_each_person_once() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let account: uuid::Uuid = me["account"]["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "P", "title": "Pan", "price": 100}))).await;
    for i in 0..5 {
        let email = format!("v{i}@x.test");
        event(&h, &pk, &email, "Viewed Product", json!({"name": "Pan", "product_id": "P"})).await;
    }
    learn::catch_up(&h.state).await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'subscribed'").execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE catalog_products SET price = 70 WHERE external_id = 'P'").execute(&h.state.db).await.unwrap();
    let pid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM catalog_products WHERE external_id = 'P'").fetch_one(&h.state.db).await.unwrap();
    // Two price drops handled at the same moment.
    let drop = |old: f64, new: f64| json!({"account_id": account, "product_id": pid, "old": old, "new": new, "drop_id": uuid::Uuid::now_v7(), "after": null});
    let (d1, d2) = (drop(100.0, 80.0), drop(80.0, 70.0));
    // Open the pool's connections first, so neither job waits on a new one and they truly overlap.
    let warm = |ms: f64| sqlx::query("SELECT pg_sleep($1)").bind(ms / 1000.0).execute(&h.state.db);
    let _ = tokio::join!(warm(50.0), warm(50.0), warm(50.0), warm(50.0));
    let (a, b) = tokio::join!(alerts::run_price_drop(&h.state, &d1), alerts::run_price_drop(&h.state, &d2));
    a.unwrap();
    b.unwrap();
    assert_eq!(metric_count(&h, "Price Dropped").await, 5, "each person told once");
    alerts::run_price_drop(&h.state, &d1).await.unwrap();
    assert_eq!(metric_count(&h, "Price Dropped").await, 5, "a retry tells nobody again");
    // The claim itself: a second drop gets nobody in the cooldown; the same drop (a retry) gets its own back.
    let viewers: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM profiles ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    let (other, retry) = tokio::join!(alerts::claim_price_drop(&h.state.db, pid, "another-drop", 30, &viewers), async {
        let key: String = sqlx::query_scalar("SELECT drop_key FROM catalog_price_drop_notices LIMIT 1").fetch_one(&h.state.db).await.unwrap();
        let mine: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT profile_id FROM catalog_price_drop_notices WHERE drop_key = $1").bind(&key).fetch_all(&h.state.db).await.unwrap();
        (alerts::claim_price_drop(&h.state.db, pid, &key, 30, &viewers).await.unwrap(), mine)
    });
    assert!(other.unwrap().is_empty(), "inside the cooldown, nobody is claimed twice");
    assert_eq!(retry.0.len(), retry.1.len());

    // The alert is queued in the same transaction as the price change: if it can't be queued, the
    // change isn't saved either (and the next save tries both again), so no drop is ever lost.
    sqlx::query("CREATE FUNCTION refuse_job() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'queue unavailable'; END $$ LANGUAGE plpgsql").execute(&h.state.db).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_drop BEFORE INSERT ON jobs FOR EACH ROW WHEN (NEW.kind = 'catalog_price_drop') EXECUTE FUNCTION refuse_job()").execute(&h.state.db).await.unwrap();
    h.req("POST", "/api/catalog/products", Some(json!({"external_id": "P", "price": 50}))).await;
    assert_eq!(h.ok("GET", "/api/catalog/products/P", None).await["price"], 70.0, "the price change rolled back with its alert");
    sqlx::query("DROP TRIGGER refuse_drop ON jobs").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "P", "price": 50}))).await;
    assert_eq!(jobs_of(&h, "catalog_price_drop").await, 1);

    // A restock with six people waiting and a limit of three.
    h.ok("PATCH", "/api/catalog/settings", Some(json!({"back_in_stock_limit": 3}))).await;
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "R", "title": "Rack", "inventory": 0}))).await;
    let rid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM catalog_products WHERE external_id = 'R'").fetch_one(&h.state.db).await.unwrap();
    sqlx::query(
        "INSERT INTO catalog_stock_subscriptions (id, account_id, profile_id, product_id, confirmed_at, created_at)
         SELECT gen_random_uuid(), account_id, id, $1, now(), now() - make_interval(mins => row_number() OVER (ORDER BY email)::int) FROM profiles",
    )
    .bind(rid)
    .execute(&h.state.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO profiles (id, account_id, email) VALUES (gen_random_uuid(), $1, 'w6@x.test')").bind(account).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO catalog_stock_subscriptions (id, account_id, profile_id, product_id, confirmed_at) SELECT gen_random_uuid(), account_id, id, $1, now() FROM profiles WHERE email = 'w6@x.test'")
        .bind(rid)
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM jobs").execute(&h.state.db).await.unwrap();
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "R", "inventory": 4}))).await;
    // The restock and its job were saved with the stock change.
    let job: Value = sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'catalog_back_in_stock'").fetch_one(&h.state.db).await.unwrap();
    let (a, b) = tokio::join!(alerts::run_back_in_stock(&h.state, &job), alerts::run_back_in_stock(&h.state, &job));
    a.unwrap();
    b.unwrap();
    alerts::run_back_in_stock(&h.state, &job).await.unwrap();
    assert_eq!(metric_count(&h, "Back in Stock").await, 3, "the limit holds");
    let told: Vec<String> = events_for(&h, "Back in Stock").await.into_iter().map(|e| e.0).collect();
    assert_eq!(told, vec!["v4@x.test", "v3@x.test", "v2@x.test"], "oldest requests first");
    h.cleanup().await;
}

/// Finding 6: a back-in-stock request with an address the site doesn't know waits for an emailed
/// confirmation; a visitor the store's emails identified is armed at once; limits hold under load.
#[tokio::test]
async fn back_in_stock_requests_are_confirmed_and_bounded() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let account: uuid::Uuid = me["account"]["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "G", "title": "Gloves", "inventory": 0}))).await;

    // Someone types a stranger's address: only a confirmation email goes out.
    let (st, v) = ask(&h, &pk, json!({"email": "stranger@x.test", "product_id": "G"}), Some("198.51.100.1")).await;
    assert_eq!((st, v["confirm"].as_bool()), (StatusCode::OK, Some(true)), "{v}");
    let mail = h.state.email.sent().into_iter().find(|m| m.to == "stranger@x.test").expect("confirmation sent");
    assert!(mail.subject.contains("Confirm your back-in-stock alert") && mail.html.contains("Gloves"));
    assert_eq!(metric_count(&h, "Subscribed to Back in Stock").await, 0);
    // Not confirmed: a restock tells them nothing.
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "G", "inventory": 2}))).await;
    h.drain().await;
    assert_eq!(metric_count(&h, "Back in Stock").await, 0);
    // Confirming while it's in stock just says so.
    let link = find_link(&mail.html, "/client/back-in-stock/confirm/");
    let (_, _, page) = h.raw("POST", &link, &[], "").await;
    assert!(page.contains("back in stock"), "{page}");
    assert_eq!(h.ok("GET", "/api/catalog/products/G", None).await["unconfirmed_total"], 0);

    // A visitor who came from one of the store's emails is armed at once, for their own address only.
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "G", "inventory": 0}))).await;
    let known: uuid::Uuid = h.ok("POST", "/api/profiles", Some(json!({"email": "known@x.test"}))).await["id"].as_str().unwrap().parse().unwrap();
    let token = dumbmail::public::profile_token(&h.state, account, known);
    let (st, v) = ask(&h, &pk, json!({"email": "known@x.test", "token": token, "product_id": "G"}), None).await;
    assert_eq!((st, v["confirm"].as_bool()), (StatusCode::OK, None), "{v}");
    assert_eq!(metric_count(&h, "Subscribed to Back in Stock").await, 1);
    let (_, v) = ask(&h, &pk, json!({"email": "someone-else@x.test", "token": token, "product_id": "G"}), None).await;
    assert_eq!(v["confirm"], true, "a token doesn't vouch for another address");
    assert!(h.state.email.sent().iter().all(|m| m.to != "known@x.test"));

    // Thirty requests at once from one person for thirty sold-out products: 25 are taken.
    let many: Vec<Value> = (0..30).map(|i| json!({"external_id": format!("S{i}"), "title": format!("Sock {i}"), "inventory": 0})).collect();
    h.ok("POST", "/api/catalog/products", Some(json!({"products": many}))).await;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..30 {
        let router = h.router.clone();
        let uri = format!("/client/back-in-stock?k={pk}");
        let body = json!({"email": "greedy@x.test", "product_id": format!("S{i}")}).to_string();
        set.spawn(async move {
            let req = Request::builder().method("POST").uri(uri).header("content-type", "application/json").header("x-real-ip", format!("192.0.2.{i}")).body(Body::from(body)).unwrap();
            router.oneshot(req).await.unwrap().status()
        });
    }
    let codes: Vec<StatusCode> = set.join_all().await;
    assert_eq!(codes.iter().filter(|c| **c == StatusCode::OK).count(), 25, "{codes:?}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions s JOIN profiles p ON p.id = s.profile_id WHERE p.email = 'greedy@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(rows, 25);
    let sent = h.state.email.sent().iter().filter(|m| m.to == "greedy@x.test").count();
    assert_eq!(sent, 3, "one inbox gets at most three confirmations a day");

    // Asking again and again for the same product doesn't get around the inbox's daily limit.
    for i in 0..6 {
        ask(&h, &pk, json!({"email": "again@x.test", "product_id": "G"}), Some(&format!("203.0.113.{i}"))).await;
    }
    assert_eq!(h.state.email.sent().iter().filter(|m| m.to == "again@x.test").count(), 3, "repeat requests count toward the three a day");

    // An account that has sent its confirmation emails for the hour sends no more.
    sqlx::query("INSERT INTO catalog_stock_confirm_sends (id, account_id, email_key) SELECT gen_random_uuid(), $1, 'k' || g FROM generate_series(1, 200) g").bind(account).execute(&h.state.db).await.unwrap();
    let (st, _) = ask(&h, &pk, json!({"email": "onemore@x.test", "product_id": "G"}), Some("203.0.113.50")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(h.state.email.sent().iter().all(|m| m.to != "onemore@x.test"));

    // An account at its hourly limit takes no more.
    sqlx::query(
        "INSERT INTO catalog_stock_subscriptions (id, account_id, profile_id, product_id, created_at, notified_at)
         SELECT gen_random_uuid(), $1, (SELECT id FROM profiles WHERE email = 'greedy@x.test'), (SELECT id FROM catalog_products WHERE external_id = 'G'), now(), now() FROM generate_series(1, 5000)",
    )
    .bind(account)
    .execute(&h.state.db)
    .await
    .unwrap();
    let (st, _) = ask(&h, &pk, json!({"email": "late@x.test", "product_id": "G"}), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    h.cleanup().await;
}

/// Finding 12: a variant's restock shows the variant, and a product the store unpublished doesn't
/// show from an event's own data.
#[tokio::test]
async fn event_items_show_variants_and_hide_unpublished_products() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/catalog/products", Some(json!({"products": [
        {"external_id": "TEE", "title": "Tee", "url": "https://acme.test/tee", "image_url": "https://cdn.acme.test/tee.jpg",
         "variants": [{"external_id": "TEE-S", "title": "Small", "price": 25, "image_url": "https://cdn.acme.test/tee-s.jpg"},
                      {"external_id": "TEE-M", "title": "Medium", "price": 30, "inventory": 0}]},
        {"external_id": "OLD", "title": "Old thing", "published": false}]}))).await;
    let event = json!({"metric": "Back in Stock", "properties": {"name": "Tee", "product_id": "TEE", "variant_id": "TEE-S",
        "items": [{"name": "Tee", "product_id": "TEE", "variant_id": "TEE-S"}, {"name": "Old thing", "product_id": "OLD", "price": 1}]}});
    let content = json!({"subject": "s", "blocks": [{"type": "products", "source": "event_items", "count": 4, "hide_out_of_stock": false}]});
    let out = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": content, "event": event}))).await;
    let html = out["html"].as_str().unwrap();
    assert!(html.contains("$25.00") && html.contains("tee-s.jpg"), "the variant's price and picture: {html}");
    assert!(!html.contains("Old thing"), "unpublished products don't show");
    h.cleanup().await;
}

/// Finding 10: a profile property called `catalog` keeps its name in templates.
#[tokio::test]
async fn profile_properties_keep_their_template_names() {
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/catalog/products", Some(json!({"external_id": "MUG", "title": "Mug", "price": 9}))).await;
    let gold = h.ok("POST", "/api/profiles", Some(json!({"email": "gold@x.test", "properties": {"catalog": "Gold club"}}))).await["id"].as_str().unwrap().to_string();
    let plain = h.ok("POST", "/api/profiles", Some(json!({"email": "plain@x.test"}))).await["id"].as_str().unwrap().to_string();
    let body = json!({"body": "[{{ catalog }}] {{ organization.catalog.products['MUG'].title }} {{ person.recommendations[0].title }}"});
    let out = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": body, "profile_id": gold}))).await;
    assert!(out["body"].as_str().unwrap().contains("[Gold club] Mug Mug"), "{out}");
    let out = h.ok("POST", "/api/render", Some(json!({"channel": "sms", "content": body, "profile_id": plain}))).await;
    assert!(out["body"].as_str().unwrap().contains("[] Mug Mug"), "the short name stays the property's on this account: {out}");
    h.cleanup().await;
}

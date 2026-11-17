//! Email content: new blocks, display rules, saved blocks, starter templates, checks before
//! sending, countdown timers, hosted icons and video thumbnails.

#[macro_use]
mod common;

use axum::http::StatusCode;
use dumbmail::models::Account;
use dumbmail::render::{render_email, EmailContent};
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

fn account() -> Account {
    Account {
        id: Uuid::nil(),
        public_key: "pk".into(),
        name: "Acme".into(),
        website: None,
        from_name: None,
        from_email: None,
        reply_to: None,
        sms_from: None,
        timezone: "America/Los_Angeles".into(),
        address: Some("1 Main St".into()),
        brand_color: "#123456".into(),
        logo_url: None,
        social_links: json!({}),
        plan: "free".into(),
        onboarding: json!({}),
        conversion_metric: "Placed Order".into(),
        created_at: chrono::Utc::now(),
        sending_paused_at: None,
        sending_paused_reason: None,
        email_daily_limit_override: None,
        email_sunset_enabled: true,
        link_domain: None,
        link_domain_status: "not_started".into(),
        link_domain_records: json!([]),
        verified_domains: Vec::new(),
    }
}

/// Render blocks with no preparation step (pure rendering of each block type).
fn render(blocks: Value, ctx: Value) -> (String, String) {
    let content: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": blocks})).unwrap();
    let r = render_email(&account(), &content, &ctx, None).unwrap();
    (r.html, r.text)
}

/// The rendered card body (between the layout's top and bottom spacer rows).
fn body(html: &str) -> &str {
    let start = html.find(r#"<tr><td style="height:8px;"></td></tr>"#).unwrap();
    let end = html.rfind(r#"<tr><td style="height:16px;"></td></tr>"#).unwrap();
    &html[start..end]
}

fn email_safe(html: &str) {
    let b = body(html);
    assert!(!b.contains("<div style=\"display:flex"), "no flex layouts");
    assert!(!b.contains("<script"), "no scripts");
    for t in b.split("<table").skip(1) {
        let open = t.split('>').next().unwrap();
        assert!(open.contains("cellpadding=\"0\""), "tables reset padding: {open}");
    }
}

#[test]
fn renders_each_new_block_as_email_safe_html() {
    let icons = json!([{"href": "https://instagram.com/acme", "src": "https://pl.test/e/icons/color-instagram.png", "label": "Instagram"}]);
    let (html, text) = render(
        json!([
            {"type": "social", "align": "center", "size": 40, "_icons": icons},
            {"type": "video", "url": "https://youtu.be/dQw4w9WgXcQ", "thumbnail": "https://pl.test/i/x", "alt": "Watch the launch", "caption": "One minute"},
            {"type": "video", "url": "https://example.test/v", "alt": "No thumb"},
            {"type": "countdown", "_src": "https://pl.test/e/countdown/t.gif", "_alt": "Ends Dec 1"},
            {"type": "table", "columns": [{"label": "Item"}, {"label": "Price", "align": "right"}], "rows": [["Tote", "$48"]],
                "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}", "{{ item.price }}"]}, "footer": [["Total", "{{ event.value }}"]]},
            {"type": "quote", "text": "Love it", "author": "Jo", "role": "Customer", "style": "card"},
            {"type": "header", "logo": "https://pl.test/logo.png", "logo_alt": "Acme", "links": [{"text": "Shop", "href": "https://acme.test/shop"}, {"text": "Bad", "href": "javascript:alert(1)"}]},
            {"type": "nav", "links": [{"text": "New", "href": "https://acme.test/new"}], "background": "#000000"},
            {"type": "footer", "text": "Questions? Reply.", "links": [{"text": "Website", "href": "https://acme.test"}], "_icons": icons},
            {"type": "image_text", "src": "https://pl.test/a.png", "alt": "Bag", "heading": "Our bag", "html": "<p>Strong.</p>", "button_text": "Buy", "button_href": "https://acme.test/bag", "image_position": "right", "radius": 12},
            {"type": "image", "src": "https://pl.test/b.png", "alt": "Hero", "radius": 0}
        ]),
        json!({"event": {"value": "$96", "properties": {"items": [{"name": "Cap", "price": "$12"}, {"name": "Mug", "price": "$9"}]}}}),
    );
    email_safe(&html);
    // Social: hosted icon with fixed size attributes for clients that ignore CSS.
    assert!(html.contains(r#"<img src="https://pl.test/e/icons/color-instagram.png" width="40" height="40" alt="Instagram""#));
    // Video: linked thumbnail, and a panel with a play link when there's no thumbnail.
    assert!(html.contains(r#"<a href="https://youtu.be/dQw4w9WgXcQ" style="display:block;text-decoration:none;"><img src="https://pl.test/i/x" alt="Watch the launch""#));
    assert!(html.contains("&#9654;&nbsp; No thumb"));
    assert!(html.contains(r#"<img src="https://pl.test/e/countdown/t.gif" alt="Ends Dec 1" width="552""#));
    // Table: static rows, one row per event item, and the total.
    assert!(html.contains(">Tote<") && html.contains(">Cap<") && html.contains(">Mug<") && html.contains(">$96<"));
    assert!(html.contains("border-collapse:collapse"));
    assert!(html.contains("&ldquo;Love it&rdquo;"));
    assert!(html.contains("https://acme.test/shop") && !html.contains("javascript:"), "unsafe links are dropped");
    assert!(html.contains(r##"bgcolor="#000000""##));
    // Image beside text: stacks on phones, image on the right on desktop, Outlook button.
    assert!(html.contains(r#"dir="rtl""#) && html.contains(r#"class="pl-col" dir="ltr""#));
    assert!(html.contains("<!--[if mso]><v:roundrect") && html.contains(r##"fillcolor="#123456""##));
    assert!(html.contains("border-radius:12px") && html.contains("border-radius:0px"));
    // Plain text has every block.
    for needle in ["Instagram: https://instagram.com/acme", "Watch the launch: https://youtu.be/dQw4w9WgXcQ", "Ends Dec 1", "Item | Price", "Cap | $12", "Total | $96", "\"Love it\" - Jo, Customer", "Shop: https://acme.test/shop", "Our bag", "Buy: https://acme.test/bag"] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
}

#[test]
fn table_loop_is_capped_and_its_source_cannot_inject() {
    let items: Vec<Value> = (0..80).map(|i| json!({"name": format!("item{i}")})).collect();
    let (html, _) = render(json!([{"type": "table", "columns": [{"label": "Item"}], "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}"]}}]), json!({"event": {"properties": {"items": items}}}));
    assert!(html.contains(">item49<") && !html.contains(">item50<"), "at most 50 rows");
    let (html, _) = render(json!([{"type": "table", "columns": [{"label": "X"}], "loop": {"source": "x %}{{ 7*7 }}{% for y in z", "cells": ["a"]}}]), json!({}));
    assert!(!html.contains("49"));
    // A table with no rows from a missing list renders without error.
    let (html, _) = render(json!([{"type": "table", "columns": [{"label": "Item"}], "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}"]}}]), json!({"event": {}}));
    assert!(html.contains(">Item<"));
}

#[test]
fn markers_from_content_cannot_break_out_of_the_template() {
    // Raw if_open/if_close are internal; a hostile expression is replaced by `false`.
    let (html, _) = render(json!([{"type": "if_open", "expr": "true %}{{ 7*7 }}{% if true"}, {"type": "text", "html": "<p>SECRETTEXT</p>"}, {"type": "if_close"}]), json!({}));
    assert!(!body(&html).contains("49") && !html.contains("SECRETTEXT"));
}

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn profile(h: &Harness, acct: Uuid, email: &str, consent: &str, first: Option<&str>, props: Value) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent, first_name, properties) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(acct)
        .bind(email)
        .bind(consent)
        .bind(first)
        .bind(props)
        .execute(&h.state.db)
        .await
        .unwrap();
    id
}

async fn preview(h: &mut Harness, content: Value, profile_id: Option<Uuid>) -> Value {
    h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": content, "profile_id": profile_id}))).await
}

#[tokio::test]
async fn display_rules_vary_one_email_by_person() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let vip = profile(&h, acct, "vip@x.test", "subscribed", Some("Vee"), json!({"tier": "Gold", "visits": "12"})).await;
    let other = profile(&h, acct, "reg@x.test", "subscribed", Some("Reg"), json!({"tier": "Silver"})).await;
    let content = json!({"subject": "s", "blocks": [
        {"type": "text", "html": "<p>Everyone</p>"},
        {"type": "text", "html": "<p>Gold perk</p>", "show_if": {"field": "person.properties.tier", "op": "eq", "value": "gold"}},
        {"type": "text", "html": "<p>Frequent</p>", "show_if": {"expr": "person.properties.visits is defined and person.properties.visits|int > 10"}},
        {"type": "columns", "columns": [[{"type": "text", "html": "<p>In column for silver</p>", "show_if": {"field": "person.properties.tier", "op": "eq", "value": "silver"}}], [{"type": "text", "html": "<p>Right</p>"}]]}
    ]});
    let v = preview(&mut h, content.clone(), Some(vip)).await;
    let html = v["html"].as_str().unwrap();
    assert!(html.contains("Everyone") && html.contains("Gold perk") && html.contains("Frequent") && !html.contains("In column for silver"));
    assert!(v["text"].as_str().unwrap().contains("Gold perk"));
    let v = preview(&mut h, content.clone(), Some(other)).await;
    let html = v["html"].as_str().unwrap();
    assert!(!html.contains("Gold perk") && !html.contains("Frequent") && html.contains("In column for silver"));
    assert!(!v["text"].as_str().unwrap().contains("Frequent"));

    // A broken rule is refused with the block it's on, and a rule can't escape the tag.
    let (s, e) = h.req("POST", "/api/render", Some(json!({"channel": "email", "content": {"subject": "s", "blocks": [{"type": "text", "html": "x"}, {"type": "text", "html": "y", "show_if": {"expr": "x %}{{ organization }}{% if y"}}]}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"]["message"].as_str().unwrap().starts_with("block 2:"), "{e}");

    // A real send applies the rule per recipient.
    h.credit(5.0).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": [vip, other]}))).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Perks", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let sent = h.state.email.sent();
    let to_vip = sent.iter().find(|m| m.to == "vip@x.test").unwrap();
    let to_reg = sent.iter().find(|m| m.to == "reg@x.test").unwrap();
    assert!(to_vip.html.contains("Gold perk") && !to_reg.html.contains("Gold perk"));
    h.cleanup().await;
}

#[tokio::test]
async fn saved_blocks_update_unsent_emails_and_sent_ones_keep_what_they_sent() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    let p = profile(&h, acct, "ann@x.test", "subscribed", Some("Ann"), json!({})).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": [p]}))).await;

    let (s, sb) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Sign-off", "blocks": [{"type": "text", "html": "<p>Version one</p>"}]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{sb}");
    let id = sb["id"].as_str().unwrap().to_string();
    let (s, _) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "sign-off", "blocks": [{"type": "divider"}]}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "names are unique per account, ignoring case");

    let reference = json!({"type": "saved", "saved_block_id": id, "name": "Sign-off", "blocks": [{"type": "text", "html": "<p>Stale copy</p>"}]});
    let content = json!({"subject": "Hi", "blocks": [{"type": "heading", "text": "News"}, reference]});
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "Uses it", "channel": "email", "content": content}))).await;
    let sent_c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Goes out", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
    let draft_c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Draft", "content": content}))).await;

    // Previews show the saved block's current content, not the stored copy.
    let v = preview(&mut h, content.clone(), None).await;
    assert!(v["html"].as_str().unwrap().contains("Version one") && !v["html"].as_str().unwrap().contains("Stale copy"));

    h.ok("POST", &format!("/api/campaigns/{}/send", sent_c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    assert!(h.state.email.sent().iter().any(|m| m.to == "ann@x.test" && m.html.contains("Version one")));

    // Edit the saved block: unsent emails change; the sent campaign keeps its copy.
    let up = h.ok("PATCH", &format!("/api/content/saved-blocks/{id}"), Some(json!({"blocks": [{"type": "text", "html": "<p>Version two</p>"}]}))).await;
    let used: Vec<(String, bool)> = up["used_in"].as_array().unwrap().iter().map(|u| (u["name"].as_str().unwrap().to_string(), u["picks_up_changes"].as_bool().unwrap())).collect();
    assert!(used.contains(&("Uses it".into(), true)) && used.contains(&("Draft".into(), true)) && used.contains(&("Goes out".into(), false)), "{used:?}");
    let sent_row = h.ok("GET", &format!("/api/campaigns/{}", sent_c["id"].as_str().unwrap()), None).await;
    assert_eq!(sent_row["content"]["blocks"][1]["frozen"], true);
    assert_eq!(sent_row["content"]["blocks"][1]["blocks"][0]["html"], "<p>Version one</p>");
    let tpl = h.ok("GET", &format!("/api/templates/{}", t["id"].as_str().unwrap()), None).await;
    assert!(preview(&mut h, tpl["content"].clone(), None).await["html"].as_str().unwrap().contains("Version two"));
    let d = h.ok("GET", &format!("/api/campaigns/{}", draft_c["id"].as_str().unwrap()), None).await;
    assert!(preview(&mut h, d["content"].clone(), None).await["html"].as_str().unwrap().contains("Version two"));
    let list_v = h.ok("GET", "/api/content/saved-blocks", None).await;
    assert_eq!(list_v["data"][0]["uses"], 2, "the template and the draft (sent campaigns don't count)");

    // Saved blocks can't hold other saved blocks.
    let (s, _) = h.req("PATCH", &format!("/api/content/saved-blocks/{id}"), Some(json!({"blocks": [reference]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Deleting hides it from the list; emails that use it keep its last content.
    h.ok("DELETE", &format!("/api/content/saved-blocks/{id}"), None).await;
    assert_eq!(h.ok("GET", "/api/content/saved-blocks", None).await["data"].as_array().unwrap().len(), 0);
    assert!(preview(&mut h, content.clone(), None).await["html"].as_str().unwrap().contains("Version two"));
    let (s, _) = h.req("GET", &format!("/api/content/saved-blocks/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn saved_blocks_stay_inside_their_account() {
    let mut h = harness!();
    h.signup().await;
    let sb = h.ok("POST", "/api/content/saved-blocks", Some(json!({"name": "Secret", "blocks": [{"type": "text", "html": "<p>Account one only</p>"}]}))).await;
    let id = sb["id"].as_str().unwrap().to_string();
    // A second account.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "two@b.test", "password": "correct horse", "company": "Two"}))).await;
    for (m, body) in [("GET", None), ("PATCH", Some(json!({"name": "Mine now"}))), ("DELETE", None)] {
        let (s, _) = h.req(m, &format!("/api/content/saved-blocks/{id}"), body).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m}");
    }
    let (s, _) = h.req("GET", &format!("/api/content/saved-blocks/{id}/usage"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Pointing at it from this account shows only the copy this account wrote.
    let v = preview(&mut h, json!({"subject": "s", "blocks": [{"type": "saved", "saved_block_id": id, "blocks": [{"type": "text", "html": "<p>My copy</p>"}]}]}), None).await;
    let html = v["html"].as_str().unwrap();
    assert!(html.contains("My copy") && !html.contains("Account one only"));
    assert_eq!(h.ok("GET", "/api/content/saved-blocks", None).await["total"], 0);
    h.cleanup().await;
}

#[tokio::test]
async fn saved_block_limit_holds_under_racing_requests() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    for i in 0..199 {
        sqlx::query("INSERT INTO saved_blocks (id, account_id, name, blocks) VALUES ($1, $2, $3, '[{\"type\":\"divider\"}]')")
            .bind(Uuid::now_v7())
            .bind(acct)
            .bind(format!("b{i}"))
            .execute(&h.state.db)
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for i in 0..4 {
        let router = h.router.clone();
        let cookie = h.cookie.clone().unwrap();
        tasks.push(tokio::spawn(async move {
            use tower::ServiceExt;
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/api/content/saved-blocks")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({"name": format!("race{i}"), "blocks": [{"type": "divider"}]}).to_string()))
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }));
    }
    let mut created = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::CREATED {
            created += 1;
        }
    }
    assert_eq!(created, 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM saved_blocks WHERE account_id = $1 AND deleted_at IS NULL").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 200);
    h.cleanup().await;
}

#[tokio::test]
async fn starter_library_renders_in_the_brand_and_makes_templates() {
    let mut h = harness!();
    h.signup().await;
    h.ok("PATCH", "/api/account", Some(json!({"brand_color": "#aa3366", "social_links": {"instagram": "https://instagram.com/acme"}}))).await;
    let list = h.ok("GET", "/api/content/starters?preview=true", None).await;
    let data = list["data"].as_array().unwrap();
    assert!((12..=15).contains(&data.len()));
    for s in data.iter().filter(|s| s["key"] != "personal_note") {
        let html = s["html"].as_str().unwrap_or_else(|| panic!("{} has no preview", s["key"]));
        assert!(html.contains("#aa3366"), "{} uses the brand color", s["key"]);
    }
    let cart = data.iter().find(|s| s["key"] == "abandoned_cart").unwrap();
    assert!(cart["html"].as_str().unwrap().contains("Everyday tote"), "flow starters preview with a sample event");
    let welcome = data.iter().find(|s| s["key"] == "welcome").unwrap();
    assert!(welcome["html"].as_str().unwrap().contains("https://pl.test/e/icons/light-instagram.png"), "the footer picks up the account's social links");

    let (s, t) = h.req("POST", "/api/content/starters/order_confirmation/template", Some(json!({"name": "Receipt"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{t}");
    assert_eq!(t["name"], "Receipt");
    assert_eq!(t["content"]["theme"]["brand_color"], "#aa3366");
    let (s, _) = h.req("POST", "/api/content/starters/nope/template", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Hosted assets.
    let (s, hd, _) = h.raw("GET", "/e/icons/color-instagram.png", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("content-type").unwrap(), "image/png");
    let (s, _, _) = h.raw("GET", "/e/icons/..%2F..%2Fetc%2Fpasswd", &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h.raw("GET", "/e/assets/placeholder-hero.png", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    h.cleanup().await;
}

#[tokio::test]
async fn countdown_images_are_signed_and_bounded() {
    let mut h = harness!();
    h.signup().await;
    let ends = (chrono::Utc::now() + chrono::Duration::hours(30)).to_rfc3339();
    let v = preview(&mut h, json!({"subject": "s", "blocks": [{"type": "countdown", "ends_at": ends, "_src": "https://evil.test/x.gif"}]}), None).await;
    let html = v["html"].as_str().unwrap();
    assert!(!html.contains("evil.test"), "content can't set the image address");
    let src = find_link(html, "https://pl.test/e/countdown/");
    assert!(v["text"].as_str().unwrap().contains("Ends "));
    let path = src.replace("https://pl.test", "");
    let (s, hd, body) = h.raw("GET", &path, &[], "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd.get("content-type").unwrap(), "image/gif");
    assert!(hd.get("cache-control").unwrap().to_str().unwrap().contains("no-store"));
    assert!(body.starts_with("GIF89a"));
    // A changed end time breaks the signature.
    let token = path.trim_start_matches("/e/countdown/").trim_end_matches(".gif");
    let mut parts: Vec<String> = token.split('~').map(String::from).collect();
    parts[2] = "9999999999".into();
    let (s, _, _) = h.raw("GET", &format!("/e/countdown/{}.gif", parts.join("~")), &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h.raw("GET", "/e/countdown/garbage.gif", &[], "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Opens in the same second share one drawing; an account past its budget gets the last one.
    let now = chrono::Utc::now().timestamp();
    let a = dumbmail::features::content::countdown::image(&h.state.cfg.secret_key, token, now).await.unwrap();
    let b = dumbmail::features::content::countdown::image(&h.state.cfg.secret_key, token, now).await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    let mut last = Ok(a);
    for k in 1..=(dumbmail::features::content::countdown::ACCOUNT_PER_MINUTE as i64 + 5) {
        last = dumbmail::features::content::countdown::image(&h.state.cfg.secret_key, token, now - 100_000 * k).await;
    }
    assert!(last.is_ok(), "past the budget, the cached image is served");
    h.cleanup().await;
}

#[tokio::test]
async fn checks_report_fixable_problems_without_reaching_private_addresses() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let a = profile(&h, acct, "a@x.test", "subscribed", Some("Ann"), json!({"city": "Portland"})).await;
    let b = profile(&h, acct, "b@x.test", "never_subscribed", None, json!({})).await;
    let gone = profile(&h, acct, "c@x.test", "unsubscribed", None, json!({})).await;
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "All"}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": [a, b, gone]}))).await;
    let big = "x".repeat(110 * 1024);
    let c = h
        .ok("POST", "/api/campaigns", Some(json!({"name": "Check me", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {
            "subject": "HUGE SALE TODAY ONLY!!",
            "blocks": [
                {"type": "text", "html": "<p>Hi {{ first_name }}, from {{ city }} ({{ person.properties.city|default('your town') }}). Total {{ event.value }}.</p>"},
                {"type": "image", "src": "https://pl.test/e/assets/placeholder-hero.png", "alt": "", "href": "http://127.0.0.1:9/admin"},
                {"type": "button", "text": "Go", "href": "https://"},
                {"type": "text", "html": "<p><a href=\"https://bit.ly/abc\">www.mybank.com</a> <a href=\"http://169.254.169.254/latest/meta-data\">meta</a></p>"},
                {"type": "html", "html": big}
            ]}})))
        .await;
    let r = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": c["id"]}))).await;
    let ids: Vec<&str> = r["findings"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
    for want in ["preheader_missing", "subject_caps", "subject_punctuation", "alt_missing", "placeholder_images", "empty_links", "shortener", "link_text_mismatch", "gmail_clip", "never_subscribed", "personalization_empty", "event_in_campaign", "broken_links", "domain_unauthenticated"] {
        assert!(ids.contains(&want), "{want} missing from {ids:?}");
    }
    assert_eq!(r["recipients"], 2, "the unsubscribed person isn't counted");
    let empty: Vec<&Value> = r["findings"].as_array().unwrap().iter().filter(|f| f["id"] == "personalization_empty").collect();
    let titles: Vec<&str> = empty.iter().map(|f| f["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"{{ first_name }} is empty for 1 person in this audience"), "{titles:?}");
    assert!(titles.contains(&"{{ person.properties.city }} is empty for 1 person in this audience"), "{titles:?}");
    // Private and metadata addresses are reported without being requested.
    let links = r["links"].as_array().unwrap();
    for u in ["http://127.0.0.1:9/admin", "http://169.254.169.254/latest/meta-data"] {
        let l = links.iter().find(|l| l["url"] == u).unwrap_or_else(|| panic!("{u} in {links:?}"));
        assert_eq!(l["ok"], false);
        assert_eq!(l["note"], "doesn't point to a public website");
        assert!(l["status"].is_null(), "never requested");
    }
    assert!(!links.iter().any(|l| l["url"].as_str().unwrap().starts_with("https://pl.test")), "our own addresses aren't requested");
    assert!(r["findings"].as_array().unwrap().iter().all(|f| f["blocks_sending"] == false), "{r}");

    // An empty audience blocks sending, like the product does.
    let empty_list = h.ok("POST", "/api/lists", Some(json!({"name": "Empty"}))).await;
    let r = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": c["id"], "skip_links": true, "audience": {"include": [{"type": "list", "id": empty_list["id"]}]}}))).await;
    let f = r["findings"].as_array().unwrap().iter().find(|f| f["id"] == "audience_empty").expect("empty audience");
    assert_eq!(f["blocks_sending"], true);
    assert!(r["links"].as_array().unwrap().is_empty());

    // Flow emails: event data is expected, so it's only a reminder to add a fallback.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "content": {"subject": "Thanks", "preheader": "Receipt", "blocks": [{"type": "text", "html": "<p>Order {{ event.properties.order_id }}</p>"}]}, "next": null}]}}))).await;
    let r = h.ok("POST", "/api/content/check", Some(json!({"flow_id": flow["id"], "node_id": "e1", "skip_links": true}))).await;
    let ids: Vec<&str> = r["findings"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"event_no_default") && !ids.contains(&"event_in_campaign"), "{ids:?}");

    // Another account can't check this account's campaign or flow.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "two@b.test", "password": "correct horse", "company": "Two"}))).await;
    let (s, _) = h.req("POST", "/api/content/check", Some(json!({"campaign_id": c["id"]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.req("POST", "/api/content/check", Some(json!({"flow_id": flow["id"], "node_id": "e1"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn video_thumbnails_from_uploads_and_never_from_private_addresses() {
    let mut h = harness!();
    h.signup().await;
    let mut img = image::RgbImage::new(320, 180);
    for p in img.pixels_mut() {
        *p = image::Rgb([20, 120, 200]);
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let (s, _, up) = h.bytes("POST", "/api/images", "image/png", png).await;
    assert_eq!(s, StatusCode::OK);
    let up: Value = serde_json::from_slice(&up).unwrap();
    let t = h.ok("POST", "/api/content/video-thumbnail", Some(json!({"image_url": up["url"]}))).await;
    assert_eq!(t["source"], "image");
    assert!(t["thumbnail_url"].as_str().unwrap().starts_with("https://pl.test/i/"));
    assert_ne!(t["thumbnail_url"], up["url"]);

    for bad in ["http://127.0.0.1:9/x.png", "http://169.254.169.254/latest", "http://user:pw@example.com/x.png", "file:///etc/passwd"] {
        let (s, e) = h.req("POST", "/api/content/video-thumbnail", Some(json!({"image_url": bad}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {e}");
    }
    let (s, e) = h.req("POST", "/api/content/video-thumbnail", Some(json!({"video_url": "https://example.com/video.mp4"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(e["error"]["message"].as_str().unwrap().contains("YouTube and Vimeo"));

    // Another account's uploaded image can't be used.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "two@b.test", "password": "correct horse", "company": "Two"}))).await;
    let (s, _) = h.req("POST", "/api/content/video-thumbnail", Some(json!({"image_url": up["url"]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn mcp_tools_cover_saved_blocks_starters_and_checks() {
    let mut h = harness!();
    h.signup().await;
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "mcp"}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {key}");
    let call = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let rpc = |body: String| {
        let (router, auth) = (h.router.clone(), auth.clone());
        async move {
            use http_body_util::BodyExt;
            use tower::ServiceExt;
            let req = axum::http::Request::builder().method("POST").uri("/mcp").header("authorization", auth).header("content-type", "application/json").body(axum::body::Body::from(body)).unwrap();
            let bytes = router.oneshot(req).await.unwrap().into_body().collect().await.unwrap().to_bytes();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            v["result"]["structuredContent"].clone()
        }
    };
    let list = rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string()).await;
    let _ = list;
    let saved = rpc(call("save_block", json!({"name": "Footer", "blocks": [{"type": "footer", "text": "Bye"}]}))).await;
    let id = saved["id"].as_str().expect("saved").to_string();
    let dry = rpc(call("delete_saved_block", json!({"id": id}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(rpc(call("list_saved_blocks", json!({}))).await["total"], 1, "a dry run deletes nothing");
    rpc(call("update_saved_block", json!({"id": id, "name": "Footer 2"}))).await;
    assert_eq!(rpc(call("delete_saved_block", json!({"id": id, "confirm": true}))).await["ok"], true);
    assert_eq!(rpc(call("list_saved_blocks", json!({}))).await["total"], 0);
    let starters = rpc(call("list_starter_templates", json!({}))).await;
    assert!(starters["data"].as_array().unwrap().len() >= 12);
    let one = rpc(call("list_starter_templates", json!({"key": "sale"}))).await;
    assert!(one["content"]["blocks"].as_array().unwrap().iter().any(|b| b["type"] == "countdown"));
    let t = rpc(call("create_template_from_starter", json!({"key": "welcome"}))).await;
    assert_eq!(t["name"], "Welcome");
    let r = rpc(call("check_email", json!({"content": {"subject": "Hello", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}, "skip_links": true}))).await;
    assert!(r["findings"].as_array().unwrap().iter().any(|f| f["id"] == "preheader_missing"));
    h.cleanup().await;
}

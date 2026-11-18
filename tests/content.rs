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
        closed_at: None,
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
fn display_rules_never_become_template_tags() {
    // Rules are decided before rendering; nothing a block holds turns into `{% if %}`. Blocks named
    // like the old markers are unknown types and render nothing, as on main.
    let (html, _) = render(json!([{"type": "if_open", "expr": "true %}{{ 7*7 }}{% if true"}, {"type": "text", "html": "<p>VISIBLE</p>"}, {"type": "if_close"}]), json!({}));
    assert!(!body(&html).contains("49") && html.contains("VISIBLE"));
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
    h.ok("DELETE", &format!("/api/content/saved-blocks/{id}?confirm=true"), None).await;
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
        // Cookie writes carry the app's origin and the open account, as the dashboard sends them.
        let builder = h.page_headers("POST", axum::http::Request::builder().method("POST").uri("/api/content/saved-blocks")).await;
        tasks.push(tokio::spawn(async move {
            use tower::ServiceExt;
            let req = builder
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
    for l in links.iter().filter(|l| l["url"].as_str().unwrap().starts_with("https://pl.test")) {
        assert_eq!(l["note"], "DumbMail link, not checked", "our own addresses aren't requested: {l}");
        assert!(l["status"].is_null());
    }
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

#[tokio::test]
async fn link_checks_only_call_web_ports() {
    let r = dumbmail::features::content::net::request(reqwest::Method::HEAD, "https://example.com:22/", 0, std::time::Duration::from_secs(5)).await;
    // DNS may be unavailable where tests run; either way nothing is requested on port 22.
    assert!(matches!(r, Err(dumbmail::features::content::net::NetError::Failed("uses a port we don't check")) | Err(dumbmail::features::content::net::NetError::NoSuchDomain) | Err(dumbmail::features::content::net::NetError::Timeout)), "{r:?}");
    let r = dumbmail::features::content::net::request(reqwest::Method::HEAD, "http://user:pw@example.com/", 0, std::time::Duration::from_secs(5)).await;
    assert!(matches!(r, Err(dumbmail::features::content::net::NetError::Invalid)));
}

// ---------- Review round: findings 1-12 ----------

async fn list_of(h: &mut Harness, people: &[Uuid]) -> Value {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": format!("L{}", Uuid::now_v7().simple())}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": people}))).await;
    list
}

#[tokio::test]
async fn a_display_rule_never_breaks_a_send() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    let vip = profile(&h, acct, "vip@x.test", "subscribed", Some("Vee"), json!({"tier": "gold", "points": 5})).await;
    let other = profile(&h, acct, "reg@x.test", "subscribed", Some("Reg"), json!({"tier": "silver", "points": "lots"})).await;
    let list = list_of(&mut h, &[vip, other]).await;
    let gold = json!({"field": "person.properties.tier", "op": "eq", "value": "gold"});
    // Everything is for gold members; the advanced rule can't be worked out for "lots" of points.
    let content = json!({"subject": "Gold only", "preheader": "p", "blocks": [
        {"type": "heading", "text": "For gold members", "show_if": gold},
        {"type": "text", "html": "<p>Points bonus</p>", "show_if": [gold, {"expr": "person.properties.points + 1 > 2"}]},
    ], "html": "<p>OLD RAW HTML</p>"});

    // The check panel says how many people would get nothing.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Gold", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
    let r = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": c["id"], "skip_links": true}))).await;
    let f = r["findings"].as_array().unwrap().iter().find(|f| f["id"] == "rules_hide_everything").unwrap_or_else(|| panic!("{r}"));
    assert_eq!(f["title"], "Display rules hide every block for 1 person");

    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let row = h.ok("GET", &format!("/api/campaigns/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(row["status"], "sent", "{row}");
    let sent = h.state.email.sent();
    let to_vip = sent.iter().find(|m| m.to == "vip@x.test").expect("the gold member gets it");
    assert!(to_vip.html.contains("For gold members") && to_vip.html.contains("Points bonus"));
    assert!(!sent.iter().any(|m| m.to == "reg@x.test"), "nobody gets an empty shell or the old raw html");
    let why: (String, String) = sqlx::query_as("SELECT status, error FROM messages WHERE profile_id = $1").bind(other).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(why, ("skipped".into(), "nothing to show for this person".into()));
    let progress = h.ok("GET", &format!("/api/campaigns/{}/send-progress", c["id"].as_str().unwrap()), None).await;
    assert!(progress["skip_reasons"].as_array().unwrap().iter().any(|r| r["reason"] == "nothing to show for this person" && r["count"] == 1), "{progress}");

    // A rule that fails for one person hides only its own block, for that person only.
    let p3 = profile(&h, acct, "lots@x.test", "subscribed", None, json!({"tier": "gold", "points": "lots"})).await;
    let list2 = list_of(&mut h, &[vip, p3]).await;
    let c2 = h.ok("POST", "/api/campaigns", Some(json!({"name": "Gold 2", "audience": {"include": [{"type": "list", "id": list2["id"]}]}, "content": content}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c2["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let sent = h.state.email.sent();
    let lots = sent.iter().find(|m| m.to == "lots@x.test").expect("sent");
    assert!(lots.html.contains("For gold members") && !lots.html.contains("Points bonus"));
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{}", c2["id"].as_str().unwrap()), None).await["status"], "sent");

    // Previews: a real person with nothing to show is told so; the sample preview shows every block.
    let (s, e) = h.req("POST", "/api/render", Some(json!({"channel": "email", "content": content, "profile_id": other}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(e["error"]["message"].as_str().unwrap().starts_with("Nothing to show for this person"), "{e}");
    assert!(preview(&mut h, content.clone(), None).await["html"].as_str().unwrap().contains("For gold members"));
    h.cleanup().await;
}

#[tokio::test]
async fn saved_block_edits_cant_break_emails_set_to_send() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    let p = profile(&h, acct, "ann@x.test", "subscribed", Some("Ann"), json!({})).await;
    let list = list_of(&mut h, &[p]).await;
    let sb = h.ok("POST", "/api/content/saved-blocks", Some(json!({"name": "Footer", "blocks": [{"type": "text", "html": "<p>One</p>"}]}))).await;
    let id = sb["id"].as_str().unwrap().to_string();
    let r = json!({"type": "saved", "saved_block_id": id, "blocks": []});
    let refs: Vec<Value> = (0..7).map(|_| r.clone()).collect();
    let content = json!({"subject": "Hi", "preheader": "p", "blocks": refs});
    let ab = json!({"metric": "click_rate", "pick": "auto", "test_percent": 100, "wait_hours": 4, "variants": [{"id": "b", "name": "B", "content": {"subject": "Hi B", "preheader": "p", "blocks": [r]}}]});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Weekly", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content, "ab_test": ab}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let at = (chrono::Utc::now() + chrono::Duration::hours(3)).to_rfc3339();
    h.ok("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": at}))).await;

    // 7 uses × 50 blocks would add 350 blocks; a template error would fail every recipient.
    let fifty: Vec<Value> = (0..50).map(|_| json!({"type": "divider"})).collect();
    for bad in [json!(fifty), json!([{"type": "text", "html": "<p>{{ unclosed</p>"}])] {
        let (s, e) = h.req("PATCH", &format!("/api/content/saved-blocks/{id}"), Some(json!({"blocks": bad}))).await;
        assert_eq!(s, StatusCode::CONFLICT, "{e}");
        assert!(e["error"]["message"].as_str().unwrap().contains("campaign \"Weekly\""), "{e}");
    }
    assert_eq!(h.ok("GET", &format!("/api/content/saved-blocks/{id}"), None).await["blocks"][0]["html"], "<p>One</p>", "refused edits change nothing");
    // A good edit is fine.
    h.ok("PATCH", &format!("/api/content/saved-blocks/{id}"), Some(json!({"blocks": [{"type": "text", "html": "<p>Two</p>"}]}))).await;

    // When the send starts, every version is copied in from one read, and checked as copied.
    sqlx::query("UPDATE campaigns SET send_at = now() WHERE id = $1::uuid").bind(&cid).execute(&h.state.db).await.unwrap();
    dumbmail::jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    let row = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert_eq!(row["content"]["blocks"][0]["frozen"], true, "{row}");
    assert_eq!(row["content"]["blocks"][0]["blocks"][0]["html"], "<p>Two</p>");
    assert_eq!(row["ab_test"]["variants"][0]["content"]["blocks"][0]["blocks"][0]["html"], "<p>Two</p>");

    // A copy that can't be sent (here: the saved block changed behind the API's back) fails the
    // campaign before anyone gets it.
    let c2 = h.ok("POST", "/api/campaigns", Some(json!({"name": "Broken", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"subject": "B", "preheader": "p", "blocks": [{"type": "saved", "saved_block_id": id, "blocks": []}]}}))).await;
    let c2id = c2["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/campaigns/{c2id}/schedule"), Some(json!({"send_at": at}))).await;
    sqlx::query("UPDATE saved_blocks SET blocks = '[{\"type\": \"text\", \"html\": \"{{ unclosed\"}]' WHERE id = $1::uuid").bind(&id).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE campaigns SET send_at = now() WHERE id = $1::uuid").bind(&c2id).execute(&h.state.db).await.unwrap();
    let before = h.state.email.sent().len();
    dumbmail::jobs::scheduler_tick(&h.state, 1).await.unwrap();
    h.drain().await;
    let row = h.ok("GET", &format!("/api/campaigns/{c2id}"), None).await;
    assert_eq!(row["status"], "failed", "{row}");
    assert!(row["error"].as_str().unwrap().contains("template"), "{row}");
    assert_eq!(h.state.email.sent().len(), before, "nobody got it");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1::uuid").bind(&c2id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);

    // Duplicating a sent campaign gives a new email whose saved blocks are linked again.
    let copy = h.ok("POST", &format!("/api/campaigns/{cid}/duplicate"), None).await;
    assert!(!copy["content"].to_string().contains("frozen") && !copy["ab_test"].to_string().contains("frozen"), "{copy}");
    // Detaching copies what the email shows, also after the saved block was deleted.
    h.ok("DELETE", &format!("/api/content/saved-blocks/{id}?confirm=true"), None).await;
    let shown = h.ok("GET", &format!("/api/content/saved-blocks/{id}?include_deleted=true"), None).await;
    assert_eq!(shown["deleted"], true);
    assert_eq!(shown["blocks"][0]["html"], "{{ unclosed");
    h.cleanup().await;
}

#[tokio::test]
async fn personalized_links_are_checked_after_personalization() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let p = profile(&h, acct, "eve@x.test", "subscribed", Some("Eve"), json!({"site": "javascript:alert(document.cookie)", "pic": "data:image/png;base64,AAAA", "good": "https://eve.test/{{x}}"})).await;
    let content = json!({"subject": "s", "blocks": [
        {"type": "image_text", "src": "{{ person.properties.pic }}", "alt": "Pic", "heading": "Hello", "href": "{{ person.properties.site }}", "button_text": "Go", "button_href": "{{ person.properties.site }}"},
        {"type": "nav", "links": [{"text": "Mine", "href": "{{ person.properties.site }}"}, {"text": "Good", "href": "{{ person.properties.good }}"}]},
        {"type": "video", "url": "{{ person.properties.site }}", "thumbnail": "{{ person.properties.good }}", "alt": "Watch"},
    ]});
    let v = preview(&mut h, content, Some(p)).await;
    let html = v["html"].as_str().unwrap();
    let text = v["text"].as_str().unwrap();
    for bad in ["javascript", "data:image"] {
        assert!(!html.contains(bad) && !text.contains(bad), "{bad} survived:\n{html}\n{text}");
    }
    assert!(html.contains("Hello") && html.contains(">Mine<") && html.contains("Watch"), "the blocks still show, without the link");
    assert!(html.contains(r#"href="https://eve.test/%7B%7Bx%7D%7D""#), "an allowed link is kept, and can't open a template tag: {html}");
    h.cleanup().await;
}

#[tokio::test]
async fn link_checks_never_request_dumbmail_links() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    // Another account's branded link domain counts too.
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "two@b.test", "password": "correct horse", "company": "Two"}))).await;
    let other = account_id(&h).await;
    sqlx::query("UPDATE accounts SET link_domain = 'links.two.test', link_domain_status = 'active' WHERE id = $1").bind(other).execute(&h.state.db).await.unwrap();
    h.cookie = None;
    h.ok("POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "correct horse"}))).await;

    // A real email's pixel and tracked link, copied onto link domains.
    let p = profile(&h, acct, "ann@x.test", "subscribed", Some("Ann"), json!({})).await;
    let list = list_of(&mut h, &[p]).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Real", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"subject": "s", "blocks": [{"type": "button", "text": "Go", "href": "https://shop.example.com/x"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let m = h.state.email.sent().into_iter().find(|m| m.to == "ann@x.test").unwrap();
    let pixel = find_link(&m.html, "https://pl.test/t/o/").replace("https://pl.test", "https://links.two.test");
    let click = find_link(&m.html, "https://pl.test/t/c/").replace("https://pl.test", "https://old.pl.test");
    let unsub = find_link(&m.html, "https://pl.test/u/");
    // A tracked link that leads to another account's link domain.
    let wrapped = format!("https://pl.test/t/c/x?u={}&s=1", url::form_urlencoded::byte_serialize(pixel.as_bytes()).collect::<String>());
    let content = json!({"subject": "Copied", "preheader": "p", "blocks": [
        {"type": "image", "src": pixel, "alt": "x"},
        {"type": "text", "html": format!("<p><a href=\"{click}\">again</a> <a href=\"{unsub}\">unsub</a> <a href=\"{}\">wrapped</a></p>", wrapped.replace('&', "&amp;"))},
    ]});
    let r = h.ok("POST", "/api/content/check", Some(json!({"content": content}))).await;
    let links = r["links"].as_array().unwrap();
    for u in [&pixel, &unsub] {
        let l = links.iter().find(|l| l["url"] == u.as_str()).unwrap_or_else(|| panic!("{u} in {links:?}"));
        assert_eq!(l["note"], "DumbMail link, not checked");
        assert!(l["status"].is_null());
    }
    assert!(links.iter().any(|l| l["url"] == "https://shop.example.com/x"), "the tracked link is checked as where it leads: {links:?}");
    assert!(!links.iter().any(|l| l["url"] == click.as_str()));
    assert!(links.iter().any(|l| l["url"] == wrapped.as_str() && l["note"] == "DumbMail link, not checked"), "a tracked link to another account's link domain isn't requested: {links:?}");
    let (opened, clicked): (Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT opened_at, clicked_at FROM messages WHERE profile_id = $1").bind(p).fetch_one(&h.state.db).await.unwrap();
    assert!(opened.is_none() && clicked.is_none(), "the check recorded no open or click");
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE profile_id = $1").bind(p).fetch_one(&h.state.db).await.unwrap_or(0);
    let received: i64 = sqlx::query_scalar("SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.profile_id = $1 AND m.name IN ('Opened Email', 'Clicked Email')").bind(p).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(received, 0, "{events} events, none of them opens or clicks");
    h.cleanup().await;
}

#[tokio::test]
async fn hosted_images_load_on_every_dumbmail_host() {
    let h = harness!();
    sqlx::query("INSERT INTO accounts (id, public_key, name, link_domain, link_domain_status) VALUES ($1, 'pk_links', 'L', 'links.acme.test', 'active')")
        .bind(Uuid::now_v7())
        .execute(&h.state.db)
        .await
        .unwrap();
    for host in ["pl.test", "old.pl.test", "links.acme.test"] {
        let (s, headers, _) = h.raw("GET", "/e/icons/color-instagram.png", &[("host", host)], "").await;
        assert_eq!(s, StatusCode::OK, "{host}: {headers:?}");
        assert_eq!(headers["content-type"], "image/png");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn starters_take_the_website_as_data_and_pre_arrival_needs_a_paid_booking() {
    let mut h = harness!();
    h.signup().await;
    h.ok("PATCH", "/api/account", Some(json!({"website": "https://acme.test/o'brien's/"}))).await;
    let all = h.ok("GET", "/api/content/starters?preview=true", None).await;
    assert!(all["data"].as_array().unwrap().iter().all(|s| s["html"].as_str().is_some_and(|x| x.len() > 1000)), "every starter renders");
    let bis = all["data"].as_array().unwrap().iter().find(|s| s["key"] == "back_in_stock").unwrap();
    let html = bis["html"].as_str().unwrap();
    assert!(html.contains("acme.test&#x2f;o&#x27;brien&#x27;s&#x2f;"), "the website is the fallback link");

    let pre = h.ok("GET", "/api/content/starters/pre_arrival", None).await;
    let content = pre["content"].clone();
    let render = |event: Value| json!({"channel": "email", "content": content.clone(), "event": event});
    let paid = h.ok("POST", "/api/render", Some(render(json!({"properties": {"address": "123 Ocean View Rd", "paid_in_full": true}, "from_browser": false})))).await;
    assert!(paid["html"].as_str().unwrap().contains("123 Ocean View Rd"));
    for unpaid in [
        json!({"properties": {"address": "123 Ocean View Rd"}, "from_browser": false}),
        json!({"properties": {"address": "123 Ocean View Rd", "paid_in_full": false}, "from_browser": false}),
        // Paid, but recorded by a browser (anyone can send those), or not known to come from the server.
        json!({"properties": {"address": "123 Ocean View Rd", "paid_in_full": true}, "from_browser": true}),
        json!({"properties": {"address": "123 Ocean View Rd", "paid_in_full": true}}),
    ] {
        let v = h.ok("POST", "/api/render", Some(render(unpaid))).await;
        let html = v["html"].as_str().unwrap();
        assert!(!html.contains("123 Ocean View Rd") && html.contains("once your booking is paid in full"), "{html}");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn transactional_email_refuses_what_it_cant_render() {
    let mut h = harness!();
    h.signup().await;
    h.ok("PATCH", "/api/transactional/settings", Some(json!({"enabled": true, "acknowledge": true}))).await;
    for (blocks, says) in [
        (json!([{"type": "text", "html": "<p>x</p>", "show_if": {"field": "first_name", "op": "set"}}]), "display rules"),
        (json!([{"type": "saved", "saved_block_id": Uuid::now_v7(), "blocks": []}]), "saved blocks"),
        (json!([{"type": "social"}]), "social block"),
    ] {
        let (s, e) = h.req("POST", "/api/transactional/templates", Some(json!({"name": "R", "content": {"subject": "Receipt", "blocks": blocks}}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["error"]["message"].as_str().unwrap().contains(says), "{e}");
    }
    // A table (order lines) is fine.
    h.ok("POST", "/api/transactional/templates", Some(json!({"name": "R", "content": {"subject": "Receipt", "blocks": [{"type": "table", "columns": [{"label": "Item"}], "rows": [["Tote"]]}]}}))).await;
    h.cleanup().await;
}

#[tokio::test]
async fn heavy_image_lookup_stays_in_the_account() {
    let mut h = harness!();
    h.signup().await;
    h.cookie = None;
    h.ok("POST", "/api/auth/signup", Some(json!({"email": "two@b.test", "password": "correct horse", "company": "Two"}))).await;
    let other = account_id(&h).await;
    let img = Uuid::now_v7();
    sqlx::query("INSERT INTO images (id, account_id, content_type, bytes, size, sha256) VALUES ($1, $2, 'image/png', '\\x00', 3000000, 'x')")
        .bind(img)
        .bind(other)
        .execute(&h.state.db)
        .await
        .unwrap();
    h.cookie = None;
    h.ok("POST", "/api/auth/login", Some(json!({"email": "owner@acme.test", "password": "correct horse"}))).await;
    let r = h.ok("POST", "/api/content/check", Some(json!({"content": {"subject": "s", "preheader": "p", "blocks": [{"type": "image", "src": format!("https://pl.test/i/{img}"), "alt": "x"}]}}))).await;
    assert!(!r["findings"].as_array().unwrap().iter().any(|f| f["id"] == "heavy_images"), "another account's image says nothing: {r}");
    h.cleanup().await;
}

#[tokio::test]
async fn permissions_follow_the_table() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    profile(&h, acct, "ann@x.test", "subscribed", None, json!({})).await;
    // Deleting needs confirm=true; without it nothing changes.
    let sb = h.ok("POST", "/api/content/saved-blocks", Some(json!({"name": "F", "blocks": [{"type": "divider"}]}))).await;
    let id = sb["id"].as_str().unwrap();
    let dry = h.ok("DELETE", &format!("/api/content/saved-blocks/{id}"), None).await;
    assert_eq!(dry["dry_run"], true, "{dry}");
    h.ok("GET", &format!("/api/content/saved-blocks/{id}"), None).await;
    // A key that may edit content but not read profiles gets the checks without people's data.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "writer", "scopes": ["content"]}))).await["key"].as_str().unwrap().to_string();
    let body = json!({"content": {"subject": "Hi {{ first_name }}", "preheader": "p", "blocks": [{"type": "text", "html": "<p>Hi {{ first_name }}</p>"}]}, "skip_links": true}).to_string();
    let auth = format!("Bearer {key}");
    let (s, _, out) = h.raw("POST", "/api/content/check", &[("authorization", &auth), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::OK, "{out}");
    let r: Value = serde_json::from_str(&out).unwrap();
    let ids: Vec<&str> = r["findings"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"people_not_checked") && !ids.contains(&"personalization_empty") && r["recipients"].is_null(), "{r}");
    // Checking a saved campaign reads it: that needs messaging.read too.
    let camp = h.ok("POST", "/api/campaigns", Some(json!({"name": "C", "content": {"subject": "s", "blocks": [{"type": "text", "html": "<p>secret plan</p>"}]}}))).await;
    let (s, _, out) = h.raw("POST", "/api/content/check", &[("authorization", &auth), ("content-type", "application/json")], &json!({"campaign_id": camp["id"], "skip_links": true}).to_string()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{out}");
    // The owner gets the counts.
    let r = h.ok("POST", "/api/content/check", Some(serde_json::from_str(&body).unwrap())).await;
    assert_eq!(r["recipients"], 1, "{r}");
    // A key that may only read content can't run checks or change saved blocks.
    let reader = h.ok("POST", "/api/api-keys", Some(json!({"name": "reader", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let auth = format!("Bearer {reader}");
    let (s, _, _) = h.raw("POST", "/api/content/check", &[("authorization", &auth), ("content-type", "application/json")], &body).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = h.raw("GET", "/api/content/saved-blocks", &[("authorization", &auth)], "").await;
    assert_eq!(s, StatusCode::OK);
    // Hosted images need no sign-in.
    let (s, _, _) = h.raw("GET", "/e/assets/placeholder-hero.png", &[], "").await;
    assert_eq!(s, StatusCode::OK);
    h.cleanup().await;
}

// ---------- Working with unique coupon codes (PR #20) ----------

async fn coupon(h: &mut Harness, name: &str, codes: &[&str]) -> String {
    let c = h.ok("POST", "/api/coupons", Some(json!({"name": name}))).await;
    let id = c["id"].as_str().unwrap().to_string();
    if !codes.is_empty() {
        h.ok("POST", &format!("/api/coupons/{id}/codes"), Some(json!({"codes": codes}))).await;
    }
    id
}

/// Codes from coupon `name` given to `profile`.
async fn codes_of(h: &Harness, name: &str, profile: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT cc.code FROM coupon_codes cc JOIN coupons c ON c.id = cc.coupon_id WHERE c.name = $1 AND cc.profile_id = $2 ORDER BY cc.code")
        .bind(name)
        .bind(profile)
        .fetch_all(&h.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn coupon_codes_go_only_to_what_each_person_sees() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    let gold = profile(&h, acct, "gold@x.test", "subscribed", Some("Goldie"), json!({"tier": "gold"})).await;
    let silver = profile(&h, acct, "silver@x.test", "subscribed", Some("Sil"), json!({"tier": "silver"})).await;
    let none = profile(&h, acct, "none@x.test", "subscribed", Some("Nan"), json!({})).await;
    let list = list_of(&mut h, &[gold, silver, none]).await;
    coupon(&mut h, "VIP", &["VIP-0001", "VIP-0002", "VIP-0003"]).await;
    coupon(&mut h, "TEAM", &["TEAM-0001", "TEAM-0002", "TEAM-0003"]).await;

    // The TEAM coupon is only in a saved block (its stored copy is stale and has no coupon), in its
    // text and in a button link.
    let (s, sb) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Offer", "blocks": [
        {"type": "text", "html": "<p>Team code {% coupon_code 'TEAM' %}</p>"},
        {"type": "image_text", "src": "https://img.example/a.png", "alt": "Tote", "heading": "Tote", "html": "<p>Ours</p>", "button_text": "Use it", "button_href": "https://shop.example/discount/{% coupon_code 'TEAM' %}"},
    ]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{sb}");
    let (s, bad) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Bad", "blocks": [{"type": "text", "html": "{% coupon_code name %}"}]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "saved blocks follow the coupon tag rules: {bad}");
    let member = json!({"field": "person.properties.tier", "op": "set"});
    let content = json!({"subject": "Offers", "preheader": "p", "blocks": [
        {"type": "heading", "text": "Hi {{ first_name }}", "show_if": member},
        {"type": "coupon", "coupon": "VIP", "title": "Gold code", "show_if": {"field": "person.properties.tier", "op": "eq", "value": "gold"}},
        {"type": "saved", "saved_block_id": sb["id"], "name": "Offer", "show_if": member, "blocks": [{"type": "text", "html": "<p>Stale copy</p>"}]},
    ]});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Offers", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
    let cid = c["id"].as_str().unwrap().to_string();

    // The check panel sees the coupon inside the saved block, through the coupons module's own check.
    let r = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": cid, "skip_links": true}))).await;
    assert!(!r["findings"].as_array().unwrap().iter().any(|f| f["id"].as_str().unwrap().starts_with("coupon_") || f["id"] == "render_error"), "{r}");
    let missing = json!({"subject": "x", "blocks": [{"type": "saved", "saved_block_id": sb["id"], "blocks": []}, {"type": "coupon", "coupon": "NOPE"}]});
    let r = h.ok("POST", "/api/content/check", Some(json!({"content": missing, "skip_links": true}))).await;
    let f = r["findings"].as_array().unwrap().iter().find(|f| f["id"] == "coupon_problem").unwrap_or_else(|| panic!("{r}"));
    assert!(f["title"].as_str().unwrap().contains("no coupon named \"nope\"") && f["blocks_sending"] == true, "{f}");
    coupon(&mut h, "SHORT", &["SHORT-0001"]).await;
    let short = json!({"subject": "x", "blocks": [{"type": "text", "html": "{% coupon_code 'SHORT' %}"}]});
    let r = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": cid, "content": short, "skip_links": true}))).await;
    assert!(r["findings"].as_array().unwrap().iter().any(|f| f["id"] == "coupon_short" && f["title"].as_str().unwrap().contains("has 1 codes left")), "{r}");

    // Previews show a sample code and use none.
    let v = preview(&mut h, content.clone(), Some(gold)).await;
    assert!(v["html"].as_str().unwrap().contains("SAMPLE-CODE"), "{v}");
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE profile_id IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(used, 0);

    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    assert_eq!(h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["status"], "sent");
    let sent = h.state.email.sent();

    // Gold sees both coupons and gets a code from each.
    let (g_vip, g_team) = (codes_of(&h, "VIP", gold).await, codes_of(&h, "TEAM", gold).await);
    assert_eq!((g_vip.len(), g_team.len()), (1, 1));
    let to_gold = sent.iter().find(|m| m.to == "gold@x.test").unwrap();
    assert!(to_gold.html.contains(&g_vip[0]) && to_gold.html.contains(&format!("Team code {}", g_team[0])));
    assert!(to_gold.text.contains(&format!("Use it: https://shop.example/discount/{}", g_team[0])), "the code is in the saved block's button link: {}", to_gold.text);
    assert!(!to_gold.html.contains("Stale copy"));

    // Silver's VIP block is hidden: no VIP code used for them, only TEAM.
    assert!(codes_of(&h, "VIP", silver).await.is_empty(), "a hidden coupon block uses no code");
    let s_team = codes_of(&h, "TEAM", silver).await;
    assert_eq!(s_team.len(), 1);
    let to_silver = sent.iter().find(|m| m.to == "silver@x.test").unwrap();
    assert!(to_silver.html.contains(&format!("Team code {}", s_team[0])) && !to_silver.html.contains("Gold code"));

    // Everything is hidden for the third person: skipped, and no code used at all.
    assert!(!sent.iter().any(|m| m.to == "none@x.test"));
    let why: (String, String) = sqlx::query_as("SELECT status, error FROM messages WHERE profile_id = $1").bind(none).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(why, ("skipped".into(), "nothing to show for this person".into()));
    assert!(codes_of(&h, "VIP", none).await.is_empty() && codes_of(&h, "TEAM", none).await.is_empty(), "a skipped person claims nothing");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE status = 'available' AND code LIKE 'VIP-%'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn a_coupon_tag_in_a_persons_data_never_becomes_a_tag() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    let acct = account_id(&h).await;
    let p = profile(&h, acct, "sly@x.test", "subscribed", Some("Sly"), json!({"site": "https://shop.example/{% coupon_code 'VIP' %}"})).await;
    let list = list_of(&mut h, &[p]).await;
    coupon(&mut h, "VIP", &["VIP-0001"]).await;
    let content = json!({"subject": "Hi", "preheader": "p", "blocks": [
        {"type": "image_text", "src": "https://img.example/a.png", "alt": "A", "heading": "Your site", "html": "<p>x</p>", "button_text": "Visit", "button_href": "{{ person.properties.site }}"},
    ]});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Sly", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let m = h.state.email.sent().into_iter().find(|m| m.to == "sly@x.test").expect("sent");
    assert!(!m.html.contains("VIP-0001") && !m.text.contains("VIP-0001"));
    assert!(codes_of(&h, "VIP", p).await.is_empty(), "no code is claimed from a person's own data");
    h.cleanup().await;
}

#[tokio::test]
async fn the_check_takes_its_account_slot_before_any_audience_work() {
    let mut h = harness!();
    h.signup().await;
    let acct = account_id(&h).await;
    let p = profile(&h, acct, "a@x.test", "subscribed", Some("A"), json!({})).await;
    let list = list_of(&mut h, &[p]).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "C", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hi {{ first_name }}</p>"}]}}))).await;
    let body = json!({"campaign_id": c["id"], "skip_links": true}).to_string();
    let mut reqs = Vec::new();
    for _ in 0..2 {
        let b = h.page_headers("POST", axum::http::Request::builder().method("POST").uri("/api/content/check").header("content-type", "application/json")).await;
        reqs.push(b.body(axum::body::Body::from(body.clone())).unwrap());
    }
    let router = h.router.clone();
    let request = move |req: axum::http::Request<axum::body::Body>| {
        let router = router.clone();
        async move { tower::ServiceExt::oneshot(router, req).await.unwrap().status() }
    };
    // Hold the people table so any audience count waits.
    let mut tx = h.state.db.begin().await.unwrap();
    sqlx::query("LOCK TABLE profiles IN ACCESS EXCLUSIVE MODE").execute(&mut *tx).await.unwrap();
    let first = tokio::spawn(request(reqs.remove(0)));
    // The first check is now waiting on the audience count, holding the account's slot.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!first.is_finished());
    // A second check is refused at once: it never reached the audience count, or it would wait too.
    let second = tokio::time::timeout(std::time::Duration::from_secs(3), request(reqs.remove(0))).await.expect("refused without waiting on the audience");
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
    tx.rollback().await.unwrap();
    assert_eq!(first.await.unwrap(), StatusCode::OK);
    h.cleanup().await;
}

#[tokio::test]
async fn hosted_images_are_served_on_the_marketing_site_host() {
    let Some(h) = Harness::with(|c| {
        c.public_url = "https://app.pl.test".into();
        c.site_url = Some("https://pl.test".into());
    })
    .await
    else {
        return;
    };
    for host in ["pl.test", "app.pl.test"] {
        let (s, headers, _) = h.raw("GET", "/e/icons/color-instagram.png", &[("host", host)], "").await;
        assert_eq!(s, StatusCode::OK, "{host}: served directly, not redirected: {headers:?}");
        assert_eq!(headers["content-type"], "image/png");
    }
    h.cleanup().await;
}

#[tokio::test]
async fn flow_emails_know_whether_a_browser_recorded_the_event() {
    let mut h = harness!();
    let me = h.signup().await;
    h.credit(5.0).await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let acct = account_id(&h).await;
    profile(&h, acct, "server@x.test", "subscribed", Some("Sam"), json!({})).await;
    profile(&h, acct, "browser@x.test", "subscribed", Some("Bea"), json!({})).await;
    let pre = h.ok("GET", "/api/content/starters/pre_arrival", None).await;
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Arrival", "trigger": {"type": "metric", "metric": "Booking Paid"}, "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "content": pre["content"], "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let props = json!({"address": "123 Ocean View Rd", "paid_in_full": true});
    h.ok("POST", "/api/events", Some(json!({"metric": "Booking Paid", "profile": {"email": "server@x.test"}, "properties": props}))).await;
    let (s, _, b) = h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Booking Paid", "profile": {"email": "browser@x.test"}, "properties": props}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    h.drain().await;
    h.fast_forward().await;
    h.drain().await;
    let sent = h.state.email.sent();
    let server = sent.iter().find(|m| m.to == "server@x.test").expect("server event's email");
    assert!(server.html.contains("123 Ocean View Rd"), "a paid booking from the server shows the address");
    let browser = sent.iter().find(|m| m.to == "browser@x.test").expect("browser event's email");
    assert!(!browser.html.contains("123 Ocean View Rd") && browser.html.contains("once your booking is paid in full"), "a browser can't unlock arrival details");
    h.cleanup().await;
}

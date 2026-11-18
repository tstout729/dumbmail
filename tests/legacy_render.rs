//! Email that uses nothing from the content module renders byte for byte as it did before it.
//!
//! `tests/fixtures/legacy_render/inputs.json` holds legacy emails (five columns, more than 300
//! blocks, raw HTML, personalization, markers content can't place, ...). `expected.json` is what
//! `origin/main` produced for them before the content module, made by running this same file there
//! with `LEGACY_RENDER_WRITE=<path>`: the pure renderer (marketing and transactional options) and a
//! real campaign send through `sending::send`. This test renders them again and compares. The
//! coupon cases (an email whose only extra is a coupon tag or coupon block) were made on `origin/main`
//! with unique coupon codes (35d7abc); the others are unchanged by it.

#[macro_use]
mod common;

use dumbmail::render::{render_email, render_email_with, EmailContent, EmailOptions, Footer, Tracking};
use regex::Regex;
use serde_json::{json, Value};
use uuid::Uuid;

use common::*;

const INPUTS: &str = include_str!("fixtures/legacy_render/inputs.json");
const EXPECTED: &str = include_str!("fixtures/legacy_render/expected.json");

/// Message-specific tokens in a real send (ids, signatures) replaced by placeholders.
fn normalize(html: &str) -> String {
    let tokens = Regex::new(r"/(t/o|t/c|u)/[A-Za-z0-9._~%-]+").unwrap();
    let sigs = Regex::new(r"([?&](amp;)?s=)[0-9A-Za-z_-]+").unwrap();
    let html = tokens.replace_all(html, "/$1/TOKEN");
    sigs.replace_all(&html, "${1}SIG").into_owned()
}

async fn render_all(h: &mut Harness) -> Value {
    h.signup().await;
    h.credit(20.0).await;
    let acct: Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let account = dumbmail::campaigns::load_account(&h.state.db, acct).await.unwrap();
    sqlx::query("INSERT INTO profiles (id, account_id, email, email_consent, first_name, properties) VALUES ($1, $2, 'legacy@x.test', 'subscribed', 'Lee', '{\"city\": \"Bend\"}')")
        .bind(Uuid::now_v7())
        .bind(acct)
        .execute(&h.state.db)
        .await
        .unwrap();
    let pid: Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'legacy@x.test'").fetch_one(&h.state.db).await.unwrap();
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "Legacy"}))).await;
    h.ok("POST", &format!("/api/lists/{}/members", list["id"].as_str().unwrap()), Some(json!({"profile_ids": [pid]}))).await;

    // Emails that use only a coupon (PR #20) take the legacy path too: one code, so every send gets it.
    let coupon = h.ok("POST", "/api/coupons", Some(json!({"name": "LEGACY"}))).await;
    h.ok("POST", &format!("/api/coupons/{}/codes", coupon["id"].as_str().unwrap()), Some(json!({"codes": ["LEGACY-0001"]}))).await;

    let ctx = json!({"first_name": "Lee", "person": {"first_name": "Lee", "properties": {"city": "Bend"}}, "organization": {"name": "Acme Goods"}, "unsubscribe_url": "https://links.test/u/x",
        "__dm_coupons": {"codes": {"legacy": {"code": "LEGACY-0001", "expires": ""}}}});
    let tracking = Tracking { public_url: "https://links.test", secret: "fixture-secret", message_id: Uuid::nil(), unsubscribe_url: "https://links.test/u/x".into() };
    let inputs: Vec<Value> = serde_json::from_str(INPUTS).unwrap();
    let mut out = serde_json::Map::new();
    for input in inputs {
        let name = input["name"].as_str().unwrap().to_string();
        let content = input["content"].clone();
        let ec: EmailContent = serde_json::from_value(content.clone()).unwrap();
        let marketing = render_email(&account, &ec, &ctx, Some(&tracking)).unwrap();
        let preview = render_email(&account, &ec, &ctx, None).unwrap();
        let transactional = render_email_with(&account, &ec, &ctx, Some(&tracking), EmailOptions { footer: Footer::None, track_clicks: false, track_opens: false }).unwrap();

        let before = h.state.email.sent().len();
        let c = h.ok("POST", "/api/campaigns", Some(json!({"name": name, "channel": "email", "audience": {"include": [{"type": "list", "id": list["id"]}]}, "content": content}))).await;
        h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
        h.drain().await;
        let sent = h.state.email.sent();
        assert_eq!(sent.len(), before + 1, "{name}: one email sent");
        let m = &sent[before];
        out.insert(
            name,
            json!({
                "marketing": {"subject": marketing.subject, "preheader": marketing.preheader, "html": marketing.html, "text": marketing.text},
                "preview": {"html": preview.html, "text": preview.text},
                "transactional": {"html": transactional.html, "text": transactional.text},
                "sent": {"subject": m.subject, "html": normalize(&m.html), "text": normalize(&m.text)},
            }),
        );
    }
    Value::Object(out)
}

#[tokio::test]
async fn legacy_email_renders_exactly_as_before_the_content_module() {
    let mut h = harness!();
    let got = render_all(&mut h).await;
    if let Ok(path) = std::env::var("LEGACY_RENDER_WRITE") {
        std::fs::write(path, serde_json::to_string_pretty(&got).unwrap()).unwrap();
        h.cleanup().await;
        return;
    }
    let want: Value = serde_json::from_str(EXPECTED).unwrap();
    let want = want.as_object().unwrap();
    assert_eq!(got.as_object().unwrap().len(), want.len());
    for (name, w) in want {
        for part in ["marketing", "preview", "transactional", "sent"] {
            for field in ["subject", "preheader", "html", "text"] {
                assert_eq!(got[name][part][field], w[part][field], "{name}: {part} {field} differs from main");
            }
        }
    }
    // The cases that changed on the first version of this module are really in there.
    assert!(want["five_columns"]["sent"]["html"].as_str().unwrap().contains("Col 5"));
    assert!(want["over_300_blocks"]["sent"]["html"].as_str().unwrap().contains("B327"));
    assert!(want["coupon_tag"]["sent"]["html"].as_str().unwrap().contains("use LEGACY-0001 at checkout"));
    assert!(want["coupon_block"]["sent"]["text"].as_str().unwrap().contains("Your code: LEGACY-0001"));
    h.cleanup().await;
}

#[test]
fn legacy_inputs_use_nothing_from_the_content_module() {
    let inputs: Vec<Value> = serde_json::from_str(INPUTS).unwrap();
    for i in inputs {
        assert!(!dumbmail::features::content::prepare::uses_content_features(&i["content"]), "{}", i["name"]);
    }
}

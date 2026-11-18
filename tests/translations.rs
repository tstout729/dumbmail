//! Messages in each person's language: profile languages, language versions of campaigns, flow
//! steps, templates and saved blocks, checks when saved and before sending, XLIFF/CSV, machine
//! drafts against a stand-in for the Anthropic API, hosted pages and system texts, right-to-left
//! email, permissions and account isolation.

#[macro_use]
mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use common::*;

async fn account_id(h: &Harness) -> Uuid {
    sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap()
}

async fn person(h: &mut Harness, email: &str, locale: Option<&str>) -> Uuid {
    let mut body = json!({"email": email, "email_consent": "subscribed"});
    if let Some(l) = locale {
        body["locale"] = json!(l);
    }
    let p = h.ok("POST", "/api/profiles", Some(body)).await;
    p["id"].as_str().or(p["profile"]["id"].as_str()).unwrap().parse().unwrap()
}

async fn list_of(h: &mut Harness, ids: &[Uuid]) -> String {
    let list = h.ok("POST", "/api/lists", Some(json!({"name": format!("L{}", Uuid::now_v7().simple())}))).await;
    let id = list["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/lists/{id}/members"), Some(json!({"profile_ids": ids}))).await;
    id
}

fn email() -> Value {
    json!({"subject": "Hello {{ first_name|default('there') }}", "preheader": "New arrivals",
        "blocks": [
            {"type": "heading", "text": "Fresh picks"},
            {"type": "text", "html": "<p>Hi <strong>{{ first_name|default('there') }}</strong>, take a look.</p>", "align": "left"},
            {"type": "columns", "columns": [[{"type": "button", "text": "Shop now", "href": "https://acme.test/en/shop"}], [{"type": "image", "src": "https://acme.test/a.png", "alt": "A bag"}]]}
        ]})
}

async fn set_languages(h: &mut Harness, langs: Value) {
    h.ok("PUT", "/api/translations/settings", Some(json!({"default_language": "en", "languages": langs}))).await;
}

fn french() -> Value {
    json!([
        {"id": "subject", "text": "Bonjour {{ first_name|default('there') }}"},
        {"id": "b0:text", "text": "Nouveautés"},
        {"id": "b1:html", "text": "<p>Salut <strong>{{ first_name|default('there') }}</strong>, regardez.</p>"},
        {"id": "b2.c0.0:text", "text": "Acheter"},
        {"id": "b2.c0.0:href", "text": "https://acme.test/fr/shop"}
    ])
}

#[tokio::test]
async fn each_person_gets_their_language_and_everyone_else_the_default() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    set_languages(&mut h, json!(["fr", "es"])).await;
    let ann = person(&mut h, "ann@x.test", Some("fr-CA")).await;
    let bob = person(&mut h, "bob@x.test", Some("de")).await;
    let cat = person(&mut h, "cat@x.test", None).await;
    let list = list_of(&mut h, &[ann, bob, cat]).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Picks", "audience": {"include": [{"type": "list", "id": list}]}, "content": email()}))).await;
    let cid = c["id"].as_str().unwrap().to_string();

    // A language that isn't one of the account's is refused; then French is saved.
    let (s, e) = h.req("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "it", "strings": [{"id": "subject", "text": "Ciao"}]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let v = h.ok("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": french()}))).await;
    assert_eq!(v["summary"]["fr"]["reviewed"], 5, "four texts and the localized link: {v}");
    assert_eq!(v["summary"]["fr"]["missing"], 2, "preview text and alt text: {v}");
    let link = v["strings"].as_array().unwrap().iter().find(|s| s["id"] == "b2.c0.0:href").unwrap().clone();
    assert_eq!(link["status"], "reviewed");

    // The check before sending says what's missing in French; it doesn't stop the send.
    let chk = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": cid, "skip_links": true}))).await;
    let ids: Vec<&str> = chk["findings"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"translation_missing"), "{chk}");
    assert!(!chk["findings"].as_array().unwrap().iter().any(|f| f["id"].as_str().unwrap().starts_with("translation") && f["blocks_sending"] == true));

    h.ok("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    h.drain().await;
    let sent = h.state.email.sent();
    let to = |a: &str| sent.iter().find(|m| m.to == a).cloned().unwrap_or_else(|| panic!("nothing to {a}"));
    let (a, b, c2) = (to("ann@x.test"), to("bob@x.test"), to("cat@x.test"));
    assert_eq!(a.subject, "Bonjour there");
    assert!(a.html.contains("Nouveautés") && a.html.contains("Acheter") && a.html.contains("regardez"), "{}", a.html);
    assert!(a.html.contains(r#"<html lang="fr">"#));
    assert!(a.html.contains("Vous recevez cet e-mail parce que vous vous êtes inscrit(e).") && a.html.contains(">Se désabonner</a>"), "French footer");
    assert!(a.text.contains("Se désabonner: "), "{}", a.text);
    assert!(a.html.contains("fr%2Fshop") && !a.html.contains("en%2Fshop"), "the localized link, tracked");
    assert!(a.html.contains("A bag"), "untranslated alt text falls back to English");
    assert!(a.html.contains("/u/"), "the unsubscribe link is still there");
    for m in [&b, &c2] {
        assert_eq!(m.subject, "Hello there");
        assert!(m.html.contains("Fresh picks") && m.html.contains(r#"<html lang="en">"#) && m.html.contains(">Unsubscribe</a>"));
        assert!(m.html.contains("en%2Fshop"));
    }
    // The chosen language is kept on each message, and results break down by it.
    let langs: Vec<(String, Option<String>)> = sqlx::query_as("SELECT to_address, language FROM messages WHERE campaign_id = $1::uuid ORDER BY to_address").bind(&cid).fetch_all(&h.state.db).await.unwrap();
    assert_eq!(langs, vec![("ann@x.test".into(), Some("fr".into())), ("bob@x.test".into(), Some("en".into())), ("cat@x.test".into(), Some("en".into()))]);
    let rep = h.ok("GET", &format!("/api/translations/report?campaign_id={cid}"), None).await;
    let fr = rep["data"].as_array().unwrap().iter().find(|r| r["language"] == "fr").unwrap();
    assert_eq!(fr["sent"], 1);

    // Sent campaigns can't have their translations changed.
    let (s, _) = h.req("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": [{"id": "subject", "text": "Salut"}]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.cleanup().await;
}

#[tokio::test]
async fn translations_follow_edits_and_are_checked_when_saved() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["fr"])).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Draft", "content": email()}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    h.ok("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": french()}))).await;

    // A translation must keep the original's template tags, and they must work.
    for bad in ["Bonjour", "Bonjour {{ person.properties.secret }}", "Bonjour {{ first_name|default('there') }} {% if %}"] {
        let (s, e) = h.req("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": [{"id": "subject", "text": bad}]}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {e}");
    }
    let (s, e) = h.req("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": [{"id": "b2.c0.0:href", "text": "javascript:alert(1)"}]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    // The same rules apply to content saved through the campaign API itself.
    let mut raw = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["content"].clone();
    raw["translations"]["fields"]["fr"]["subject"]["text"] = json!("Bonjour sans balise");
    let (s, e) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"content": raw}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");

    // Editing the default heading makes its French stale; the default is sent for it.
    let mut content = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["content"].clone();
    content["blocks"][0]["text"] = json!("Fresh picks for winter");
    h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"content": content.clone()}))).await;
    let v = h.ok("GET", &format!("/api/translations/campaigns/{cid}?language=fr"), None).await;
    let st = |id: &str| v["strings"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap()["status"].clone();
    assert_eq!(st("b0:text"), "stale");
    assert_eq!(st("subject"), "reviewed");
    let p = h.ok("POST", "/api/translations/preview", Some(json!({"channel": "email", "content": content, "language": "fr"}))).await;
    assert!(p["html"].as_str().unwrap().contains("Fresh picks for winter") && p["html"].as_str().unwrap().contains("Acheter"));
    assert_eq!(p["subject"], "Bonjour Alex");
    let chk = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": cid, "skip_links": true}))).await;
    assert!(chk["findings"].as_array().unwrap().iter().any(|f| f["id"] == "translation_stale"), "{chk}");

    // Removing the language takes every French translation off.
    let v = h.ok("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "remove_language": true}))).await;
    assert_eq!(v["languages"], json!([]));
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert!(!c["content"].to_string().contains("Acheter"));
    h.cleanup().await;
}

#[tokio::test]
async fn ab_versions_flow_steps_templates_and_saved_blocks_have_their_own_translations() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    set_languages(&mut h, json!(["es"])).await;
    let ana = person(&mut h, "ana@x.test", Some("es-MX")).await;
    let _ = ana;
    // A saved block with Spanish, used in a template.
    let (s, sb) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Sign-off", "blocks": [{"type": "text", "html": "<p>See you soon</p>"}]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{sb}");
    let sbid = sb["id"].as_str().unwrap();
    h.ok("PUT", &format!("/api/translations/saved-blocks/{sbid}"), Some(json!({"language": "es", "strings": [{"id": "b0:html", "text": "<p>Hasta pronto</p>"}]}))).await;
    let content = json!({"subject": "Hi", "blocks": [{"type": "heading", "text": "News"}, {"type": "saved", "saved_block_id": sbid}]});
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "T", "channel": "email", "content": content}))).await;
    let tid = t["id"].as_str().unwrap();
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(json!({"language": "es", "strings": [{"id": "subject", "text": "Hola"}, {"id": "b0:text", "text": "Noticias"}]}))).await;
    let tpl = h.ok("GET", &format!("/api/templates/{tid}"), None).await;
    let p = h.ok("POST", "/api/translations/preview", Some(json!({"channel": "email", "content": tpl["content"], "language": "es"}))).await;
    let html = p["html"].as_str().unwrap();
    assert!(html.contains("Noticias") && html.contains("Hasta pronto"), "the saved block follows the email's language: {html}");
    let p = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": tpl["content"]}))).await;
    assert!(p["html"].as_str().unwrap().contains("See you soon"), "the sample person has no language: the default");

    // A/B: version B carries its own Spanish; a test send in Spanish uses it.
    let ab = json!({"variants": [{"id": "b", "content": {"subject": "Hey", "blocks": [{"type": "heading", "text": "Big news"}]}}], "test_percent": 50, "wait_hours": 1});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "AB", "content": content, "ab_test": ab}))).await;
    let cid = c["id"].as_str().unwrap();
    h.ok("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"version": "b", "language": "es", "strings": [{"id": "subject", "text": "Oye"}, {"id": "b0:text", "text": "Grandes noticias"}]}))).await;
    let v = h.ok("GET", &format!("/api/translations/campaigns/{cid}?language=es"), None).await;
    assert_eq!(v["languages"], json!([]), "version A has no Spanish yet");
    let r = h.ok("POST", &format!("/api/translations/campaigns/{cid}/test"), Some(json!({"to": ["owner@acme.test"], "version": "b", "language": "es"}))).await;
    assert_eq!(r["sent"], 1);
    let m = h.state.email.sent().into_iter().rev().find(|m| m.to == "owner@acme.test").unwrap();
    assert_eq!(m.subject, "[Test] Oye");
    assert!(m.html.contains("Grandes noticias"));

    // A flow step.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "content": {"subject": "Thanks", "blocks": [{"type": "text", "html": "<p>Order {{ event.properties.order_id }}</p>"}]}, "next": null}]}}))).await;
    let fid = flow["id"].as_str().unwrap();
    h.ok("PUT", &format!("/api/translations/flows/{fid}/steps/e1"), Some(json!({"language": "es", "strings": [{"id": "subject", "text": "Gracias"}, {"id": "b0:html", "text": "<p>Pedido {{ event.properties.order_id }}</p>"}]}))).await;
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ana@x.test"}, "properties": {"order_id": "A-1"}}))).await;
    h.fast_forward().await;
    let m = h.state.email.sent().into_iter().find(|m| m.to == "ana@x.test").expect("flow email");
    assert_eq!(m.subject, "Gracias");
    assert!(m.html.contains("Pedido A-1"));
    let (s, _) = h.req("GET", &format!("/api/translations/flows/{fid}/steps/nope"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    h.cleanup().await;
}

#[tokio::test]
async fn texts_have_language_versions_too() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["es"])).await;
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "SMS", "channel": "sms", "content": {"body": "Sale today {{ first_name|default('friend') }}"}}))).await;
    let tid = t["id"].as_str().unwrap();
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(json!({"language": "es", "strings": [{"id": "body", "text": "Oferta hoy {{ first_name|default('friend') }}"}]}))).await;
    let tpl = h.ok("GET", &format!("/api/templates/{tid}"), None).await;
    let p = h.ok("POST", "/api/translations/preview", Some(json!({"channel": "sms", "content": tpl["content"], "language": "es"}))).await;
    assert!(p["body"].as_str().unwrap().contains("Oferta hoy Alex"), "{p}");
    h.cleanup().await;
}

#[tokio::test]
async fn a_persons_language_comes_from_the_api_imports_and_forms() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "a@x.test", "locale": "pt_br"}))).await;
    assert_eq!(p["locale"], "pt-BR", "{p}");
    let (s, _) = h.req("POST", "/api/profiles", Some(json!({"email": "b@x.test", "locale": "Portuguese"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "c@x.test", "language": "French"}))).await;
    assert!(p["locale"].is_null() && p["properties"]["language"] == "French", "a language that isn't a tag stays a property: {p}");
    let p = h.ok("POST", "/api/profiles", Some(json!({"email": "c@x.test", "language": "fr"}))).await;
    assert_eq!(p["locale"], "fr");
    let id = p["id"].as_str().unwrap();
    let p = h.ok("PATCH", &format!("/api/profiles/{id}"), Some(json!({"locale": "es"}))).await;
    assert_eq!(p["locale"], "es");
    let csv = "Email,Locale\nd@x.test,de-AT\ne@x.test,\n";
    h.ok("POST", "/api/profiles/import", Some(json!({"csv": csv, "email_consent": "subscribed", "permission_confirmed": true}))).await;
    let loc: Option<String> = sqlx::query_scalar("SELECT locale FROM profiles WHERE email = 'd@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(loc.as_deref(), Some("de-AT"));

    // A form: the browser's language fills an empty language, and never overwrites one.
    let list = h.ok("POST", "/api/lists", Some(json!({"name": "News"}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": list["id"], "config": {"headline": "Join"}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    for (email, browser) in [("new@x.test", "nl-BE"), ("a@x.test", "en-US")] {
        let (s, _, body) = h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": fid, "email": email, "locale": browser}).to_string()).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let got = |e: &'static str| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, Option<String>>("SELECT locale FROM profiles WHERE email = $1").bind(e).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(got("new@x.test").await.as_deref(), Some("nl-BE"));
    assert_eq!(got("a@x.test").await.as_deref(), Some("pt-BR"), "a browser can't overwrite the language the API set");
    // The site snippet can't overwrite it either.
    h.raw("POST", &format!("/client/events?k={pk}"), &[("content-type", "application/json")], &json!({"metric": "Viewed Page", "profile": {"email": "a@x.test", "locale": "ru"}}).to_string()).await;
    assert_eq!(got("a@x.test").await.as_deref(), Some("pt-BR"));

    // Segments: language is / is not / is one of.
    let count = |def: Value| json!({"definition": {"match": "all", "conditions": [def]}});
    let n = h.ok("POST", "/api/segments/preview", Some(count(json!({"type": "language", "op": "is", "value": "pt"})))).await;
    assert_eq!(n["count"], 1, "pt matches pt-BR");
    let n = h.ok("POST", "/api/segments/preview", Some(count(json!({"type": "language", "op": "is", "value": "de-DE"})))).await;
    assert_eq!(n["count"], 0, "a region matches only that region");
    let n = h.ok("POST", "/api/segments/preview", Some(count(json!({"type": "language", "op": "is_one_of", "values": ["de", "nl", "es"]})))).await;
    assert_eq!(n["count"], 3);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles").fetch_one(&h.state.db).await.unwrap();
    let n = h.ok("POST", "/api/segments/preview", Some(count(json!({"type": "language", "op": "is_not", "value": "pt"})))).await;
    assert_eq!(n["count"].as_i64().unwrap(), total - 1, "is_not includes people with no language");
    let (s, _) = h.req("POST", "/api/segments/preview", Some(count(json!({"type": "language", "op": "is", "value": "Klingon!"})))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

#[tokio::test]
async fn translators_work_round_trips_through_xliff_and_csv() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["de"])).await;
    h.ok("POST", "/api/translations/glossary", Some(json!({"term": "Acme", "never_translate": true}))).await;
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "Welcome to Acme", "channel": "email", "content": email()}))).await;
    let tid = t["id"].as_str().unwrap().to_string();
    let (s, hdrs, x) = h.raw("GET", &format!("/api/translations/templates/{tid}/export?language=de&format=xliff"), &[("cookie", h.cookie.clone().unwrap().as_str())], "").await;
    assert_eq!(s, StatusCode::OK, "{x}");
    assert!(hdrs[header::CONTENT_DISPOSITION].to_str().unwrap().contains("welcome-to-acme.de.xlf"));
    assert!(x.contains(r#"target-language="de""#) && x.contains("Acme: never translate") && x.contains(r#"<trans-unit id="b0:text""#));
    // The translator fills in two targets.
    let x = x
        .replace(r#"<target state="new" xml:space="preserve"></target>
        <note>Block 1 (heading) (text)</note>"#, r#"<target state="translated">Frische Auswahl</target>
        <note>Block 1 (heading) (text)</note>"#)
        .replace(r#"<target state="new" xml:space="preserve"></target>
        <note>Subject (text)</note>"#, r#"<target>Hallo {{ first_name|default('there') }}</target>
        <note>Subject (text)</note>"#);
    let v = h.ok("POST", &format!("/api/translations/templates/{tid}/import"), Some(json!({"language": "de", "format": "xliff", "data": x}))).await;
    assert_eq!(v["import"]["applied"], 2, "{v}");
    assert_eq!(v["summary"]["de"]["reviewed"], 2);

    // CSV: a row whose default changed since the export is skipped, not applied to the new text.
    let (_, _, csv) = h.raw("GET", &format!("/api/translations/templates/{tid}/export?language=de&format=csv"), &[("cookie", h.cookie.clone().unwrap().as_str())], "").await;
    assert!(csv.starts_with("id,where,source,translation,status"));
    let mut content = h.ok("GET", &format!("/api/templates/{tid}"), None).await["content"].clone();
    content["preheader"] = json!("Changed preview");
    h.ok("PATCH", &format!("/api/templates/{tid}"), Some(json!({"content": content}))).await;
    let edited = csv.replace("preheader,Preview text,New arrivals,,missing", "preheader,Preview text,New arrivals,Neuheiten,missing");
    let v = h.ok("POST", &format!("/api/translations/templates/{tid}/import"), Some(json!({"language": "de", "format": "csv", "data": edited}))).await;
    assert_eq!(v["import"]["changed_since_export"], json!(["preheader"]), "{v}");
    let (s, _) = h.req("POST", &format!("/api/translations/templates/{tid}/import"), Some(json!({"language": "de", "format": "xliff", "data": "<html>"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    h.cleanup().await;
}

// ---------- Machine translation against a stand-in ----------

#[derive(Default)]
struct Calls {
    requests: Vec<(Value, String, String)>,
}

/// A stand-in for `POST /v1/messages`: answers each string with "DE:" in front of it, keeping its
/// placeholders, except strings containing "MANGLE" (placeholders dropped).
async fn mock_anthropic() -> (Arc<Mutex<Calls>>, String) {
    use axum::extract::State;
    use axum::http::HeaderMap;
    let calls: Arc<Mutex<Calls>> = Default::default();
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(|State(c): State<Arc<Mutex<Calls>>>, headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                let key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let version = headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                c.lock().unwrap().requests.push((body.clone(), key, version));
                let strings: serde_json::Map<String, Value> = serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
                let out: serde_json::Map<String, Value> = strings
                    .into_iter()
                    .map(|(k, v)| {
                        let s = v.as_str().unwrap();
                        let t = if s.contains("MANGLE") {
                            "kaputt".to_string()
                        } else if s.contains("LINKIFY") {
                            // A model that adds a link of its own.
                            r#"<a href="https://evil.test">DE</a>"#.to_string()
                        } else {
                            format!("DE:{s}")
                        };
                        (k, json!(t))
                    })
                    .collect();
                axum::Json(json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "stop_reason": "end_turn",
                    "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": format!("```json\n{}\n```", Value::Object(out))}],
                    "usage": {"input_tokens": 10, "output_tokens": 10}}))
            }),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (calls, format!("http://{addr}"))
}

#[tokio::test]
async fn draft_with_ai_sends_only_the_text_and_keeps_tags_exactly() {
    let (calls, url) = mock_anthropic().await;
    let Some(mut h) = Harness::with(|c| {
        c.anthropic_api_key = Some("sk-ant-test-0123456789".into());
        c.anthropic_api_url = url.clone();
        c.translation_daily_chars = 2_000;
    })
    .await
    else {
        return;
    };
    h.signup().await;
    set_languages(&mut h, json!(["de"])).await;
    h.ok("POST", "/api/translations/glossary", Some(json!({"term": "Acme", "never_translate": true}))).await;
    h.ok("POST", "/api/profiles", Some(json!({"email": "secret.person@x.test", "first_name": "Zelda", "locale": "de"}))).await;
    let mut content = email();
    content["blocks"].as_array_mut().unwrap().push(json!({"type": "heading", "text": "Acme MANGLE this"}));
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "AI", "content": content}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let settings = h.ok("GET", "/api/translations/settings", None).await;
    assert_eq!(settings["machine_translation"]["available"], true);

    let job = h.ok("POST", &format!("/api/translations/campaigns/{cid}/draft"), Some(json!({"language": "de"}))).await;
    assert_eq!(job["status"], "queued", "{job}");
    // One run per account at a time.
    let (s, e) = h.req("POST", &format!("/api/translations/campaigns/{cid}/draft"), Some(json!({"language": "de"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    h.drain().await;
    let jid = job["id"].as_str().unwrap();
    let done = h.ok("GET", &format!("/api/translations/jobs/{jid}"), None).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["skipped"], 1, "the mangled one is thrown away: {done}");

    // What went to the model: the strings with placeholders, the right headers, nothing about people.
    let reqs = calls.lock().unwrap().requests.clone();
    assert_eq!(reqs.len(), 1);
    let (body, key, version) = &reqs[0];
    assert_eq!(key, "sk-ant-test-0123456789");
    assert_eq!(version, "2026-10-21");
    assert_eq!(body["model"], "claude-sonnet-5-5");
    let sent = body.to_string();
    assert!(!sent.contains("secret.person") && !sent.contains("Zelda"), "no profile data");
    assert!(!sent.contains("first_name") && !sent.contains("<strong>"), "tags and markup are placeholders: {sent}");
    assert!(sent.contains("⟦T1⟧") && sent.contains("⟦K"), "{sent}");

    let v = h.ok("GET", &format!("/api/translations/campaigns/{cid}?language=de"), None).await;
    let s = |id: &str| v["strings"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
    assert_eq!(s("subject")["text"], "DE:Hello {{ first_name|default('there') }}");
    assert_eq!(s("subject")["status"], "machine");
    assert_eq!(s("b1:html")["text"], "DE:<p>Hi <strong>{{ first_name|default('there') }}</strong>, take a look.</p>");
    assert_eq!(s("b3:text")["status"], "missing");
    assert_eq!(s("b2.c0.0:href")["status"], "default", "links aren't machine translated");
    // Unreviewed drafts are sent (with a warning) until someone reviews them.
    let chk = h.ok("POST", "/api/content/check", Some(json!({"campaign_id": cid, "skip_links": true}))).await;
    assert!(chk["findings"].as_array().unwrap().iter().any(|f| f["id"] == "translation_unreviewed"), "{chk}");
    let r = h.ok("POST", &format!("/api/translations/campaigns/{cid}/review"), Some(json!({"language": "de"}))).await;
    assert!(r["reviewed"].as_u64().unwrap() >= 4, "{r}");

    // A replay of the finished run changes nothing and calls nothing.
    dumbmail::features::translations::draft::run(&h.state, &json!({"job_id": jid})).await.unwrap();
    assert_eq!(calls.lock().unwrap().requests.len(), 1);

    // The daily allowance is enforced in the statement that reserves it.
    let mut big = email();
    big["blocks"][1]["html"] = json!(format!("<p>{}</p>", "word ".repeat(400)));
    let c2 = h.ok("POST", "/api/campaigns", Some(json!({"name": "Big", "content": big}))).await;
    let (s, e) = h.req("POST", &format!("/api/translations/campaigns/{}/draft", c2["id"].as_str().unwrap()), Some(json!({"language": "de"}))).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{e}");
    assert_eq!(e["error"]["code"], "limit_reached");
    assert!(!e.to_string().contains("sk-ant"), "the key is never shown");

    // A draft whose default text changed before it came back isn't written over the new text.
    let c3 = h.ok("POST", "/api/campaigns", Some(json!({"name": "Race", "content": {"subject": "One", "blocks": [{"type": "heading", "text": "Two"}]}}))).await;
    let c3id = c3["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/translations/campaigns/{c3id}/draft"), Some(json!({"language": "de"}))).await;
    h.ok("PATCH", &format!("/api/campaigns/{c3id}"), Some(json!({"content": {"subject": "One more", "blocks": [{"type": "heading", "text": "Two"}]}}))).await;
    h.drain().await;
    let v = h.ok("GET", &format!("/api/translations/campaigns/{c3id}?language=de"), None).await;
    let st: Vec<(String, String)> = v["strings"].as_array().unwrap().iter().map(|s| (s["id"].as_str().unwrap().to_string(), s["status"].as_str().unwrap().to_string())).collect();
    assert_eq!(st, vec![("subject".into(), "missing".into()), ("b0:text".into(), "machine".into())], "{v}");
    let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'translation.draft'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(audit, 2);
    h.cleanup().await;
}

#[tokio::test]
async fn two_draft_requests_at_once_start_one_run() {
    let (_calls, url) = mock_anthropic().await;
    let Some(mut h) = Harness::with(|c| {
        c.anthropic_api_key = Some("sk-ant-test".into());
        c.anthropic_api_url = url.clone();
    })
    .await
    else {
        return;
    };
    h.signup().await;
    set_languages(&mut h, json!(["de"])).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "AI", "content": email()}))).await;
    let path = format!("/api/translations/campaigns/{}/draft", c["id"].as_str().unwrap());
    let mk = || async {
        let b = h.page_headers("POST", Request::builder().method("POST").uri(&path)).await;
        b.header(header::CONTENT_TYPE, "application/json").body(Body::from(json!({"language": "de"}).to_string())).unwrap()
    };
    let (r1, r2) = (mk().await, mk().await);
    let (a, b) = tokio::join!(h.router.clone().oneshot(r1), h.router.clone().oneshot(r2));
    let mut codes = vec![a.unwrap().status(), b.unwrap().status()];
    codes.sort();
    assert_eq!(codes, vec![StatusCode::OK, StatusCode::CONFLICT]);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM translation_jobs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(runs, 1);
    let used: i64 = sqlx::query_scalar("SELECT chars FROM translation_usage").fetch_one(&h.state.db).await.unwrap();
    let chars: i32 = sqlx::query_scalar("SELECT chars FROM translation_jobs").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(used, i64::from(chars), "only the run that started reserved characters");
    h.cleanup().await;
}

#[tokio::test]
async fn without_a_key_drafting_is_unavailable_and_mcp_asks_to_confirm() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["fr"])).await;
    let settings = h.ok("GET", "/api/translations/settings", None).await;
    assert_eq!(settings["machine_translation"]["available"], false);
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "X", "content": email()}))).await;
    let (s, _) = h.req("POST", &format!("/api/translations/campaigns/{}/draft", c["id"].as_str().unwrap()), Some(json!({"language": "fr"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "k"}))).await["key"].as_str().unwrap().to_string();
    let call = |tool: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    let (st, v) = bearer(&h, &key, "POST", "/mcp", Some(call("draft_translations", json!({"target": {"kind": "campaign", "id": c["id"]}, "language": "fr"})))).await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("dry_run"), "{v}");
    let (_, v) = bearer(&h, &key, "POST", "/mcp", Some(call("set_translations", json!({"target": {"kind": "campaign", "id": c["id"]}, "language": "fr", "strings": [{"id": "subject", "text": "Bonjour {{ first_name|default('there') }}"}]})))).await;
    assert_ne!(v["result"]["isError"], true, "{v}");
    let (_, v) = bearer(&h, &key, "POST", "/mcp", Some(call("get_translations", json!({"target": {"kind": "campaign", "id": c["id"]}, "language": "fr"})))).await;
    assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("Bonjour"), "{v}");
    h.cleanup().await;
}

async fn bearer(h: &Harness, key: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(path).header(header::AUTHORIZATION, format!("Bearer {key}"));
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = h.router.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&bytes).unwrap_or(json!({"_raw": String::from_utf8_lossy(&bytes)})))
}

#[tokio::test]
async fn read_only_keys_and_other_accounts_cant_change_translations() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["fr"])).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "X", "content": email()}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let read = h.ok("POST", "/api/api-keys", Some(json!({"name": "ro", "scopes": ["read"]}))).await["key"].as_str().unwrap().to_string();
    let (s, _) = bearer(&h, &read, "GET", &format!("/api/translations/campaigns/{cid}"), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = bearer(&h, &read, "PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": []}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = bearer(&h, &read, "PUT", "/api/translations/settings", Some(json!({"languages": ["de"]}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "set_translations", "arguments": {"target": {"kind": "campaign", "id": cid}, "language": "fr", "strings": []}}});
    let (_, v) = bearer(&h, &read, "POST", "/mcp", Some(call)).await;
    assert!(v["result"]["isError"] == true || v["error"].is_object(), "{v}");

    // Another account can't read or change this account's campaign.
    let mut other = Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: String::new() };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "boss@other.test", "password": "correct horse", "company": "Other"}))).await;
    other.ok("PUT", "/api/translations/settings", Some(json!({"languages": ["fr"]}))).await;
    let (s, _) = other.req("GET", &format!("/api/translations/campaigns/{cid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = other.req("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "fr", "strings": [{"id": "subject", "text": "x"}]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = other.req("GET", &format!("/api/translations/report?campaign_id={cid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    drop(other);
    h.cleanup().await;
}

// ---------- Hosted pages, system texts, right to left ----------

#[tokio::test]
async fn hosted_pages_and_the_opt_in_email_speak_the_persons_language() {
    let mut h = harness!();
    let me = h.signup().await;
    let pk = me["account"]["public_key"].as_str().unwrap().to_string();
    h.credit(5.0).await;
    set_languages(&mut h, json!(["fr", "ar"])).await;
    h.ok("PUT", "/api/translations/system-texts", Some(json!({"language": "fr", "texts": {"unsub.button": "Oui, me désabonner"}}))).await;
    let (s, _) = h.req("PUT", "/api/translations/system-texts", Some(json!({"language": "fr", "texts": {"unsub.done_body": "Au revoir"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "placeholders must stay");
    let ann = person(&mut h, "ann@x.test", Some("fr")).await;
    let cat = person(&mut h, "cat@x.test", None).await;
    let list = list_of(&mut h, &[ann, cat]).await;
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "Plain", "audience": {"include": [{"type": "list", "id": list}]}, "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let sent = h.state.email.sent();
    let unsub = |to: &str| sent.iter().find(|m| m.to == to).unwrap().unsubscribe_url.clone().unwrap().replace("https://pl.test", "");
    // An email without language versions is unchanged, but its pages follow the person.
    assert!(sent.iter().find(|m| m.to == "ann@x.test").unwrap().html.contains(">Unsubscribe</a>"));
    let (_, _, page) = h.raw("GET", &unsub("ann@x.test"), &[], "").await;
    assert!(page.contains(r#"<html lang="fr">"#) && page.contains("Ne plus recevoir les e-mails marketing de Acme Goods ?") && page.contains("Oui, me désabonner"), "{page}");
    let (_, _, page) = h.raw("GET", &unsub("cat@x.test"), &[("accept-language", "ar-EG,en;q=0.5")], "").await;
    assert!(page.contains(r#"<html lang="ar" dir="rtl">"#), "no profile language: the browser's: {page}");
    let (_, _, page) = h.raw("GET", &unsub("cat@x.test"), &[("accept-language", "ja")], "").await;
    assert!(page.contains(r#"<html lang="en">"#) && page.contains("Stop receiving marketing emails from Acme Goods?"));
    let (_, _, done) = h.raw("POST", &unsub("ann@x.test"), &[("content-type", "application/x-www-form-urlencoded")], "").await;
    assert!(done.contains("Vous êtes désabonné(e)"), "{done}");
    let consent: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'ann@x.test'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(consent, "unsubscribed", "the French page unsubscribes like the English one");

    // Double opt-in: the email and the page in the language the form gave.
    let dlist = h.ok("POST", "/api/lists", Some(json!({"name": "Newsletter", "double_opt_in": true}))).await;
    let form = h.ok("POST", "/api/forms", Some(json!({"name": "Popup", "list_id": dlist["id"], "config": {"headline": "Join"}}))).await;
    let fid = form["id"].as_str().unwrap().to_string();
    h.ok("PATCH", &format!("/api/forms/{fid}"), Some(json!({"status": "live"}))).await;
    h.raw("POST", &format!("/client/subscribe?k={pk}"), &[("content-type", "application/json")], &json!({"form_id": fid, "email": "neu@x.test", "locale": "fr-FR"}).to_string()).await;
    let m = h.state.email.sent().into_iter().find(|m| m.to == "neu@x.test").expect("confirmation email");
    assert_eq!(m.subject, "Confirmez votre inscription à Acme Goods");
    assert!(m.html.contains(r#"<html lang="fr">"#) && m.html.contains("Oui, je m'inscris") && m.text.contains("Le lien est valable 7 jours."), "{}", m.html);
    let link = find_link(&m.html, "https://pl.test/c/");
    let (_, _, page) = h.raw("GET", link.trim_start_matches("https://pl.test"), &[], "").await;
    assert!(page.contains("Commencer à recevoir les e-mails de Acme Goods ?"), "{page}");
    let (_, _, page) = h.raw("POST", link.trim_start_matches("https://pl.test"), &[], "").await;
    assert!(page.contains("Vous êtes inscrit(e)"), "{page}");

    // An account that never set up languages gets exactly the English pages.
    let texts = h.ok("GET", "/api/translations/system-texts?language=fr", None).await;
    let k = texts["texts"].as_array().unwrap().iter().find(|t| t["key"] == "unsub.button").unwrap();
    assert_eq!(k["text"], "Oui, me désabonner");
    assert_eq!(k["shipped"], "Se désabonner");
    h.cleanup().await;
}

#[tokio::test]
async fn arabic_and_hebrew_emails_render_right_to_left_without_breaking_the_layout() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["ar", "he"])).await;
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "RTL", "channel": "email", "content": {"subject": "Sale", "blocks": [
        {"type": "heading", "text": "Big sale", "align": "left"},
        {"type": "text", "html": "<p>Everything is half off.</p>"},
        {"type": "button", "text": "Shop", "href": "https://acme.test", "align": "right"}
    ]}}))).await;
    let tid = t["id"].as_str().unwrap();
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(json!({"language": "ar", "strings": [{"id": "subject", "text": "تخفيضات"}, {"id": "b0:text", "text": "تخفيضات كبيرة"}, {"id": "b1:html", "text": "<p>كل شيء بنصف السعر.</p>"}, {"id": "b2:text", "text": "تسوق"}]}))).await;
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(json!({"language": "he", "strings": [{"id": "b0:text", "text": "מבצע גדול"}]}))).await;
    let content = h.ok("GET", &format!("/api/templates/{tid}"), None).await["content"].clone();
    for (lang, word) in [("ar", "تخفيضات كبيرة"), ("he", "מבצע גדול")] {
        let p = h.ok("POST", "/api/translations/preview", Some(json!({"channel": "email", "content": content, "language": lang}))).await;
        let html = p["html"].as_str().unwrap();
        assert!(html.contains(&format!(r#"<html lang="{lang}" dir="rtl">"#)) && html.contains(r#"<body dir="rtl" style="#), "{html}");
        assert!(html.contains(word));
        assert!(html.contains("text-align:right;") && html.contains(r#"align="left""#), "alignment is mirrored");
        let tables = html.matches(r#"width="600""#).count();
        assert_eq!(tables, 2, "the card and footer keep their widths");
        assert!(html.contains("@media (max-width:620px)") && html.contains("</table></td></tr></table></body></html>"));
    }
    h.cleanup().await;
}

#[tokio::test]
async fn languages_are_bounded_and_the_glossary_is_per_account() {
    let mut h = harness!();
    h.signup().await;
    let many: Vec<String> = ["fr", "es", "de", "it", "pt", "nl", "ar", "he", "ja", "zh", "ko", "pl", "sv", "da", "fi", "tr", "ru", "uk", "cs", "el", "hi"].iter().map(|s| s.to_string()).collect();
    let (s, _) = h.req("PUT", "/api/translations/settings", Some(json!({"languages": many}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "at most 20");
    let (s, _) = h.req("PUT", "/api/translations/settings", Some(json!({"languages": ["fr", "<script>"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v = h.ok("PUT", "/api/translations/settings", Some(json!({"default_language": "EN-us", "languages": ["fr", "fr", "en-US"]}))).await;
    assert_eq!(v["default_language"], "en-US");
    assert_eq!(v["languages"], json!(["fr"]));
    let g = h.ok("POST", "/api/translations/glossary", Some(json!({"term": "Acme", "never_translate": true}))).await;
    let (s, _) = h.req("POST", "/api/translations/glossary", Some(json!({"term": "acme"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    h.ok("PATCH", &format!("/api/translations/glossary/{}", g["id"].as_str().unwrap()), Some(json!({"translations": {"fr": "Acmé"}}))).await;
    let list = h.ok("GET", "/api/translations/glossary", None).await;
    assert_eq!(list["data"][0]["translations"]["fr"], "Acmé");
    let acct = account_id(&h).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM translation_glossary WHERE account_id = $1").bind(acct).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 1);
    h.ok("DELETE", &format!("/api/translations/glossary/{}", g["id"].as_str().unwrap()), None).await;
    h.cleanup().await;
}

#[tokio::test]
async fn the_editor_works_on_unsaved_content_without_storing_anything() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["fr"])).await;
    let v = h.ok("POST", "/api/translations/apply", Some(json!({"content": email(), "language": "fr", "strings": [{"id": "b0:text", "text": "Nouveautés"}]}))).await;
    let content = v["content"].clone();
    assert_eq!(content["translations"]["languages"], json!(["fr"]));
    assert_eq!(content["blocks"][0]["i18n"]["fr"]["text"]["text"], "Nouveautés");
    assert_eq!(content["blocks"][0]["i18n"]["fr"]["text"]["source"].as_str().unwrap().len(), 8, "the source hash is filled in");
    let (s, _) = h.req("POST", "/api/translations/apply", Some(json!({"content": email(), "language": "fr", "strings": [{"id": "subject", "text": "Bonjour"}]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "tags are checked here too");
    let i = h.ok("POST", "/api/translations/inspect", Some(json!({"content": content, "language": "fr"}))).await;
    assert_eq!(i["summary"]["fr"]["reviewed"], 1);
    let (s, _, csv) = h.raw("POST", "/api/translations/export", &[("cookie", h.cookie.clone().unwrap().as_str()), ("content-type", "application/json"), ("origin", "https://pl.test"), ("x-dumbmail-account", &account_id(&h).await.to_string())], &json!({"content": content, "language": "fr", "format": "csv", "name": "News"}).to_string()).await;
    assert_eq!(s, StatusCode::OK, "{csv}");
    let edited = csv.replace("subject,Subject,Hello {{ first_name|default('there') }},,missing", "subject,Subject,Hello {{ first_name|default('there') }},Bonjour {{ first_name|default('there') }},missing");
    let v = h.ok("POST", "/api/translations/import", Some(json!({"content": content, "language": "fr", "format": "csv", "data": edited}))).await;
    assert_eq!(v["import"]["applied"], 2, "{v}");
    assert_eq!(v["content"]["translations"]["fields"]["fr"]["subject"]["text"], "Bonjour {{ first_name|default('there') }}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM campaigns").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);
    h.cleanup().await;
}

/// A template whose `html` has a French version, previewed in French.
async fn preview_fr(h: &mut Harness, html: &str, fr: &str) -> String {
    let t = h.ok("POST", "/api/templates", Some(json!({"name": format!("T{}", Uuid::now_v7().simple()), "channel": "email", "content": {"subject": "Hi", "html": html}}))).await;
    let tid = t["id"].as_str().unwrap().to_string();
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(json!({"language": "fr", "strings": [{"id": "html", "text": fr}]}))).await;
    let content = h.ok("GET", &format!("/api/templates/{tid}"), None).await["content"].clone();
    h.ok("POST", "/api/translations/preview", Some(json!({"channel": "email", "content": content, "language": "fr"}))).await["html"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_translation_cant_remove_the_unsubscribe_footer() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["fr"])).await;
    // The original relies on the automatic footer; the translation only mentions the word as text.
    let out = preview_fr(&mut h, "<html><body><p>Hello</p></body></html>", "<html><body><p>Bonjour unsubscribe_url</p></body></html>").await;
    assert!(out.contains("Vous recevez cet e-mail parce que vous vous êtes inscrit(e).") && out.contains("Se désabonner</a>"), "the footer stays: {out}");
    // The original places its own unsubscribe link: every language keeps it, with no second footer.
    let own = r#"<html><body><p>Hi</p><a href="{{ unsubscribe_url }}">Leave</a></body></html>"#;
    let out = preview_fr(&mut h, own, r#"<html><body><p>Salut</p><a href="{{ unsubscribe_url }}">Partir</a></body></html>"#).await;
    assert!(out.contains(">Partir</a>") && !out.contains("Vous recevez cet e-mail"), "{out}");
    h.cleanup().await;
}

#[tokio::test]
async fn translations_change_only_words_on_every_path() {
    let (calls, url) = mock_anthropic().await;
    let Some(mut h) = Harness::with(|c| {
        c.anthropic_api_key = Some("sk-ant-test".into());
        c.anthropic_api_url = url.clone();
    })
    .await
    else {
        return;
    };
    h.signup().await;
    set_languages(&mut h, json!(["de"])).await;
    let content = json!({"subject": "Hello {{ first_name|default('there') }}", "blocks": [
        {"type": "text", "html": "<p>Hi <a href=\"https://acme.test/shop\">shop now</a>, {{ first_name }}.</p>"},
        {"type": "heading", "text": "LINKIFY this"}
    ]});
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "S", "content": content}))).await;
    let cid = c["id"].as_str().unwrap().to_string();
    let bad = [
        // A new link.
        ("b0:html", "<p>Hallo <a href=\"https://acme.test/shop\">jetzt kaufen</a>, {{ first_name }}. <a href=\"https://evil.test\">x</a></p>"),
        // A changed destination.
        ("b0:html", "<p>Hallo <a href=\"https://evil.test\">jetzt kaufen</a>, {{ first_name }}.</p>"),
        // A variable moved from the text into the link.
        ("b0:html", "<p>Hallo <a href=\"https://acme.test/shop?n={{ first_name }}\">jetzt kaufen</a>.</p>"),
        // Markup in a plain-text field.
        ("subject", "Hallo <b>{{ first_name|default('there') }}</b>"),
        // A changed filter argument.
        ("subject", "Hallo {{ first_name|default('du') }}"),
    ];
    for (id, text) in bad {
        let (s, e) = h.req("PUT", &format!("/api/translations/campaigns/{cid}"), Some(json!({"language": "de", "strings": [{"id": id, "text": text}]}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "API {id} {text}: {e}");
        let (s, _) = h.req("POST", "/api/translations/apply", Some(json!({"content": content, "language": "de", "strings": [{"id": id, "text": text}]}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "editor {id} {text}");
    }
    // MCP.
    let key = h.ok("POST", "/api/api-keys", Some(json!({"name": "k"}))).await["key"].as_str().unwrap().to_string();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "set_translations", "arguments": {"target": {"kind": "campaign", "id": cid}, "language": "de", "strings": [{"id": "b0:html", "text": bad[1].1}]}}});
    let (_, v) = bearer(&h, &key, "POST", "/mcp", Some(call)).await;
    assert_eq!(v["result"]["isError"], true, "{v}");
    // Content saved directly, with a translation whose source hash is current.
    let mut raw = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await["content"].clone();
    let src = raw["blocks"][0]["html"].as_str().unwrap().to_string();
    let v = h.ok("POST", "/api/translations/apply", Some(json!({"content": raw, "language": "de", "strings": [{"id": "b0:html", "text": "<p>Hallo <a href=\"https://acme.test/shop\">jetzt kaufen</a>, {{ first_name }}.</p>"}]}))).await;
    raw = v["content"].clone();
    assert_eq!(raw["blocks"][0]["i18n"]["de"]["html"]["source"].as_str().unwrap().len(), 8, "{src}");
    raw["blocks"][0]["i18n"]["de"]["html"]["text"] = json!(bad[1].1);
    let (s, e) = h.req("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"content": raw}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    // XLIFF and CSV imports refuse it (and say why).
    let (_, _, x) = h.raw("GET", &format!("/api/translations/campaigns/{cid}/export?language=de&format=xliff"), &[("cookie", h.cookie.clone().unwrap().as_str())], "").await;
    let marker = r#"<trans-unit id="b0:html""#;
    let at = x.find(marker).unwrap();
    let target_at = at + x[at..].find("<target").unwrap();
    let end = target_at + x[target_at..].find("</target>").unwrap();
    let x = format!("{}<target>{}{}", &x[..target_at], bad[0].1.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"), &x[end..]);
    let v = h.ok("POST", &format!("/api/translations/campaigns/{cid}/import"), Some(json!({"language": "de", "format": "xliff", "data": x}))).await;
    assert_eq!(v["import"]["refused"][0]["id"], "b0:html", "{v}");
    let csv = format!("id,where,source,translation,status\nsubject,Subject,\"Hello {{{{ first_name|default('there') }}}}\",\"{}\",missing\n", bad[3].1);
    let v = h.ok("POST", &format!("/api/translations/campaigns/{cid}/import"), Some(json!({"language": "de", "format": "csv", "data": csv}))).await;
    assert_eq!(v["import"]["refused"][0]["id"], "subject", "{v}");
    // A machine draft that adds a link is thrown away; the others are kept.
    h.ok("POST", &format!("/api/translations/campaigns/{cid}/draft"), Some(json!({"language": "de"}))).await;
    h.drain().await;
    assert_eq!(calls.lock().unwrap().requests.len(), 1);
    let v = h.ok("GET", &format!("/api/translations/campaigns/{cid}?language=de"), None).await;
    let st = |id: &str| v["strings"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap()["status"].clone();
    assert_eq!(st("b1:text"), "missing", "{v}");
    assert_eq!(st("subject"), "machine");
    assert!(!v.to_string().contains("evil.test"));
    h.cleanup().await;
}

#[tokio::test]
async fn localized_links_are_real_web_addresses_and_never_dumbmails_own() {
    let mut h = harness!();
    h.signup().await;
    set_languages(&mut h, json!(["fr"])).await;
    let content = json!({"subject": "Hi", "blocks": [
        {"type": "button", "text": "Shop", "href": "https://acme.test/en"},
        {"type": "button", "text": "Leave", "href": "{{ unsubscribe_url }}"},
        {"type": "button", "text": "Write", "href": "mailto:hi@acme.test"}
    ]});
    let t = h.ok("POST", "/api/templates", Some(json!({"name": "Links", "channel": "email", "content": content}))).await;
    let tid = t["id"].as_str().unwrap().to_string();
    let put = |id: &str, text: &str| json!({"language": "fr", "strings": [{"id": id, "text": text}]});
    for bad in ["https://pl.test/u/abc", "https://links.pl.test/x", "http://acme.test/fr", "https://user:pw@acme.test/fr", "https://acme.test@evil.test/", "javascript:alert(1)", "https://{{ first_name }}.test/", "mailto:fr@acme.test", "//acme.test/fr"] {
        let (s, e) = h.req("PUT", &format!("/api/translations/templates/{tid}"), Some(put("b0:href", bad))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {e}");
    }
    let (s, _) = h.req("PUT", &format!("/api/translations/templates/{tid}"), Some(put("b1:href", "https://acme.test/desabonnement"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the unsubscribe link can't be localized");
    let (s, _) = h.req("PUT", &format!("/api/translations/templates/{tid}"), Some(put("b2:href", "https://acme.test/contact"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a mailto: link stays a mailto: link");
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(put("b2:href", "mailto:bonjour@acme.test"))).await;
    h.ok("PUT", &format!("/api/translations/templates/{tid}"), Some(put("b0:href", "https://acme.test/fr"))).await;

    // A localized link stored straight in the content, pointing at DumbMail, is never sent.
    let mut c = h.ok("GET", &format!("/api/templates/{tid}"), None).await["content"].clone();
    c["blocks"][0]["i18n"]["fr"]["href"]["text"] = json!("https://pl.test/t/c/forged");
    h.ok("PATCH", &format!("/api/templates/{tid}"), Some(json!({"content": c.clone()}))).await;
    let p = h.ok("POST", "/api/translations/preview", Some(json!({"channel": "email", "content": c, "language": "fr"}))).await;
    let html = p["html"].as_str().unwrap();
    assert!(html.contains("https://acme.test/en") && !html.contains("forged"), "the default link is used: {html}");
    assert!(html.contains("mailto:bonjour@acme.test"));
    h.cleanup().await;
}

#[tokio::test]
async fn translations_inside_a_saved_block_are_used_on_their_own() {
    let mut h = harness!();
    h.signup().await;
    h.credit(5.0).await;
    set_languages(&mut h, json!(["fr"])).await;
    let (_, sb) = h.req("POST", "/api/content/saved-blocks", Some(json!({"name": "Sign-off", "blocks": [{"type": "text", "html": "<p>See you soon</p>"}]}))).await;
    let sbid = sb["id"].as_str().unwrap().to_string();
    h.ok("PUT", &format!("/api/translations/saved-blocks/{sbid}"), Some(json!({"language": "fr", "strings": [{"id": "b0:html", "text": "<p>À bientôt</p>"}]}))).await;
    // The email itself has no translations at all.
    let content = json!({"subject": "Hi", "blocks": [{"type": "heading", "text": "News"}, {"type": "saved", "saved_block_id": sbid}]});
    let ann = person(&mut h, "ann@x.test", Some("fr")).await;
    let bob = person(&mut h, "bob@x.test", None).await;
    let list = list_of(&mut h, &[ann, bob]).await;
    // A campaign: the saved block is copied in when the send starts, translations and all.
    let c = h.ok("POST", "/api/campaigns", Some(json!({"name": "SB", "audience": {"include": [{"type": "list", "id": list}]}, "content": content}))).await;
    h.ok("POST", &format!("/api/campaigns/{}/send", c["id"].as_str().unwrap()), None).await;
    h.drain().await;
    let sent = h.state.email.sent();
    let a = sent.iter().find(|m| m.to == "ann@x.test").unwrap();
    let b = sent.iter().find(|m| m.to == "bob@x.test").unwrap();
    assert!(a.html.contains("À bientôt") && a.html.contains(r#"<html lang="fr">"#), "{}", a.html);
    assert!(b.html.contains("See you soon") && !b.html.contains("À bientôt"));
    // A flow: the saved block is read live.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "F", "trigger": {"type": "metric", "metric": "Placed Order"}, "definition": {"entry": "e1", "nodes": [{"id": "e1", "type": "email", "content": content, "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    h.ok("POST", "/api/events", Some(json!({"metric": "Placed Order", "profile": {"email": "ann@x.test"}}))).await;
    h.fast_forward().await;
    let m = h.state.email.sent().into_iter().filter(|m| m.to == "ann@x.test").last().unwrap();
    assert!(m.html.contains("À bientôt"), "{}", m.html);
    h.cleanup().await;
}

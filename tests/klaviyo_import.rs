//! Moving from Klaviyo, end to end against a local stand-in for Klaviyo's API (revision 2026-10-15):
//! JSON:API pages with `links.next` cursors, the `revision` header, a 429 with `Retry-After`, and a 403
//! for a scope the key lacks.
#![recursion_limit = "512"]
#[macro_use]
mod common;
use common::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use chrono::{Duration, Utc};
use serde_json::{json, Value};

#[derive(Default)]
struct Mock {
    /// API key → the Klaviyo account it opens.
    sets: HashMap<String, Value>,
    /// API key → requests seen ("METHOD path?query").
    calls: HashMap<String, Vec<String>>,
}

type Shared = Arc<Mutex<Mock>>;

static MOCK: OnceLock<(String, Shared)> = OnceLock::new();

/// One stand-in server for the whole test binary (the importer's base URL is process-wide), on its own
/// thread so it outlives each test's runtime. Tests keep apart by using their own API key.
fn mock() -> &'static (String, Shared) {
    MOCK.get_or_init(|| {
        let shared: Shared = Default::default();
        let s2 = shared.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                tx.send(base).unwrap();
                let app = axum::Router::new().fallback(handle).with_state(s2);
                axum::serve(listener, app).await.unwrap();
            });
        });
        let base = rx.recv().unwrap();
        dumbmail::features::klaviyo_import::set_api_base(&base);
        (base, shared)
    })
}

fn params(q: &Option<String>) -> HashMap<String, String> {
    url::form_urlencoded::parse(q.as_deref().unwrap_or("").as_bytes()).into_owned().collect()
}

fn page_of(base: &str, path: &str, q: &Option<String>, items: Vec<Value>, max: usize) -> Value {
    let p = params(q);
    let size = p.get("page[size]").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20).min(max).max(1);
    let start = p.get("page[cursor]").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let slice: Vec<Value> = items.iter().skip(start).take(size).cloned().collect();
    let next = if start + size < items.len() {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &p {
            if k != "page[cursor]" {
                ser.append_pair(k, v);
            }
        }
        ser.append_pair("page[cursor]", &(start + size).to_string());
        json!(format!("{base}{path}?{}", ser.finish()))
    } else {
        Value::Null
    };
    json!({"data": slice, "links": {"self": format!("{base}{path}"), "next": next}})
}

fn err(status: u16, code: &str, detail: &str) -> axum::response::Response {
    (StatusCode::from_u16(status).unwrap(), axum::Json(json!({"errors": [{"id": "x", "status": status, "code": code, "title": code, "detail": detail}]}))).into_response()
}

/// `greater-than(field,2025-01-01T00:00:00Z)` style filters: the timestamp after `field,`.
fn filter_time(filter: &str, field: &str) -> Option<(bool, chrono::DateTime<Utc>)> {
    for op in ["greater-or-equal", "greater-than"] {
        let pre = format!("{op}({field},");
        if let Some(i) = filter.find(&pre) {
            let rest = &filter[i + pre.len()..];
            let t = rest.split(')').next()?;
            let parsed = chrono::DateTime::parse_from_rfc3339(t).map(|d| d.with_timezone(&Utc)).ok().or_else(|| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S").ok().map(|n| n.and_utc()))?;
            return Some((op == "greater-or-equal", parsed));
        }
    }
    None
}

fn time_of(v: &Value) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap_or("1970-01-01T00:00:00Z")).unwrap().with_timezone(&Utc)
}

fn profile_res(ds: &Value, id: &str, with_subs: bool) -> Option<Value> {
    let mut p = ds["profiles"].as_array()?.iter().chain(ds["hidden_profiles"].as_array().into_iter().flatten()).find(|p| p["id"] == id)?.clone();
    if !with_subs {
        p["attributes"].as_object_mut().unwrap().remove("subscriptions");
    }
    Some(p)
}

async fn handle(State(m): State<Shared>, method: Method, uri: Uri, headers: HeaderMap, RawQuery(q): RawQuery, body: axum::body::Bytes) -> axum::response::Response {
    let base = mock().0.clone();
    let path = uri.path().trim_end_matches('/').to_string();
    let key = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Klaviyo-API-Key ")).unwrap_or("").to_string();
    if headers.get("revision").and_then(|v| v.to_str().ok()) != Some("2026-10-15") {
        return err(400, "invalid", "missing or wrong revision header");
    }
    let mut g = m.lock().unwrap();
    let Some(ds) = g.sets.get(&key).cloned() else { return err(401, "not_authenticated", "Incorrect authentication credentials.") };
    g.calls.entry(key.clone()).or_default().push(format!("{method} {path}?{}", q.clone().unwrap_or_default()));
    let p = params(&q);
    let subs = p.get("additional-fields[profile]").is_some_and(|v| v.contains("subscriptions"));
    let filter = p.get("filter").cloned().unwrap_or_default();
    let seg: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
    // Klaviyo's 429: no rate-limit headers, a Retry-After in seconds.
    if path == "/api/profiles" && ds["throttle_once"] == true && !g.sets[&key]["throttled"].as_bool().unwrap_or(false) {
        g.sets.get_mut(&key).unwrap()["throttled"] = json!(true);
        return (StatusCode::TOO_MANY_REQUESTS, [("Retry-After", "1")], axum::Json(json!({"errors": [{"status": 429, "code": "throttled", "detail": "Request was throttled."}]}))).into_response();
    }
    drop(g);
    let arr = |k: &str| ds[k].as_array().cloned().unwrap_or_default();
    let out: Value = match (method.as_str(), seg.as_slice()) {
        ("GET", ["accounts"]) => json!({"data": [{"type": "account", "id": ds["account_id"].as_str().unwrap_or("ACCT1"), "attributes": {"test_account": false, "contact_information": {"organization_name": "Acme Klaviyo", "website_url": "https://acme.test"}, "timezone": "US/Pacific", "preferred_currency": "USD"}}]}),
        ("GET", ["profiles"]) => {
            let mut items: Vec<Value> = arr("profiles");
            if let Some(reason) = filter.strip_prefix("equals(subscriptions.email.marketing.suppression.reason,'").and_then(|r| r.strip_suffix("')")) {
                items.retain(|p| p["attributes"]["subscriptions"]["email"]["marketing"]["suppression"].as_array().is_some_and(|s| s.iter().any(|x| x["reason"] == reason)));
            }
            if let Some((_, t)) = filter_time(&filter, "updated") {
                items.retain(|p| time_of(&p["attributes"]["updated"]) > t);
            }
            // Klaviyo documents greater-than on these two timestamps.
            if let Some((_, t)) = filter_time(&filter, "subscriptions.email.marketing.suppression.timestamp") {
                items.retain(|p| p["attributes"]["subscriptions"]["email"]["marketing"]["suppression"].as_array().is_some_and(|s| s.iter().any(|x| time_of(&x["timestamp"]) > t)));
            }
            if let Some((_, t)) = filter_time(&filter, "subscriptions.email.marketing.list_suppressions.timestamp") {
                items.retain(|p| p["attributes"]["subscriptions"]["email"]["marketing"]["list_suppressions"].as_array().is_some_and(|s| s.iter().any(|x| time_of(&x["timestamp"]) > t)));
            }
            if let Some(list) = filter.strip_prefix("any(id,[").and_then(|r| r.strip_suffix("])")) {
                let ids: Vec<String> = list.split(',').map(|x| x.trim_matches(['"', '\'']).to_string()).collect();
                items = arr("profiles").into_iter().chain(arr("hidden_profiles")).filter(|p| ids.iter().any(|i| p["id"] == i.as_str())).collect();
            }
            let items = items.into_iter().map(|p| profile_res(&ds, p["id"].as_str().unwrap(), subs).unwrap()).collect();
            page_of(&base, &path, &q, items, 3)
        }
        ("GET", ["profiles", id]) => match profile_res(&ds, id, subs) {
            Some(p) => json!({"data": p}),
            None => return err(404, "not_found", "no profile"),
        },
        ("GET", ["lists"]) => page_of(&base, &path, &q, arr("lists"), 10),
        ("GET", [kind @ ("lists" | "segments"), id]) => {
            let Some(mut l) = arr(kind).into_iter().find(|l| l["id"] == *id) else { return err(404, "not_found", "") };
            let af = if *kind == "lists" { "additional-fields[list]" } else { "additional-fields[segment]" };
            if p.get(af).is_some_and(|v| v == "profile_count") {
                l["attributes"]["profile_count"] = ds["counts"][*id].clone();
            }
            json!({"data": l})
        }
        ("GET", [kind @ ("lists" | "segments"), id, "relationships", "profiles"]) => {
            let _ = kind;
            let items: Vec<Value> = ds["members"][*id].as_array().cloned().unwrap_or_default().iter().map(|m| json!({"type": "profile", "id": m[0]})).collect();
            page_of(&base, &path, &q, items, 100)
        }
        ("GET", [kind @ ("lists" | "segments"), id, "profiles"]) => {
            let members = ds["members"][*id].as_array().cloned().unwrap_or_default();
            let since = filter_time(&filter, "joined_group_at");
            let items: Vec<Value> = members
                .iter()
                .filter(|m| since.is_none_or(|(_, t)| time_of(&m[1]) > t))
                .filter_map(|m| {
                    let mut r = profile_res(&ds, m[0].as_str()?, subs)?;
                    r["attributes"]["joined_group_at"] = m[1].clone();
                    Some(r)
                })
                .collect();
            let _ = kind;
            page_of(&base, &path, &q, items, 3)
        }
        ("GET", ["segments"]) => page_of(&base, &path, &q, arr("segments"), 10),
        ("GET", ["metrics"]) => page_of(&base, &path, &q, arr("metrics"), 200),
        ("GET", ["events"]) => {
            let mut items = arr("events");
            if let Some(m) = filter.split("equals(metric_id,").nth(1).and_then(|r| r.split(')').next()) {
                let m = m.trim_matches(['"', '\'']);
                items.retain(|e| e["relationships"]["metric"]["data"]["id"] == m);
            }
            if let Some((inclusive, t)) = filter_time(&filter, "datetime") {
                items.retain(|e| {
                    let d = time_of(&e["attributes"]["datetime"]);
                    if inclusive { d >= t } else { d > t }
                });
            }
            items.sort_by_key(|e| time_of(&e["attributes"]["datetime"]));
            page_of(&base, &path, &q, items, 3)
        }
        ("GET", ["templates"]) => page_of(&base, &path, &q, arr("templates"), 10),
        ("GET", ["templates", id]) => match arr("templates").into_iter().chain(arr("campaign_templates")).find(|t| t["id"] == *id) {
            Some(t) => json!({"data": t}),
            None => return err(404, "not_found", ""),
        },
        ("GET", ["campaigns"]) => {
            let mut pg = page_of(&base, &path, &q, arr("campaigns"), 100);
            pg["included"] = json!(arr("campaign_messages").into_iter().map(|mut m| {
                m.as_object_mut().unwrap().remove("variations");
                m
            }).chain(arr("audiences")).collect::<Vec<_>>());
            pg
        }
        ("GET", ["campaign-messages", id, "campaign-audience"]) => {
            let Some(msg) = arr("campaign_messages").into_iter().find(|m| m["id"] == *id) else { return err(404, "not_found", "") };
            let aid = msg["relationships"]["campaign-audience"]["data"]["id"].clone();
            match arr("audiences").into_iter().find(|a| a["id"] == aid) {
                Some(a) => json!({"data": a}),
                None => return err(404, "not_found", ""),
            }
        }
        ("GET", ["campaign-messages", id]) => {
            let Some(mut msg) = arr("campaign_messages").into_iter().find(|m| m["id"] == *id) else { return err(404, "not_found", "") };
            let vars = msg.as_object_mut().unwrap().remove("variations").unwrap_or(json!([]));
            json!({"data": msg, "included": vars})
        }
        ("POST", ["campaign-values-reports"]) => json!({"data": {"type": "campaign-values-report", "id": "r1", "attributes": {"results": ds["campaign_stats"]}}, "links": {"self": format!("{base}{path}")}}),
        ("POST", ["metric-aggregates"]) => {
            let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let a = &b["data"]["attributes"];
            let mid = a["metric_id"].as_str().unwrap_or("");
            let f: Vec<String> = a["filter"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
            let start = f.iter().find_map(|x| filter_time(x, "datetime")).map(|x| x.1);
            let end = f.iter().find_map(|x| x.strip_prefix("less-than(datetime,").and_then(|t| t.strip_suffix(')')).and_then(|t| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S").ok()).map(|n| n.and_utc()));
            let mut months: std::collections::BTreeMap<String, (f64, f64)> = Default::default();
            for e in arr("events") {
                if e["relationships"]["metric"]["data"]["id"] != mid {
                    continue;
                }
                let d = time_of(&e["attributes"]["datetime"]);
                if start.is_some_and(|s| d < s) || end.is_some_and(|x| d >= x) {
                    continue;
                }
                let m = months.entry(d.format("%Y-%m-01T00:00:00+00:00").to_string()).or_default();
                m.0 += 1.0;
                m.1 += e["attributes"]["event_properties"]["$value"].as_f64().unwrap_or(0.0);
            }
            json!({"data": {"type": "metric-aggregate", "id": "agg", "attributes": {
                "dates": months.keys().collect::<Vec<_>>(),
                "data": [{"dimensions": [], "measurements": {"count": months.values().map(|v| v.0).collect::<Vec<_>>(), "sum_value": months.values().map(|v| v.1).collect::<Vec<_>>()}}]}}})
        }
        ("GET", ["flows"]) => {
            let items = arr("flows").into_iter().map(|mut f| {
                f["attributes"].as_object_mut().unwrap().remove("definition");
                f
            }).collect();
            page_of(&base, &path, &q, items, 50)
        }
        ("GET", ["flows", id]) => {
            let Some(mut f) = arr("flows").into_iter().find(|f| f["id"] == *id) else { return err(404, "not_found", "") };
            if p.get("additional-fields[flow]").map(String::as_str) != Some("definition") {
                f["attributes"].as_object_mut().unwrap().remove("definition");
            }
            json!({"data": f})
        }
        ("GET", ["forms"]) => {
            let items = arr("forms").into_iter().map(|mut f| {
                f["attributes"].as_object_mut().unwrap().remove("definition");
                f
            }).collect();
            page_of(&base, &path, &q, items, 100)
        }
        ("GET", ["forms", id]) => match arr("forms").into_iter().find(|f| f["id"] == *id) {
            Some(f) => json!({"data": f}),
            None => return err(404, "not_found", ""),
        },
        ("GET", ["catalog-items"]) => return err(403, "permission_denied", "Your API key is missing required scopes: catalogs:read"),
        ("GET", ["coupons"]) => return err(403, "permission_denied", "Your API key is missing required scopes: coupons:read"),
        _ => return err(404, "not_found", &format!("no route {path}")),
    };
    axum::Json(out).into_response()
}

fn ago(days: i64) -> String {
    (Utc::now() - Duration::days(days)).to_rfc3339()
}

fn prof(id: &str, email: Option<&str>, consent: &str, can: bool, sup: &[&str], extra: Value) -> Value {
    let mut attrs = json!({
        "email": email, "phone_number": null, "external_id": null, "first_name": id, "last_name": "Test",
        "created": "2023-01-05T10:00:00+00:00", "updated": ago(10), "properties": {}, "location": {},
        "subscriptions": {
            "email": {"marketing": {"can_receive_email_marketing": can, "consent": consent, "consent_timestamp": "2023-01-05T10:00:00+00:00", "method": "FORM", "method_detail": "Footer",
                "suppression": sup.iter().map(|r| json!({"reason": r, "timestamp": "2024-02-01T00:00:00+00:00"})).collect::<Vec<_>>(), "list_suppressions": []}},
            "sms": {"marketing": {"can_receive_sms_marketing": false, "consent": "NEVER_SUBSCRIBED"}}
        }
    });
    if let Value::Object(o) = extra {
        for (k, v) in o {
            if k == "sms" {
                attrs["subscriptions"]["sms"]["marketing"] = v;
            } else if k == "list_suppressions" {
                attrs["subscriptions"]["email"]["marketing"]["list_suppressions"] = v;
            } else {
                attrs[k] = v;
            }
        }
    }
    json!({"type": "profile", "id": id, "attributes": attrs})
}

fn event(id: &str, metric: &str, profile: &str, days_ago: i64, props: Value) -> Value {
    let t = Utc::now() - Duration::days(days_ago);
    json!({"type": "event", "id": id, "attributes": {"timestamp": t.timestamp(), "datetime": t.to_rfc3339(), "uuid": format!("u-{id}"), "event_properties": props},
        "relationships": {"profile": {"data": {"type": "profile", "id": profile}}, "metric": {"data": {"type": "metric", "id": metric}}}})
}

fn dataset() -> Value {
    let profiles = vec![
        prof("P1", Some("ann@example.com"), "SUBSCRIBED", true, &[], json!({"phone_number": "+15035550101", "properties": {"vip": true, "Favorite Color": "blue"},
            "location": {"city": "Portland", "country": "United States", "region": "OR", "zip": "97201", "timezone": "America/Los_Angeles", "ip": "203.0.113.9"},
            "sms": {"can_receive_sms_marketing": true, "consent": "SUBSCRIBED", "method": "TEXT", "consent_timestamp": "2024-03-01T00:00:00+00:00"}})),
        prof("P2", Some("bob@example.com"), "NEVER_SUBSCRIBED", true, &[], json!({})),
        prof("P3", Some("cat@example.com"), "UNSUBSCRIBED", false, &["UNSUBSCRIBE"], json!({"list_suppressions": [{"list_id": "L1", "reason": "USER_SUPPRESSED", "timestamp": "2024-01-01T00:00:00+00:00"}]})),
        prof("P4", Some("dan@example.com"), "SUBSCRIBED", false, &["HARD_BOUNCE"], json!({"properties": {"vip": true}})),
        prof("P5", Some("eve@example.com"), "SUBSCRIBED", false, &["SPAM_COMPLAINT"], json!({})),
        prof("P6", Some("fay@example.com"), "SUBSCRIBED", false, &["USER_SUPPRESSED"], json!({})),
        prof("P7", Some("gus@example.com"), "SUBSCRIBED", false, &[], json!({})),
        prof("P8", None, "NEVER_SUBSCRIBED", false, &[], json!({"phone_number": "+15035550108", "sms": {"can_receive_sms_marketing": false, "consent": "SUBSCRIBED", "method": "TEXT"}})),
        prof("P9", None, "NEVER_SUBSCRIBED", false, &[], json!({})),
        prof("P10", Some("pre@example.com"), "SUBSCRIBED", true, &[], json!({})),
        prof("P11", Some("ivy@example.com"), "SUBSCRIBED", true, &[], json!({"external_id": "X11"})),
    ];
    let welcome_html = "<html><head><style>p{margin:0}</style></head><body><p>Hi {{ person.first_name|default:\"there\" }}</p><p>{% web_view %}</p>{% unsubscribe %}</body></html>";
    json!({
        "throttle_once": true,
        "profiles": profiles,
        // Created in Klaviyo after the profile list was read; found through an event.
        "hidden_profiles": [prof("P12", Some("zed@example.com"), "NEVER_SUBSCRIBED", true, &[], json!({}))],
        "lists": [
            {"type": "list", "id": "L1", "attributes": {"name": "Newsletter", "opt_in_process": "single_opt_in", "created": "2023-01-01T00:00:00+00:00"}},
            {"type": "list", "id": "L2", "attributes": {"name": "VIP", "opt_in_process": "double_opt_in", "created": "2023-01-01T00:00:00+00:00"}}
        ],
        "members": {
            "L1": [["P1", "2024-01-01T00:00:00+00:00"], ["P2", "2024-01-02T00:00:00+00:00"], ["P3", "2024-01-03T00:00:00+00:00"]],
            "L2": [["P1", "2024-02-01T00:00:00+00:00"], ["P4", "2024-02-02T00:00:00+00:00"]],
            "S2": [["P1", "2024-02-01T00:00:00+00:00"], ["P2", "2024-02-01T00:00:00+00:00"]]
        },
        "counts": {"L1": 3, "L2": 2, "S1": 2, "S2": 2},
        "segments": [
            {"type": "segment", "id": "S1", "attributes": {"name": "VIPs", "definition": {"condition_groups": [{"conditions": [
                {"type": "profile-property", "property": "properties['vip']", "filter": {"type": "boolean", "operator": "equals", "value": true}}]}]}}},
            {"type": "segment", "id": "S2", "attributes": {"name": "Likely to churn", "definition": {"condition_groups": [{"conditions": [
                {"type": "profile-predictive-analytics", "dimension": "churn_probability", "filter": {"type": "numeric", "operator": "greater-than", "value": 0.5}}]}]}}}
        ],
        "metrics": [
            {"type": "metric", "id": "MPO", "attributes": {"name": "Placed Order", "integration": {"name": "Shopify", "category": "eCommerce"}}},
            {"type": "metric", "id": "MRE", "attributes": {"name": "Received Email", "integration": {"name": "Klaviyo"}}},
            {"type": "metric", "id": "MVP", "attributes": {"name": "Viewed Page", "integration": {"name": "Klaviyo"}}}
        ],
        "events": [
            event("E1", "MPO", "P1", 30, json!({"$value": 50.0, "OrderId": "1001"})),
            event("E2", "MPO", "P1", 400, json!({"$value": 20.5})),
            event("E3", "MPO", "P4", 31, json!({"$value": 10.0})),
            event("E4", "MPO", "P1", 800, json!({"$value": 99.0})),
            event("E5", "MRE", "P1", 40, json!({"$message": "CM1", "Subject": "Hi"})),
            event("E6", "MRE", "P2", 40, json!({"$message": "CM1"})),
            event("E7", "MVP", "P1", 5, json!({"url": "https://acme.test"})),
            event("E8", "MPO", "P12", 20, json!({"$value": 5.25}))
        ],
        "templates": [{"type": "template", "id": "T1", "attributes": {"name": "Welcome", "editor_type": "CODE", "html": welcome_html, "updated": ago(100)}}],
        "campaign_templates": [{"type": "template", "id": "TC1", "attributes": {"name": "Spring copy", "html": "<p>Spring sale, {{ first_name|default:'friend' }}!</p>"}}],
        "campaigns": [
            {"type": "campaign", "id": "C1", "attributes": {"created_at": ago(45), "updated_at": ago(40), "definition": {"name": "Spring Sale", "archived": false}},
             "relationships": {"campaign-messages": {"data": [{"type": "campaign-message", "id": "CM1"}]}, "campaign-audiences": {"data": [{"type": "campaign-audience", "id": "AUD1"}]}}},
            {"type": "campaign", "id": "C2", "attributes": {"created_at": ago(2), "updated_at": ago(2), "definition": {"name": "Draft promo", "archived": false}},
             "relationships": {"campaign-messages": {"data": [{"type": "campaign-message", "id": "CM2"}]}, "campaign-audiences": {"data": [{"type": "campaign-audience", "id": "AUD2"}]}}}
        ],
        "audiences": [
            {"type": "campaign-audience", "id": "AUD1", "attributes": {"definition": {"name": "Main", "included": ["L1"], "excluded": ["S1"], "priority": 1}}},
            {"type": "campaign-audience", "id": "AUD2", "attributes": {"definition": {"name": "Main", "included": ["S2"], "excluded": [], "priority": 1}}}
        ],
        "campaign_messages": [
            {"type": "campaign-message", "id": "CM1", "attributes": {"definition": {"name": "Email", "status": "sent", "send_options": {"send_time": ago(40)}}, "updated": ago(40)},
             "relationships": {"campaign": {"data": {"type": "campaign", "id": "C1"}}, "campaign-audience": {"data": {"type": "campaign-audience", "id": "AUD1"}}, "campaign-variations": {"data": [{"type": "campaign-variation", "id": "V1"}]}},
             "variations": [{"type": "campaign-variation", "id": "V1", "attributes": {"definition": {"name": "A", "details": {"channel": "email", "template_id": "TC1", "subject": "Hi {{ first_name|default:'friend' }}", "preview_text": "Spring", "from_label": "Acme", "from_email": "hello@acme.test"}}}}]},
            {"type": "campaign-message", "id": "CM2", "attributes": {"definition": {"name": "Text", "status": "scheduled", "send_options": {"send_time": ago(-3)}}},
             "relationships": {"campaign": {"data": {"type": "campaign", "id": "C2"}}, "campaign-audience": {"data": {"type": "campaign-audience", "id": "AUD2"}}, "campaign-variations": {"data": [{"type": "campaign-variation", "id": "V2"}]}},
             "variations": [{"type": "campaign-variation", "id": "V2", "attributes": {"definition": {"name": "A", "details": {"channel": "sms", "body": "Sale today {{ first_name }}! {% unsubscribe_url %}"}}}}]}
        ],
        "campaign_stats": [{"groupings": {"send_channel": "email", "campaign_id": "C1", "campaign_message_id": "CM1"}, "statistics": {"recipients": 2, "delivered": 2, "opens_unique": 1, "clicks_unique": 1, "conversions": 1, "conversion_value": 50.0}}],
        "flows": [
            {"type": "flow", "id": "F1", "attributes": {"name": "Welcome Series", "status": "live", "archived": false, "trigger_type": "Added to List", "definition": {
                "triggers": [{"type": "list", "id": "L1"}], "entry_action_id": "a1",
                "actions": [
                    {"id": "a1", "type": "send-email", "links": {"next": "a2"}, "data": {"status": "live", "message": {"name": "Welcome 1", "subject_line": "Welcome, {{ first_name|default:'friend' }}", "template_id": "T1", "smart_sending_enabled": true}}},
                    {"id": "a2", "type": "time-delay", "links": {"next": "a3"}, "data": {"unit": "days", "value": 2}},
                    {"id": "a3", "type": "send-mobile-push", "links": {"next": "a4"}, "data": {"message": {"body": "Hi"}}},
                    {"id": "a4", "type": "conditional-split", "links": {"next_if_true": "a5", "next_if_false": null}, "data": {"profile_filter": {"condition_groups": [{"conditions": [{"type": "profile-predictive-analytics", "dimension": "churn_probability"}]}]}}},
                    {"id": "a5", "type": "send-sms", "links": {"next": null}, "data": {"status": "live", "message": {"body": "Thanks {{ first_name }}!"}}}
                ]}}},
            {"type": "flow", "id": "F2", "attributes": {"name": "Price drop", "status": "live", "archived": false, "trigger_type": "Price Drop", "definition": {"triggers": [{"type": "price-drop"}], "actions": []}}}
        ],
        "forms": [{"type": "form", "id": "FORM1", "attributes": {"name": "Footer signup", "status": "live", "ab_test": false,
            "definition": {"versions": [{"status": "live", "steps": [{"columns": [{"rows": [{"components": [
                {"type": "button", "action": {"type": "next_step", "submit": true, "properties": {"list_id": "L1"}}},
                {"type": "button", "action": {"type": "close", "submit": false, "properties": {"list_id": "L2"}}}]}]}]}]}]}}}]
    })
}

async fn register(ds: Value) -> String {
    let key = format!("pk_test_{}", uuid::Uuid::new_v4().simple());
    mock().1.lock().unwrap().sets.insert(key.clone(), ds);
    key
}

fn calls(key: &str) -> Vec<String> {
    mock().1.lock().unwrap().calls.get(key).cloned().unwrap_or_default()
}

async fn count(h: &Harness, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&h.state.db).await.unwrap()
}

/// Run queued jobs, including ones put off by a Retry-After, until the import settles.
async fn settle(h: &Harness) {
    for _ in 0..40 {
        h.fast_forward().await;
        let busy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind LIKE 'klaviyo_%' AND run_at <> 'infinity')").fetch_one(&h.state.db).await.unwrap();
        if !busy {
            return;
        }
    }
    panic!("import didn't settle");
}

fn line<'a>(report: &'a Value, section: &str, label: &str) -> &'a Value {
    report["lines"].as_array().unwrap().iter().find(|l| l["section"] == section && l["label"] == label).unwrap_or_else(|| panic!("no line {section}/{label} in {report}"))
}

#[tokio::test]
async fn imports_everything_and_proves_it() {
    let key = register(dataset()).await;
    let mut h = harness!();
    h.signup().await;
    // Someone who already unsubscribed in DumbMail stays unsubscribed, whatever Klaviyo says.
    h.ok("POST", "/api/profiles", Some(json!({"email": "pre@example.com", "email_consent": "unsubscribed"}))).await;
    // A live flow on orders must not start for imported order history.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"},
        "definition": {"entry": "e", "nodes": [{"id": "e", "type": "email", "content": {"subject": "Thanks", "blocks": [{"type": "text", "html": "<p>Thanks</p>"}]}, "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live", "confirm": true}))).await;
    let ledger_before = count(&h, "SELECT count(*) FROM ledger").await;

    // Bad keys are refused before anything is stored.
    let (s, v) = h.req("POST", "/api/klaviyo/connect", Some(json!({"api_key": "ABC123"}))).await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = h.req("POST", "/api/klaviyo/connect", Some(json!({"api_key": "pk_wrong"}))).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("didn't accept"), "{v}");

    let st = h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    assert_eq!(st["import"]["status"], "surveying");
    assert!(st["import"]["key_hint"].as_str().unwrap().starts_with("pk_…"));
    assert!(!st.to_string().contains(&key), "the key must never come back");
    let stored: String = sqlx::query_scalar("SELECT api_key_enc FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap();
    assert!(!stored.contains("pk_test"), "stored encrypted");
    let (s, _) = h.req("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    assert_eq!(s, 409, "one import at a time");

    settle(&h).await;
    let st = h.ok("GET", "/api/klaviyo", None).await;
    let sv = &st["import"]["survey"];
    assert_eq!(st["import"]["status"], "ready", "{st}");
    assert_eq!(sv["profiles"], json!({"count": 11, "exact": true}));
    assert_eq!(sv["lists"]["count"], 2);
    assert_eq!(sv["segments"]["count"], 2);
    assert_eq!(sv["flows"]["count"], 2);
    assert_eq!(sv["forms"]["count"], 1);
    assert_eq!(sv["templates"]["count"], 1);
    assert_eq!(sv["campaigns"]["count"], 2);
    assert_eq!(sv["metrics"]["count"], 3);
    assert_eq!(sv["suppressions"]["by_reason"]["hard_bounce"]["count"], 1);
    assert_eq!(sv["catalog_items"]["error"], "no access");
    assert_eq!(sv["missing_scopes"], json!(["catalogs:read", "coupons:read"]));
    assert!(sv["events"]["estimate"].as_f64().unwrap() >= 4.0, "{sv}");
    assert!(sv["estimate_minutes"].as_f64().unwrap() >= 1.0);
    // The 429 was waited out, then retried.
    let c = calls(&key);
    assert!(c.iter().filter(|x| x.starts_with("GET /api/profiles?")).count() >= 2);

    // Without confirm: a dry run, nothing starts.
    let dry = h.ok("POST", "/api/klaviyo/start", Some(json!({}))).await;
    assert_eq!(dry["dry_run"], true);
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'klaviyo_import_run'").await, 0);
    let (s, _) = h.req("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": ["nope"]}))).await;
    assert_eq!(s, 400);
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true}))).await;
    settle(&h).await;

    let st = h.ok("GET", "/api/klaviyo", None).await;
    let imp = &st["import"];
    assert_eq!(imp["status"], "done", "{st}");
    assert_eq!(imp["key_stored"], false, "the key is deleted when the import finishes");
    assert_eq!(count(&h, "SELECT count(*) FROM klaviyo_imports WHERE api_key_enc IS NOT NULL").await, 0);
    assert_eq!(imp["progress"]["profiles"]["done"], 10, "{}", imp["progress"]);
    assert_eq!(imp["progress"]["profiles"]["skipped"], 1);

    // Consent, case by case.
    let consent = |email: &'static str| {
        let db = h.state.db.clone();
        async move {
            sqlx::query_as::<_, (String, String, Option<String>)>("SELECT email_consent, sms_consent, email_suppressed_reason FROM profiles WHERE email = $1")
                .bind(email)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(consent("ann@example.com").await, ("subscribed".into(), "subscribed".into(), None));
    assert_eq!(consent("bob@example.com").await, ("never_subscribed".into(), "never_subscribed".into(), None));
    assert_eq!(consent("cat@example.com").await, ("unsubscribed".into(), "never_subscribed".into(), None));
    assert_eq!(consent("dan@example.com").await, ("subscribed".into(), "never_subscribed".into(), Some("hard_bounce".into())));
    assert_eq!(consent("eve@example.com").await, ("unsubscribed".into(), "never_subscribed".into(), Some("complaint".into())));
    assert_eq!(consent("fay@example.com").await, ("subscribed".into(), "never_subscribed".into(), Some("manual".into())));
    assert_eq!(consent("gus@example.com").await, ("subscribed".into(), "never_subscribed".into(), Some("klaviyo_suppressed".into())));
    assert_eq!(consent("pre@example.com").await.0, "unsubscribed");
    assert_eq!(consent("zed@example.com").await.0, "never_subscribed");
    let hal: (String, Option<String>) = sqlx::query_as("SELECT sms_consent, email FROM profiles WHERE phone_number = '+15035550108'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(hal, ("never_subscribed".into(), None), "subscribed in Klaviyo but not textable there: never textable here");
    // Properties, location, Klaviyo id and consent record; original sign-up date.
    let ann: (Value, chrono::DateTime<Utc>, Option<String>) = sqlx::query_as("SELECT properties, created_at, phone_number FROM profiles WHERE email = 'ann@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ann.0["Favorite Color"], "blue");
    assert_eq!(ann.0["country"], "US");
    assert_eq!(ann.0["city"], "Portland");
    assert_eq!(ann.0["klaviyo_id"], "P1");
    assert_eq!(ann.0["klaviyo_consent"]["email"]["method"], "FORM");
    assert_eq!(ann.0["klaviyo_consent"]["email"]["at"], "2023-01-05T10:00:00+00:00");
    assert!(ann.0.get("ip").is_none());
    assert_eq!(ann.1.format("%Y-%m-%d").to_string(), "2023-01-05");
    assert_eq!(ann.2.as_deref(), Some("+15035550101"));

    // Lists, with their opt-in setting; list unsubscribes stay off the list.
    let lists: Vec<(String, bool, i64)> = sqlx::query_as("SELECT l.name, l.double_opt_in, (SELECT count(*) FROM list_members m WHERE m.list_id = l.id) FROM lists l ORDER BY l.name").fetch_all(&h.state.db).await.unwrap();
    assert!(lists.contains(&("Newsletter".into(), false, 2)), "{lists:?}");
    assert!(lists.contains(&("VIP".into(), true, 2)), "{lists:?}");
    assert!(lists.iter().any(|(n, _, c)| n.starts_with("Likely to churn (Klaviyo segment snapshot") && *c == 2), "{lists:?}");
    // Segments: rules where they translate, snapshot (with the reason) where they don't.
    let seg: Value = sqlx::query_scalar("SELECT definition FROM segments WHERE name = 'VIPs'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(seg["conditions"][0], json!({"type": "property", "field": "vip", "op": "eq", "value": true}));
    let snap: String = sqlx::query_scalar("SELECT message FROM klaviyo_import_issues WHERE kind = 'snapshot'").fetch_one(&h.state.db).await.unwrap();
    assert!(snap.contains("predictive analytics"), "{snap}");

    // Events: the chosen metrics in the last two years, deduplicated, tied to campaigns; nothing woke up.
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Placed Order'").await, 4);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Received Email'").await, 2);
    assert_eq!(count(&h, "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE m.name = 'Viewed Page'").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM flow_runs").await, 0, "imported history must not start flows");
    assert_eq!(count(&h, "SELECT count(*) FROM messages").await, 0, "nobody is messaged");
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind IN ('flow_event', 'flow_list')").await, 0);
    assert_eq!(count(&h, "SELECT count(*) FROM ledger").await, ledger_before, "nothing billed");
    let revenue: f64 = sqlx::query_scalar("SELECT sum(value) FROM events").fetch_one(&h.state.db).await.unwrap();
    assert!((revenue - 85.75).abs() < 1e-9, "{revenue}");

    // Campaigns: the sent one as history with Klaviyo's numbers, the scheduled one as a draft.
    let camps: Vec<(uuid::Uuid, String, String, String, Value, Value)> = sqlx::query_as("SELECT id, name, status, channel, content, audience FROM campaigns ORDER BY created_at").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(camps.len(), 2);
    let (spring_id, _, status, channel, content, audience) = &camps[0];
    assert_eq!((status.as_str(), channel.as_str()), ("sent", "email"));
    assert_eq!(content["subject"], "Hi {{ first_name|default('friend') }}");
    assert_eq!(content["from_name"], "Acme");
    assert_eq!(audience["include"][0]["type"], "list");
    assert_eq!(audience["exclude"][0]["type"], "segment");
    assert_eq!((camps[1].2.as_str(), camps[1].3.as_str()), ("draft", "sms"));
    assert_eq!(camps[1].4["body"], "Sale today {{ first_name }}! {{ unsubscribe_url }}");
    let hist = h.ok("GET", "/api/klaviyo/campaigns", None).await;
    assert_eq!(hist["data"][0]["stats"]["recipients"], 2, "{hist}");
    let received = h.ok("POST", "/api/segments/preview", Some(json!({"definition": {"conditions": [{"type": "message", "action": "received", "campaign_id": spring_id}]}}))).await;
    assert_eq!(received["count"], 2, "\"received campaign X\" keeps working: {received}");

    // Templates: Klaviyo tags converted, the rest removed and listed, original kept.
    let tpl: Value = sqlx::query_scalar("SELECT content FROM templates WHERE name = 'Welcome'").fetch_one(&h.state.db).await.unwrap();
    let html = tpl["html"].as_str().unwrap();
    assert!(html.contains("{{ person.first_name|default('there') }}") && html.contains("{{ unsubscribe_url }}") && !html.contains("web_view"), "{html}");
    assert!(tpl["imported"]["original_html"].as_str().unwrap().contains("{% web_view %}"));
    let rendered = h.ok("POST", "/api/render", Some(json!({"channel": "email", "content": tpl}))).await;
    assert!(rendered["html"].as_str().unwrap().contains("<p>Hi Alex</p>") && rendered["html"].as_str().unwrap().contains("Unsubscribe</a>"), "{rendered}");

    // Flows: drafts only, partial ones labelled, unmappable ones listed.
    let flows: Vec<(String, String, Value)> = sqlx::query_as("SELECT name, status, definition FROM flows WHERE name <> 'Thanks'").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(flows.len(), 1, "{flows:?}");
    assert_eq!(flows[0].0, "Welcome Series (partial import)");
    assert_eq!(flows[0].1, "draft");
    assert_eq!(flows[0].2["nodes"].as_array().unwrap().len(), 4);
    let skipped: String = sqlx::query_scalar("SELECT message FROM klaviyo_import_issues WHERE step = 'flows' AND kind = 'skipped'").fetch_one(&h.state.db).await.unwrap();
    assert!(skipped.contains("price-drop"), "{skipped}");
    // Forms: the name and its list, as a draft.
    let form: (String, String, bool) = sqlx::query_as("SELECT f.name, f.status, f.list_id = (SELECT id FROM lists WHERE name = 'Newsletter') FROM forms f").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(form, ("Footer signup".into(), "draft".into(), true));

    // The comparison report: every line matches, with the numbers.
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(r["all_ok"], true, "{}", serde_json::to_string_pretty(&r["lines"]).unwrap());
    assert_eq!(line(&r, "Profiles", "Profiles")["klaviyo"], 11, "Klaviyo's own total from the full scan");
    assert_eq!(line(&r, "Profiles", "Profiles")["state"], "match");
    assert_eq!(line(&r, "Email", "Subscribed in Klaviyo")["klaviyo"], 3);
    assert_eq!(line(&r, "Email", "Subscribed in Klaviyo")["dumbmail"], 2, "one unsubscribed in DumbMail first, and that wins");
    assert_eq!(line(&r, "Email", "Unsubscribed in Klaviyo")["klaviyo"], 2);
    assert_eq!(line(&r, "Email", "Suppressed in Klaviyo (bounced, complained, invalid, by hand, or not emailable)")["dumbmail"], 3);
    assert_eq!(line(&r, "Email", "More reachable here than in Klaviyo")["dumbmail"], 0);
    assert_eq!(line(&r, "SMS", "More textable here than in Klaviyo")["dumbmail"], 0);
    assert_eq!(line(&r, "List opt-outs", "Unsubscribed from a list in Klaviyo")["klaviyo"], 1);
    assert_eq!(line(&r, "List opt-outs", "Unsubscribed from a list in Klaviyo")["state"], "match");
    assert_eq!(line(&r, "Lists", "Newsletter")["klaviyo"], 3);
    assert_eq!(line(&r, "Lists", "Newsletter")["dumbmail"], 2);
    assert_eq!(line(&r, "Segments", "VIPs")["dumbmail"], 2);
    assert_eq!(line(&r, "Events", "Placed Order")["klaviyo"], 4);
    assert_eq!(line(&r, "Events", "Placed Order")["dumbmail"], 4);
    assert!(r["lines"].as_array().unwrap().iter().any(|l| l["section"] == "Revenue by month"));
    let (s, hd, csv) = h.raw("GET", "/api/klaviyo/report.csv", &[("cookie", h.cookie.clone().unwrap().as_str())], "").await;
    assert_eq!(s, 200);
    assert!(hd["content-type"].to_str().unwrap().starts_with("text/csv"));
    assert!(csv.starts_with("Section,What,Klaviyo,DumbMail,Result,Note") && csv.contains("Newsletter,3,2,MATCH"), "{csv}");
    let issues = h.ok("GET", "/api/klaviyo/issues", None).await;
    assert!(issues["total"].as_i64().unwrap() >= 4, "{issues}");

    // Checklist: from live data.
    let cl = &h.ok("GET", "/api/klaviyo", None).await["checklist"];
    let item = |k: &str| cl["items"].as_array().unwrap().iter().find(|i| i["key"] == k).unwrap().clone();
    assert_eq!(item("suppressions")["done"], true);
    assert_eq!(item("sending_domain")["done"], false);
    assert_eq!(item("test_send")["done"], false);
    let (s, _) = h.req("POST", "/api/klaviyo/checklist", Some(json!({"item": "test_send", "done": true}))).await;
    assert_eq!(s, 400, "automatic items can't be ticked by hand");
    let st = h.ok("POST", "/api/klaviyo/checklist", Some(json!({"item": "warmup", "done": true}))).await;
    assert_eq!(st["checklist"]["items"].as_array().unwrap().iter().find(|i| i["key"] == "warmup").unwrap()["done"], true);

    // Running it again adds only what's new.
    let profiles_before = count(&h, "SELECT count(*) FROM profiles").await;
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true}))).await;
    settle(&h).await;
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "done");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, profiles_before);
    assert_eq!(count(&h, "SELECT count(*) FROM events").await, 6);
    assert_eq!(count(&h, "SELECT count(*) FROM campaigns").await, 2);
    assert_eq!(count(&h, "SELECT count(*) FROM flows").await, 2);
    assert_eq!(count(&h, "SELECT count(*) FROM segments").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM lists").await, 3);
    assert_eq!(count(&h, "SELECT count(*) FROM list_members").await, 6);
    assert_eq!(h.ok("GET", "/api/klaviyo/report", None).await["all_ok"], true);
    h.cleanup().await;
}

#[tokio::test]
async fn ongoing_sync_brings_unsubscribes_and_then_deletes_the_key() {
    let mut ds = dataset();
    ds["throttle_once"] = json!(false);
    let key = register(ds).await;
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": ["lists", "events"], "sync": true}))).await;
    settle(&h).await;
    let st = h.ok("GET", "/api/klaviyo", None).await;
    assert_eq!(st["import"]["status"], "done");
    assert_eq!(st["import"]["key_stored"], true, "kept while the sync is on");
    assert_eq!(count(&h, "SELECT count(*) FROM segments").await, 0, "segments weren't chosen");
    assert_eq!(count(&h, "SELECT count(*) FROM campaigns").await, 0);

    // In Klaviyo: Ann unsubscribes from email, Bob gets texts, a new order and a new list member.
    {
        let mut g = mock().1.lock().unwrap();
        let d = g.sets.get_mut(&key).unwrap();
        let profiles = d["profiles"].as_array_mut().unwrap();
        let ann = profiles.iter_mut().find(|p| p["id"] == "P1").unwrap();
        ann["attributes"]["subscriptions"]["email"]["marketing"] = json!({"can_receive_email_marketing": false, "consent": "UNSUBSCRIBED", "suppression": [{"reason": "UNSUBSCRIBE", "timestamp": Utc::now().to_rfc3339()}]});
        ann["attributes"]["updated"] = json!(ago(-1));
        let unsub_event = event("E20", "MRE", "P2", 0, json!({}));
        d["events"].as_array_mut().unwrap().push(event("E21", "MPO", "P2", 0, json!({"$value": 12.0})));
        d["events"].as_array_mut().unwrap().push(unsub_event);
        d["members"]["L2"].as_array_mut().unwrap().push(json!(["P2", ago(-1)]));
    }
    let (s, _) = h.req("POST", "/api/klaviyo/sync-now", None).await;
    assert_eq!(s, 200);
    let (s, _) = h.req("POST", "/api/klaviyo/sync-now", None).await;
    assert_eq!(s, 409, "already running");
    settle(&h).await;
    let ann: (String,) = sqlx::query_as("SELECT email_consent FROM profiles WHERE email = 'ann@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ann.0, "unsubscribed");
    assert_eq!(count(&h, "SELECT count(*) FROM events WHERE unique_id = 'klaviyo:E21'").await, 1);
    assert_eq!(count(&h, "SELECT count(*) FROM list_members m JOIN lists l ON l.id = m.list_id WHERE l.name = 'VIP'").await, 3);
    let st = h.ok("GET", "/api/klaviyo", None).await;
    assert_eq!(st["import"]["status"], "done");
    assert!(st["import"]["last_sync_at"].is_string());
    let sync_calls = calls(&key).into_iter().filter(|c| c.starts_with("GET /api/profiles?") && c.contains("greater-than%28updated")).count();
    assert!(sync_calls >= 1, "the sync asks only for changed profiles");

    // The scheduler starts the next one once an hour has passed.
    sqlx::query("UPDATE klaviyo_imports SET last_sync_at = now() - interval '2 hours'").execute(&h.state.db).await.unwrap();
    dumbmail::jobs::scheduler_tick(&h.state, 3).await.unwrap();
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "syncing");
    settle(&h).await;

    // Switching the sync off deletes the key; sync now then explains why it can't.
    let st = h.ok("POST", "/api/klaviyo/sync", Some(json!({"enabled": false}))).await;
    assert_eq!(st["import"]["key_stored"], false);
    let (s, v) = h.req("POST", "/api/klaviyo/sync-now", None).await;
    assert_eq!(s, 409, "{v}");
    let (s, _) = h.req("POST", "/api/klaviyo/sync", Some(json!({"enabled": true}))).await;
    assert_eq!(s, 409, "no key to sync with");
    h.cleanup().await;
}

#[tokio::test]
async fn pause_cancel_isolation_and_mcp() {
    let mut ds = dataset();
    ds["throttle_once"] = json!(false);
    let key = register(ds).await;
    let mut h = harness!();
    h.signup().await;
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true}))).await;
    h.ok("POST", "/api/klaviyo/pause", None).await;
    settle(&h).await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 0, "a paused import does nothing");
    let (s, _) = h.req("POST", "/api/klaviyo/pause", None).await;
    assert_eq!(s, 409);
    h.ok("POST", "/api/klaviyo/resume", None).await;
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "importing");
    // Cancel before it runs again: it stops, and the key goes.
    let st = h.ok("POST", "/api/klaviyo/cancel", None).await;
    assert_eq!(st["import"]["status"], "cancelled");
    assert_eq!(st["import"]["key_stored"], false);
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind LIKE 'klaviyo_%'").await, 0);
    settle(&h).await;
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 0);
    let (s, _) = h.req("POST", "/api/klaviyo/cancel", None).await;
    assert_eq!(s, 409);

    // Another account sees none of it.
    let mut other = Harness { cookie: None, ..harness_like(&h) };
    other.ok("POST", "/api/auth/signup", Some(json!({"email": "two@other.test", "password": "correct horse", "company": "Other"}))).await;
    let st = other.ok("GET", "/api/klaviyo", None).await;
    assert!(st["import"].is_null(), "{st}");
    let (s, _) = other.req("GET", "/api/klaviyo/report", None).await;
    assert_eq!(s, 404);
    let (s, _) = other.req("POST", "/api/klaviyo/cancel", None).await;
    assert_eq!(s, 404);
    let (s, _) = Harness { cookie: None, ..harness_like(&h) }.req("GET", "/api/klaviyo", None).await;
    assert_eq!(s, 401);

    // MCP: the tools are listed, start without confirm is a dry run.
    let k = h.ok("POST", "/api/api-keys", Some(json!({"name": "mcp"}))).await;
    let token = k["key"].as_str().unwrap().to_string();
    let mcp = |body: Value| {
        let h = &h;
        let token = token.clone();
        async move {
            let (s, _, b) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json")], &body.to_string()).await;
            assert_eq!(s, 200);
            serde_json::from_str::<Value>(&b).unwrap()
        }
    };
    let tools = mcp(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
    let names: Vec<String> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for n in ["klaviyo_survey", "klaviyo_start_import", "klaviyo_import_status", "klaviyo_cancel_import", "klaviyo_comparison_report", "klaviyo_sync_now"] {
        assert!(names.iter().any(|x| x == n), "{n}");
    }
    let doc = mcp(json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "dumbmail://docs/moving-from-klaviyo"}})).await;
    assert!(doc["result"]["contents"][0]["text"].as_str().unwrap().contains("Read access"));
    let key2 = register({
        let mut d = dataset();
        d["throttle_once"] = json!(false);
        d
    })
    .await;
    let r = mcp(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "klaviyo_survey", "arguments": {"api_key": key2}}})).await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    settle(&h).await;
    let r = mcp(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "klaviyo_start_import", "arguments": {}}})).await;
    assert_eq!(r["result"]["structuredContent"]["dry_run"], true, "{r}");
    let r = mcp(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "klaviyo_start_import", "arguments": {"confirm": true, "events_from": "2999-01-01"}}})).await;
    assert_eq!(r["result"]["isError"], true, "future dates are refused");
    h.cleanup().await;
}

/// A second session against the same app and database.
fn harness_like(h: &Harness) -> Harness {
    Harness { state: h.state.clone(), router: h.router.clone(), cookie: None, admin: h.admin.clone(), dbname: format!("{}_unused", h.dbname) }
}

// ---------- Consent, case by case, against the database ----------

use dumbmail::features::klaviyo_import::{runner::Run, store};

/// An import row to run profile writes against (no Klaviyo calls are made).
async fn bare_import(h: &Harness, never: &str, status: &str) -> store::Import {
    let acct: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let id = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO klaviyo_imports (id, account_id, status, options, klaviyo_account) VALUES ($1, $2, $3, $4, '{\"id\": \"ACCT1\"}')")
        .bind(id)
        .bind(acct)
        .bind(status)
        .bind(json!({"never_subscribed": never}))
        .execute(&h.state.db)
        .await
        .unwrap();
    store::load(&h.state.db, id).await.unwrap().unwrap()
}

fn kres(kid: &str, email: Option<&str>, phone: Option<&str>, ext: &str, email_m: Option<Value>, sms_m: Option<Value>) -> Value {
    let mut subs = json!({});
    if let Some(e) = email_m {
        subs["email"] = json!({"marketing": e});
    }
    if let Some(s) = sms_m {
        subs["sms"] = json!({"marketing": s});
    }
    json!({"type": "profile", "id": kid, "attributes": {"email": email, "phone_number": phone, "external_id": ext, "first_name": "T", "properties": {}, "location": {}, "subscriptions": subs}})
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Dm {
    None,
    Never,
    Subscribed,
    Unsubscribed,
    Pending,
    /// Subscribed, with a hard bounce.
    Suppressed,
}

const DMS: [Dm; 6] = [Dm::None, Dm::Never, Dm::Subscribed, Dm::Unsubscribed, Dm::Pending, Dm::Suppressed];

fn dm_cols(d: Dm) -> (&'static str, Option<&'static str>) {
    match d {
        Dm::None | Dm::Never => ("never_subscribed", None),
        Dm::Subscribed => ("subscribed", None),
        Dm::Unsubscribed => ("unsubscribed", None),
        Dm::Pending => ("pending", None),
        Dm::Suppressed => ("subscribed", Some("hard_bounce")),
    }
}

/// The ruling, written out independently of the importer: (email consent, suppressed, held).
fn expect_email(state: &str, can: bool, sup: &str, dm: Dm, same: bool, hold_never: bool) -> (String, bool, bool) {
    let opt_out = state == "UNSUBSCRIBED" || sup == "UNSUBSCRIBE" || sup == "SPAM_COMPLAINT";
    let unknown = state.is_empty() || !matches!(state, "SUBSCRIBED" | "UNSUBSCRIBED" | "NEVER_SUBSCRIBED");
    let opt_in = state == "SUBSCRIBED" && !opt_out;
    // No consent record at all says nothing about suppression: such people are held instead.
    let k_sup = matches!(sup, "HARD_BOUNCE" | "SPAM_COMPLAINT" | "INVALID_EMAIL" | "USER_SUPPRESSED") || (!state.is_empty() && !can && !opt_out);
    let k_hold = !opt_out && (unknown || (state == "NEVER_SUBSCRIBED" && hold_never));
    if dm == Dm::None {
        let c = if opt_out { "unsubscribed" } else if opt_in { "subscribed" } else { "never_subscribed" };
        return (c.into(), k_sup, k_hold);
    }
    let (dc, ds) = dm_cols(dm);
    if !same {
        return (dc.into(), ds.is_some(), false);
    }
    let c = if opt_out { "unsubscribed" } else if opt_in && dc == "never_subscribed" && ds.is_none() { "subscribed" } else { dc };
    (c.into(), ds.is_some() || k_sup, k_hold && dc != "subscribed")
}

fn email_marketing(state: &str, can: bool, sup: &str) -> Option<Value> {
    if state == "-" {
        return None;
    }
    let sups: Vec<Value> = if sup.is_empty() { vec![] } else { vec![json!({"reason": sup, "timestamp": "2024-02-01T00:00:00+00:00"})] };
    Some(json!({"consent": state, "can_receive_email_marketing": can, "consent_timestamp": "2023-01-01T00:00:00+00:00", "suppression": sups, "list_suppressions": []}))
}

#[tokio::test]
async fn email_consent_every_combination() {
    let mut h = harness!();
    h.signup().await;
    // Klaviyo email states ("-" = no consent record at all, "ODD" = a state DumbMail doesn't know).
    let k_states: [(&str, bool); 7] = [("SUBSCRIBED", true), ("SUBSCRIBED", false), ("UNSUBSCRIBED", false), ("NEVER_SUBSCRIBED", true), ("NEVER_SUBSCRIBED", false), ("-", false), ("ODD", true)];
    let sups = ["", "HARD_BOUNCE", "SPAM_COMPLAINT", "INVALID_EMAIL", "USER_SUPPRESSED", "UNSUBSCRIBE"];
    let mut n = 0usize;
    let mut checked = 0usize;
    for mode in ["first", "rerun", "sync"] {
        for hold_never in [false, true] {
            let imp = bare_import(&h, if hold_never { "hold" } else { "send" }, if mode == "sync" { "syncing" } else { "importing" }).await;
            let run = Run::new(&h.state, imp.clone(), "pk_unused");
            let mut cases = Vec::new();
            for (state, can) in k_states {
                for sup in sups {
                    if state == "-" && !sup.is_empty() {
                        continue;
                    }
                    for dm in DMS {
                        for same in [true, false] {
                            if dm == Dm::None && !same {
                                continue;
                            }
                            n += 1;
                            cases.push((n, state, can, sup, dm, same));
                        }
                    }
                }
            }
            let dm_email = |n: usize| format!("dm{n}@case.test");
            let k_email = |n: usize, same: bool| if same { dm_email(n) } else { format!("k{n}@case.test") };
            // Set up DumbMail's side.
            if mode == "first" {
                for (n, _, _, _, dm, _) in &cases {
                    if *dm != Dm::None {
                        let (c, s) = dm_cols(*dm);
                        sqlx::query("INSERT INTO profiles (id, account_id, email, external_id, email_consent, email_suppressed_reason) VALUES ($1, $2, $3, $4, $5, $6)")
                            .bind(uuid::Uuid::now_v7())
                            .bind(imp.account_id)
                            .bind(dm_email(*n))
                            .bind(format!("X{n}"))
                            .bind(c)
                            .bind(s)
                            .execute(&h.state.db)
                            .await
                            .unwrap();
                    }
                }
            } else {
                // An earlier import brought each person across (neutral consent); then DumbMail's state changed.
                let first: Vec<Value> = cases.iter().map(|(n, ..)| kres(&format!("K{n}"), Some(&dm_email(*n)), None, &format!("X{n}"), email_marketing("NEVER_SUBSCRIBED", true, ""), None)).collect();
                for page in first.chunks(100) {
                    run.upsert_profiles(page).await.unwrap();
                }
                for (n, _, _, _, dm, _) in &cases {
                    if *dm == Dm::None {
                        sqlx::query("DELETE FROM profiles WHERE external_id = $1").bind(format!("X{n}")).execute(&h.state.db).await.unwrap();
                    } else {
                        let (c, s) = dm_cols(*dm);
                        sqlx::query("UPDATE profiles SET email_consent = $2, email_suppressed_reason = $3, marketing_hold = false WHERE external_id = $1").bind(format!("X{n}")).bind(c).bind(s).execute(&h.state.db).await.unwrap();
                    }
                }
            }
            // Klaviyo's side, in pages of 100 like the import.
            let pages: Vec<Value> = cases.iter().map(|(n, state, can, sup, _, same)| kres(&format!("K{n}"), Some(&k_email(*n, *same)), None, &format!("X{n}"), email_marketing(state, *can, sup), None)).collect();
            for page in pages.chunks(100) {
                run.upsert_profiles(page).await.unwrap();
            }
            for (n, state, can, sup, dm, same) in &cases {
                let got: (Option<String>, String, Option<String>, bool) = sqlx::query_as("SELECT email, email_consent, email_suppressed_reason, marketing_hold FROM profiles WHERE external_id = $1")
                    .bind(format!("X{n}"))
                    .fetch_one(&h.state.db)
                    .await
                    .unwrap();
                let want = expect_email(if *state == "-" { "" } else { state }, *can, sup, *dm, *same, hold_never);
                let case = format!("mode={mode} hold_never={hold_never} klaviyo={state}/can={can}/sup={sup} dumbmail={dm:?} same_address={same}");
                assert_eq!((got.1.clone(), got.2.is_some(), got.3), want, "{case}");
                // The invariants, checked on their own: never easier to reach than both sides allowed...
                let reach = |c: &str, s: bool, held: bool| !matches!(c, "unsubscribed" | "pending") && !s && !held;
                let k_reach = *state != "-" && *can && !matches!(*sup, "HARD_BOUNCE" | "SPAM_COMPLAINT" | "INVALID_EMAIL" | "USER_SUPPRESSED" | "UNSUBSCRIBE") && *state != "UNSUBSCRIBED" && *state != "ODD";
                let (dc, ds) = dm_cols(*dm);
                let dm_reach = *dm != Dm::None && reach(dc, ds.is_some(), false) && dc == "subscribed";
                if reach(&got.1, got.2.is_some(), got.3) {
                    assert!(k_reach || (dm_reach && *same) || !*same, "{case}: more reachable than Klaviyo and DumbMail allowed");
                }
                // ...and a restriction already in DumbMail is never lifted.
                if *dm != Dm::None {
                    match dm {
                        Dm::Unsubscribed => assert_eq!(got.1, "unsubscribed", "{case}"),
                        Dm::Pending => assert!(matches!(got.1.as_str(), "pending" | "unsubscribed"), "{case}"),
                        Dm::Suppressed => assert!(got.2.is_some(), "{case}"),
                        _ => {}
                    }
                }
                // A different address: DumbMail's is kept, Klaviyo's consent isn't applied, the clash is reported.
                if !*same {
                    assert_eq!(got.0.as_deref(), Some(dm_email(*n).as_str()), "{case}");
                    let flagged: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM klaviyo_import_issues WHERE import_id = $1 AND kind = 'conflict' AND klaviyo_id = $2 AND message LIKE '%d***@case.test%')")
                        .bind(imp.id)
                        .bind(format!("K{n}"))
                        .fetch_one(&h.state.db)
                        .await
                        .unwrap();
                    assert!(flagged, "{case}: identity conflict recorded with masked values");
                }
                // Every Klaviyo suppression is kept on the profile with its time.
                if !sup.is_empty() && *same {
                    let p: Value = sqlx::query_scalar("SELECT properties FROM profiles WHERE external_id = $1").bind(format!("X{n}")).fetch_one(&h.state.db).await.unwrap();
                    assert_eq!(p["klaviyo_consent"]["email"]["suppressions"][0], json!({"reason": sup, "at": "2024-02-01T00:00:00+00:00"}), "{case}");
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 1400, "{checked}");
    h.cleanup().await;
}

/// (SMS consent, held)
fn expect_sms(state: &str, can: bool, dm: Option<&str>, same: bool) -> (String, bool) {
    let opt_out = state == "UNSUBSCRIBED";
    let opt_in = state == "SUBSCRIBED" && can;
    let odd = !matches!(state, "SUBSCRIBED" | "UNSUBSCRIBED" | "NEVER_SUBSCRIBED" | "" | "-");
    match dm {
        None => ((if opt_out { "unsubscribed" } else if opt_in { "subscribed" } else { "never_subscribed" }).into(), odd),
        Some(d) if !same => (d.into(), false),
        Some(d) => ((if opt_out { "unsubscribed" } else if opt_in && d == "never_subscribed" { "subscribed" } else { d }).into(), odd && d != "subscribed"),
    }
}

#[tokio::test]
async fn sms_consent_every_combination() {
    let mut h = harness!();
    h.signup().await;
    let k_states: [(&str, bool); 6] = [("SUBSCRIBED", true), ("SUBSCRIBED", false), ("UNSUBSCRIBED", false), ("NEVER_SUBSCRIBED", false), ("-", false), ("ODD", true)];
    let dms: [Option<&str>; 5] = [None, Some("never_subscribed"), Some("subscribed"), Some("unsubscribed"), Some("pending")];
    let mut n = 0usize;
    for mode in ["first", "rerun", "sync"] {
        let imp = bare_import(&h, "send", if mode == "sync" { "syncing" } else { "importing" }).await;
        let run = Run::new(&h.state, imp.clone(), "pk_unused");
        let mut cases = Vec::new();
        for (state, can) in k_states {
            for dm in dms {
                for same in [true, false] {
                    if dm.is_none() && !same {
                        continue;
                    }
                    n += 1;
                    cases.push((n, state, can, dm, same));
                }
            }
        }
        let dm_phone = |n: usize| format!("+1503{:07}", n);
        let k_phone = |n: usize, same: bool| if same { dm_phone(n) } else { format!("+1971{:07}", n) };
        let sms = |state: &str, can: bool| if state == "-" { None } else { Some(json!({"consent": state, "can_receive_sms_marketing": can, "method": "TEXT"})) };
        if mode == "first" {
            for (n, _, _, dm, _) in &cases {
                if let Some(d) = dm {
                    sqlx::query("INSERT INTO profiles (id, account_id, phone_number, external_id, sms_consent) VALUES ($1, $2, $3, $4, $5)")
                        .bind(uuid::Uuid::now_v7())
                        .bind(imp.account_id)
                        .bind(dm_phone(*n))
                        .bind(format!("S{n}"))
                        .bind(d)
                        .execute(&h.state.db)
                        .await
                        .unwrap();
                }
            }
        } else {
            let first: Vec<Value> = cases.iter().map(|(n, ..)| kres(&format!("KS{n}"), None, Some(&dm_phone(*n)), &format!("S{n}"), None, sms("NEVER_SUBSCRIBED", false))).collect();
            for page in first.chunks(100) {
                run.upsert_profiles(page).await.unwrap();
            }
            for (n, _, _, dm, _) in &cases {
                match dm {
                    None => sqlx::query("DELETE FROM profiles WHERE external_id = $1").bind(format!("S{n}")).execute(&h.state.db).await.unwrap(),
                    Some(d) => sqlx::query("UPDATE profiles SET sms_consent = $2, marketing_hold = false WHERE external_id = $1").bind(format!("S{n}")).bind(d).execute(&h.state.db).await.unwrap(),
                };
            }
        }
        let pages: Vec<Value> = cases.iter().map(|(n, state, can, _, same)| kres(&format!("KS{n}"), None, Some(&k_phone(*n, *same)), &format!("S{n}"), None, sms(state, *can))).collect();
        for page in pages.chunks(100) {
            run.upsert_profiles(page).await.unwrap();
        }
        for (n, state, can, dm, same) in &cases {
            let got: (Option<String>, String, bool) = sqlx::query_as("SELECT phone_number, sms_consent, marketing_hold FROM profiles WHERE external_id = $1").bind(format!("S{n}")).fetch_one(&h.state.db).await.unwrap();
            let case = format!("mode={mode} klaviyo={state}/can={can} dumbmail={dm:?} same_number={same}");
            assert_eq!((got.1.clone(), got.2), expect_sms(state, *can, *dm, *same), "{case}");
            if got.1 == "subscribed" {
                assert!((*state == "SUBSCRIBED" && *can && *same) || *dm == Some("subscribed"), "{case}: textable without a reachable Klaviyo opt-in or a DumbMail one");
            }
            if !*same {
                assert_eq!(got.0.as_deref(), Some(dm_phone(*n).as_str()), "{case}: DumbMail's number kept");
            }
        }
    }
    h.cleanup().await;
}

/// A STOP or unsubscribe that lands after the importer read the profile and before it writes is kept.
#[tokio::test]
async fn an_opt_out_between_read_and_write_wins() {
    let mut h = harness!();
    h.signup().await;
    let imp = bare_import(&h, "send", "syncing").await;
    let pid = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, phone_number, email_consent, sms_consent) VALUES ($1, $2, 'race@x.test', '+15035550199', 'never_subscribed', 'never_subscribed')")
        .bind(pid)
        .bind(imp.account_id)
        .execute(&h.state.db)
        .await
        .unwrap();
    // Klaviyo says: subscribed to both.
    let k = kres("KR", Some("race@x.test"), Some("+15035550199"), "XR", email_marketing("SUBSCRIBED", true, ""), Some(json!({"consent": "SUBSCRIBED", "can_receive_sms_marketing": true})));
    // The person replies STOP and unsubscribes: committed while the importer is between its read and write.
    let mut stop = h.state.db.begin().await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed', sms_consent = 'unsubscribed' WHERE id = $1").bind(pid).execute(&mut *stop).await.unwrap();
    let state = h.state.clone();
    let task = tokio::spawn(async move {
        let run = Run::new(&state, imp, "pk_unused");
        run.upsert_profiles(&[k]).await.unwrap();
    });
    // Wait until the importer has read the row and is blocked writing it.
    let dbname = h.dbname.clone();
    let mut blocked = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND wait_event_type = 'Lock' AND query LIKE 'UPDATE profiles p SET%'")
            .bind(&dbname)
            .fetch_one(&h.admin)
            .await
            .unwrap();
        if n > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(blocked, "the importer should be waiting on the row");
    stop.commit().await.unwrap();
    task.await.unwrap();
    let got: (String, String) = sqlx::query_as("SELECT email_consent, sms_consent FROM profiles WHERE id = $1").bind(pid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(got, ("unsubscribed".into(), "unsubscribed".into()), "the opt-out that landed meanwhile wins");
    h.cleanup().await;
}

async fn import_with(h: &mut Harness, ds: Value, opts: Value) -> String {
    let key = register(ds).await;
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(h).await;
    let mut o = opts;
    o["confirm"] = json!(true);
    h.ok("POST", "/api/klaviyo/start", Some(o)).await;
    settle(h).await;
    key
}

fn quiet(mut ds: Value) -> Value {
    ds["throttle_once"] = json!(false);
    ds
}

async fn on_list(h: &Harness, list: &str, email: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM list_members m JOIN lists l ON l.id = m.list_id JOIN profiles p ON p.id = m.profile_id WHERE l.name LIKE $1 AND p.email = $2)")
        .bind(list)
        .bind(email)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn people_who_left_a_list_stay_off_it_and_snapshots_are_replaced() {
    let mut h = harness!();
    h.signup().await;
    let key = import_with(&mut h, quiet(dataset()), json!({"what": ["lists", "segments"], "sync": true})).await;
    assert!(on_list(&h, "Newsletter", "bob@example.com").await && on_list(&h, "VIP", "ann@example.com").await);
    assert!(!on_list(&h, "Newsletter", "cat@example.com").await, "opted out of this list in Klaviyo");
    assert!(on_list(&h, "Likely to churn%", "ann@example.com").await);
    // In DumbMail, Bob is taken off the Newsletter list.
    sqlx::query("DELETE FROM list_members m USING lists l, profiles p WHERE l.id = m.list_id AND p.id = m.profile_id AND l.name = 'Newsletter' AND p.email = 'bob@example.com'").execute(&h.state.db).await.unwrap();
    // In Klaviyo: Ann is removed from VIP; Dan opts out of VIP only; the churn segment is now Bob and Dan.
    {
        let mut g = mock().1.lock().unwrap();
        let d = g.sets.get_mut(&key).unwrap();
        d["members"]["L2"] = json!([["P4", "2024-02-02T00:00:00+00:00"]]);
        d["members"]["S2"] = json!([["P2", "2024-02-01T00:00:00+00:00"], ["P4", "2024-02-01T00:00:00+00:00"]]);
        let dan = d["profiles"].as_array_mut().unwrap().iter_mut().find(|p| p["id"] == "P4").unwrap();
        dan["attributes"]["subscriptions"]["email"]["marketing"]["list_suppressions"] = json!([{"list_id": "L2", "reason": "USER_SUPPRESSED", "timestamp": Utc::now().to_rfc3339()}]);
    }
    h.ok("POST", "/api/klaviyo/sync-now", None).await;
    settle(&h).await;
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "done");
    assert!(!on_list(&h, "Newsletter", "bob@example.com").await, "left in DumbMail: the sync doesn't add him back");
    assert!(!on_list(&h, "VIP", "ann@example.com").await, "removed in Klaviyo: the sync takes her off");
    assert!(!on_list(&h, "VIP", "dan@example.com").await, "a list opt-out in Klaviyo: off that list");
    assert!(on_list(&h, "Newsletter", "ann@example.com").await, "other lists untouched");
    let dan: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'dan@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(dan, "subscribed", "a list opt-out stays scoped to the list");
    // A re-run (connect again) retires the old sync and its key, adds nobody back, and replaces the snapshot.
    let old_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    let old: (bool, bool) = sqlx::query_as("SELECT sync_enabled, api_key_enc IS NOT NULL FROM klaviyo_imports WHERE id = $1").bind(old_id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(old, (false, false), "the old connection's sync stops and its key is deleted at once");
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": ["lists", "segments"]}))).await;
    settle(&h).await;
    assert!(!on_list(&h, "Newsletter", "bob@example.com").await);
    assert!(!on_list(&h, "VIP", "ann@example.com").await && !on_list(&h, "VIP", "dan@example.com").await);
    assert!(!on_list(&h, "Likely to churn%", "ann@example.com").await, "no longer in the segment: off the snapshot");
    assert!(on_list(&h, "Likely to churn%", "dan@example.com").await && on_list(&h, "Likely to churn%", "bob@example.com").await);
    // Another Klaviyo account can't be imported into the same DumbMail account.
    let mut other = quiet(dataset());
    other["account_id"] = json!("ACCT2");
    let key2 = register(other).await;
    let (s, v) = h.req("POST", "/api/klaviyo/connect", Some(json!({"api_key": key2}))).await;
    assert_eq!(s, 409, "{v}");
    h.cleanup().await;
}

async fn sync_import(h: &mut Harness) -> uuid::Uuid {
    import_with(h, quiet(dataset()), json!({"what": ["lists"], "sync": true})).await;
    sqlx::query_scalar("SELECT id FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn campaigns_and_flows_wait_for_klaviyo_opt_outs() {
    let mut h = harness!();
    h.signup().await;
    let imp = sync_import(&mut h).await;
    let list: uuid::Uuid = sqlx::query_scalar("SELECT id FROM lists WHERE name = 'Newsletter'").fetch_one(&h.state.db).await.unwrap();
    let make = |name: &str| json!({"name": name, "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}});
    let pool = h.state.db.clone();
    let messages = |id: uuid::Uuid| {
        let db = pool.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM messages WHERE campaign_id = $1").bind(id).fetch_one(&db).await.unwrap() }
    };
    let c1: uuid::Uuid = h.ok("POST", "/api/campaigns", Some(make("Fresh"))).await["id"].as_str().unwrap().parse().unwrap();
    // Fresh consent (the import just finished): it goes.
    sqlx::query("UPDATE klaviyo_imports SET consent_synced_at = now() WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(c1).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, c1).await.unwrap();
    assert!(messages(c1).await > 0);
    // Stale by 20 minutes: it waits, says why, and a sync starts.
    let c2: uuid::Uuid = h.ok("POST", "/api/campaigns", Some(make("Stale"))).await["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE klaviyo_imports SET consent_synced_at = now() - interval '20 minutes' WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(c2).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, c2).await.unwrap();
    assert_eq!(messages(c2).await, 0, "nothing goes out on stale consent");
    let c: (String, Option<String>) = sqlx::query_as("SELECT status, error FROM campaigns WHERE id = $1").bind(c2).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c.0, "sending");
    assert!(c.1.unwrap().starts_with("Waiting for Klaviyo opt-outs to sync"));
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "syncing", "starting it ran a sync");
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["waiting_campaigns"][0]["name"], "Stale");
    // The sync finishes: the campaign goes.
    settle(&h).await;
    h.fast_forward().await;
    assert!(messages(c2).await > 0, "sent once Klaviyo's opt-outs were in");
    let err: Option<String> = sqlx::query_scalar("SELECT error FROM campaigns WHERE id = $1").bind(c2).fetch_one(&h.state.db).await.unwrap();
    assert!(err.is_none());
    // No successful sync for 2 hours: back to draft with the reason, nothing sent.
    let c3: uuid::Uuid = h.ok("POST", "/api/campaigns", Some(make("Never synced"))).await["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE klaviyo_imports SET consent_synced_at = now() - interval '3 hours', status = 'syncing' WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(c3).execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO klaviyo_campaign_waits (campaign_id, account_id, since) SELECT $1, account_id, now() - interval '121 minutes' FROM campaigns WHERE id = $1").bind(c3).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, c3).await.unwrap();
    let c: (String, Option<String>) = sqlx::query_as("SELECT status, error FROM campaigns WHERE id = $1").bind(c3).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c.0, "draft");
    assert!(c.1.unwrap().contains("no sync from Klaviyo succeeded in 2 hours"));
    assert_eq!(messages(c3).await, 0);

    // Flows: consent up to 2 hours old is fine; older, the email waits at its step.
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "Thanks", "trigger": {"type": "metric", "metric": "Placed Order"},
        "definition": {"entry": "w", "nodes": [{"id": "w", "type": "update_profile", "properties": {"seen": true}, "next": "e"}, {"id": "e", "type": "email", "content": {"subject": "Thanks", "blocks": [{"type": "text", "html": "<p>Thanks</p>"}]}, "next": null}]}}))).await;
    let fid: uuid::Uuid = flow["id"].as_str().unwrap().parse().unwrap();
    h.ok("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    let f: dumbmail::models::FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(fid).fetch_one(&h.state.db).await.unwrap();
    let ann: uuid::Uuid = sqlx::query_scalar("SELECT id FROM profiles WHERE email = 'ann@example.com'").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE klaviyo_imports SET status = 'done', consent_synced_at = now() - interval '3 hours' WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM jobs WHERE kind = 'klaviyo_import_run'").execute(&h.state.db).await.unwrap();
    let run = dumbmail::flows::start(&h.state, &f, ann, json!({})).await.unwrap().unwrap();
    dumbmail::flows::step(&h.state, run).await.unwrap();
    let flow_msgs = || {
        let db = pool.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM messages WHERE flow_id = $1").bind(fid).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(flow_msgs().await, 0, "held: consent is 3 hours old");
    let at: (String, Value) = sqlx::query_as("SELECT current_node, context FROM flow_runs WHERE id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(at.0, "e", "waits at the email, not before the profile update");
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "syncing", "the hold started a sync");
    sqlx::query("UPDATE klaviyo_imports SET status = 'done', consent_synced_at = now() - interval '90 minutes' WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    dumbmail::flows::step(&h.state, run).await.unwrap();
    assert_eq!(flow_msgs().await, 1, "consent 90 minutes old is within the flow's limit");
    let visits: Value = sqlx::query_scalar("SELECT visited FROM flow_runs WHERE id = $1").bind(run).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(visits, json!(["w", "e"]), "the wait didn't repeat steps");
    // With the sync off, nothing waits.
    h.ok("POST", "/api/klaviyo/sync", Some(json!({"enabled": false}))).await;
    let c4: uuid::Uuid = h.ok("POST", "/api/campaigns", Some(make("Sync off"))).await["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE campaigns SET status = 'sending' WHERE id = $1").bind(c4).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, c4).await.unwrap();
    assert!(messages(c4).await > 0);
    h.cleanup().await;
}

#[tokio::test]
async fn segment_flows_absorb_people_who_enter_during_history_import() {
    let mut h = harness!();
    h.signup().await;
    let imp = sync_import(&mut h).await;
    let seg = h.ok("POST", "/api/segments", Some(json!({"name": "Ordered", "definition": {"conditions": [{"type": "property", "field": "vip", "op": "eq", "value": true}]}}))).await;
    let sid: uuid::Uuid = seg["id"].as_str().unwrap().parse().unwrap();
    let flow = h.ok("POST", "/api/flows", Some(json!({"name": "VIP welcome", "trigger": {"type": "segment", "segment_id": sid},
        "definition": {"entry": "e", "nodes": [{"id": "e", "type": "email", "content": {"subject": "Hi", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]}, "next": null}]}}))).await;
    h.ok("POST", &format!("/api/flows/{}/status", flow["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await;
    let pool = h.state.db.clone();
    let runs = || {
        let db = pool.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM flow_runs").fetch_one(&db).await.unwrap() }
    };
    let enter = |email: &'static str| {
        let db = pool.clone();
        async move {
            sqlx::query("UPDATE profiles SET properties = properties || '{\"vip\": true}' WHERE email = $1").bind(email).execute(&db).await.unwrap();
        }
    };
    // History is being written: people entering the segment are taken in quietly.
    sqlx::query("UPDATE klaviyo_imports SET history_started_at = now(), history_done_at = NULL WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    enter("bob@example.com").await;
    dumbmail::flows::refresh_segment(&h.state, sid).await.unwrap();
    assert_eq!(runs().await, 0);
    // It finished after this segment's last refresh: the first refresh after it is quiet too.
    sqlx::query("UPDATE klaviyo_imports SET history_done_at = now() + interval '1 second' WHERE id = $1").bind(imp).execute(&h.state.db).await.unwrap();
    enter("gus@example.com").await;
    dumbmail::flows::refresh_segment(&h.state, sid).await.unwrap();
    assert_eq!(runs().await, 0);
    // After that, new members start the flow as usual.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    dumbmail::flows::refresh_segment(&h.state, sid).await.unwrap();
    enter("fay@example.com").await;
    dumbmail::flows::refresh_segment(&h.state, sid).await.unwrap();
    assert_eq!(runs().await, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn imported_items_that_need_review_cant_send_or_go_live() {
    let mut ds = quiet(dataset());
    // The draft text campaign excludes a list that isn't imported and went to half its audience.
    ds["audiences"][1]["attributes"]["definition"]["excluded"] = json!(["LGONE"]);
    ds["campaign_messages"][1]["attributes"]["definition"]["send_options"]["recipient_percentage"] = json!(50);
    // The VIP list isn't on Klaviyo's count endpoint: that report line can't be verified.
    ds["counts"].as_object_mut().unwrap().remove("L2");
    let mut h = harness!();
    h.signup().await;
    import_with(&mut h, ds, json!({})).await;
    let cid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM campaigns WHERE status = 'draft'").fetch_one(&h.state.db).await.unwrap();
    let st = h.ok("GET", "/api/klaviyo", None).await;
    let rev = st["reviews"].as_array().unwrap().iter().find(|r| r["kind"] == "campaign").unwrap().clone();
    assert_eq!(rev["id"], json!(cid));
    let notes = rev["notes"].to_string();
    assert!(notes.contains("LGONE") && notes.contains("50%"), "{notes}");
    // The audience isn't widened: the exclusion that couldn't be mapped keeps it unsendable.
    let (s, v) = h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("needs review"), "{v}");
    // Even if the content mirror were lost, the send path refuses.
    sqlx::query("UPDATE campaigns SET content = content #- '{imported,review}', status = 'sending' WHERE id = $1").bind(cid).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, cid).await.unwrap();
    let c: (String, i64) = sqlx::query_as("SELECT status, (SELECT count(*) FROM messages WHERE campaign_id = $1) FROM campaigns WHERE id = $1").bind(cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, ("draft".into(), 0));
    h.ok("POST", "/api/klaviyo/review", Some(json!({"kind": "campaign", "id": cid}))).await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert!(c["content"]["imported"]["review"].is_null(), "{c}");
    // The imported flow has a split whose rules didn't translate: it can't go live until reviewed.
    let fid: uuid::Uuid = sqlx::query_scalar("SELECT local_id FROM klaviyo_refs WHERE kind = 'flow' AND local_id IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    let (s, v) = h.req("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    assert_eq!(s, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("needs review"), "{v}");
    let tpl: uuid::Uuid = sqlx::query_scalar("SELECT local_id FROM klaviyo_refs WHERE kind = 'template'").fetch_one(&h.state.db).await.unwrap();
    h.ok("POST", "/api/klaviyo/review", Some(json!({"kind": "flow", "id": fid}))).await;
    h.ok("POST", "/api/klaviyo/review", Some(json!({"kind": "template", "id": tpl}))).await;
    let (_, v) = h.req("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    assert!(!v.to_string().contains("needs review"), "{v}");
    // The report: a list Klaviyo wouldn't count is "not verified", so there's no "nothing was lost".
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(line(&r, "Lists", "VIP")["state"], "not_verified");
    assert_eq!(r["all_ok"], false);
    assert!(r["not_verified"].as_u64().unwrap() >= 1);
    let (_, _, csv) = h.raw("GET", "/api/klaviyo/report.csv", &[("cookie", h.cookie.clone().unwrap().as_str())], "").await;
    assert!(csv.contains("VIP,,") && csv.contains("NOT VERIFIED"), "{csv}");
    h.cleanup().await;
}

#[tokio::test]
async fn one_runner_per_import_and_dates_in_the_account_time_zone() {
    let mut h = harness!();
    h.signup().await;
    let key = register(quiet(dataset())).await;
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    let dry = h.ok("POST", "/api/klaviyo/start", Some(json!({"events_from": "2025-01-01"}))).await;
    // Midnight in Los Angeles, not in UTC.
    assert_eq!(dry["options"]["events_since"], "2025-01-01T08:00:00+00:00", "{dry}");
    assert_eq!(dry["options"]["never_subscribed"], "send");
    let (s, _) = h.req("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "never_subscribed": "maybe"}))).await;
    assert_eq!(s, 400);
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "never_subscribed": "hold"}))).await;
    // Another worker holds the import: this run leaves it alone and looks again later.
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("UPDATE klaviyo_imports SET lease_owner = gen_random_uuid(), lease_until = now() + interval '5 minutes' WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM jobs").execute(&h.state.db).await.unwrap();
    dumbmail::features::klaviyo_import::runner::run(&h.state, id).await.unwrap();
    let phase: Option<String> = sqlx::query_scalar("SELECT phase FROM klaviyo_imports WHERE id = $1").bind(id).fetch_one(&h.state.db).await.unwrap();
    assert!(phase.is_none(), "nothing ran");
    assert_eq!(count(&h, "SELECT count(*) FROM jobs WHERE kind = 'klaviyo_import_run'").await, 1);
    sqlx::query("UPDATE klaviyo_imports SET lease_until = now() - interval '1 second' WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
    settle(&h).await;
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "done");
    // Hold-back chosen: Bob (never subscribed in Klaviyo) is held; Ann (subscribed) isn't.
    let held: Vec<(String, bool)> = sqlx::query_as("SELECT email, marketing_hold FROM profiles WHERE email IN ('ann@example.com', 'bob@example.com') ORDER BY email").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(held, vec![("ann@example.com".into(), false), ("bob@example.com".into(), true)]);
    h.cleanup().await;
}

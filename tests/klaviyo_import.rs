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
    /// API key → requests refused for breaking Klaviyo's rules (a missing required parameter).
    refused: HashMap<String, Vec<String>>,
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
    // Klaviyo's rules for profile filters and pages (Get Profiles reference, and the consent guide's
    // "You must specify a list_id to filter on any list suppression properties").
    let breaks = if filter.contains("list_suppressions.") && !filter.contains("equals(subscriptions.email.marketing.list_suppressions.list_id,") {
        Some("list suppression filters need a list_id")
    } else if filter.contains("list_suppressions.") && filter.contains("marketing.suppression.") {
        Some("list and global suppression filters can't be combined")
    } else if path.starts_with("/api/profiles") && p.get("page[size]").and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| n == 0 || n > 100) {
        Some("page[size] must be 1-100")
    } else {
        None
    };
    if let Some(why) = breaks {
        g.refused.entry(key.clone()).or_default().push(format!("{method} {path}?{} ({why})", q.clone().unwrap_or_default()));
        return err(400, "invalid", why);
    }
    let delay = ds["delay_ms"][path.as_str()].as_u64();
    let page_max = ds["page_max"].as_u64().map(|n| n as usize);
    let seg: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
    // Klaviyo's 429: no rate-limit headers, a Retry-After in seconds.
    if path == "/api/profiles" && ds["throttle_once"] == true && !g.sets[&key]["throttled"].as_bool().unwrap_or(false) {
        g.sets.get_mut(&key).unwrap()["throttled"] = json!(true);
        return (StatusCode::TOO_MANY_REQUESTS, [("Retry-After", "1")], axum::Json(json!({"errors": [{"status": 429, "code": "throttled", "detail": "Request was throttled."}]}))).into_response();
    }
    drop(g);
    if let Some(ms) = delay {
        // (The stand-in runs on its own multi-threaded runtime; blocking one of its threads is fine.)
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
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
            page_of(&base, &path, &q, items, page_max.unwrap_or(3))
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
            if ds["gone_lists"].as_array().is_some_and(|g| g.iter().any(|x| x == *id)) {
                return err(404, "not_found", "");
            }
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
            page_of(&base, &path, &q, items, page_max.unwrap_or(3))
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

/// Requests the stand-in refused for breaking Klaviyo's documented rules (should be none).
fn refused(key: &str) -> Vec<String> {
    mock().1.lock().unwrap().refused.get(key).cloned().unwrap_or_default()
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

    // The comparison report: one Klaviyo profile (P9) has no email, phone or id, so the profile total is
    // "not verified" and there's no "nothing was lost"; every other comparison matches, with the numbers.
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    let pretty = serde_json::to_string_pretty(&r["lines"]).unwrap();
    assert_eq!(r["all_ok"], false, "{pretty}");
    assert_eq!(line(&r, "Profiles", "Profiles")["klaviyo"], 11, "Klaviyo's own total from the full scan");
    assert_eq!(line(&r, "Profiles", "Profiles")["dumbmail"], 10);
    assert_eq!(line(&r, "Profiles", "Profiles")["state"], "not_verified");
    assert!(line(&r, "Profiles", "Profiles")["note"].as_str().unwrap().contains("1 profile had no usable email"), "{pretty}");
    for l in r["lines"].as_array().unwrap() {
        if l["label"] == "Profiles" {
            continue;
        }
        if l["diagnostic"] == true {
            assert_eq!(l["state"], "info", "a diagnostic is never a match: {l}");
        } else {
            assert_eq!(l["state"], "match", "{l}");
        }
    }
    assert!(refused(&key).is_empty(), "every request followed Klaviyo's rules: {:?}", refused(&key));
    assert_eq!(line(&r, "Email", "Subscribed in Klaviyo")["klaviyo"], 3);
    assert_eq!(line(&r, "Email", "Subscribed in Klaviyo")["dumbmail"], 2, "one unsubscribed in DumbMail first, and that wins");
    assert_eq!(line(&r, "Email", "Unsubscribed in Klaviyo")["klaviyo"], 2);
    assert_eq!(line(&r, "Email", "Suppressed in Klaviyo (bounced, complained, invalid, by hand, or not emailable)")["dumbmail"], 3);
    assert_eq!(line(&r, "Email", "More reachable here than in Klaviyo (diagnostic)")["dumbmail"], 0);
    assert_eq!(line(&r, "Email", "More reachable here than in Klaviyo (diagnostic)")["klaviyo"], Value::Null, "no Klaviyo number for a diagnostic");
    assert_eq!(line(&r, "SMS", "More textable here than in Klaviyo (diagnostic)")["dumbmail"], 0);
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
    assert_eq!(item("unsubscribes_current")["done"], true, "a run that just finished, after Klaviyo's last send: {}", item("unsubscribes_current"));
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
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(line(&r, "Lists", "Newsletter")["state"], "match", "this run's own scan");
    assert_eq!(line(&r, "Profiles", "Profiles")["klaviyo"], 11);
    h.cleanup().await;
}

/// "Nothing was lost" only when every required line has both numbers and they agree.
#[tokio::test]
async fn nothing_was_lost_only_when_every_line_matches() {
    let mut ds = quiet(dataset());
    // Everyone has an email or a phone number this time.
    ds["profiles"].as_array_mut().unwrap().retain(|p| p["id"] != "P9");
    let mut h = harness!();
    h.signup().await;
    import_with(&mut h, ds, json!({})).await;
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(r["all_ok"], true, "{}", serde_json::to_string_pretty(&r["lines"]).unwrap());
    assert_eq!(line(&r, "Profiles", "Profiles")["state"], "match");
    // Klaviyo's count for orders comes back malformed: that line is "not verified", never zero, and the
    // headline goes.
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap();
    let mpo = dumbmail::features::klaviyo_import::runner::aggregate_months(&json!({"data": {"attributes": {"dates": ["2025-01-01T00:00:00+00:00"], "data": [{"measurements": {"count": []}}]}}}));
    assert!(mpo.is_err(), "a measurement array of the wrong length is an error, not zero");
    sqlx::query("UPDATE klaviyo_imports SET report_cached_at = NULL, source_counts = jsonb_set(source_counts, '{metrics,MPO}', jsonb_build_object('name', 'Placed Order', 'months', '{}'::jsonb, 'error', 'Klaviyo''s count for this metric came back incomplete')) WHERE id = $1")
        .bind(id)
        .execute(&h.state.db)
        .await
        .unwrap();
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(line(&r, "Events", "Placed Order")["state"], "not_verified");
    assert_eq!(line(&r, "Events", "Placed Order")["klaviyo"], Value::Null);
    assert_eq!(r["all_ok"], false);
    // Three days later the import is stale for switching: "unsubscribes up to date" says how old it is.
    sqlx::query("UPDATE klaviyo_imports SET finished_at = now() - interval '3 days 1 hour' WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
    let cl = h.ok("GET", "/api/klaviyo", None).await["checklist"]["items"].clone();
    let item = cl.as_array().unwrap().iter().find(|i| i["key"] == "unsubscribes_current").unwrap().clone();
    assert_eq!(item["done"], false);
    assert!(item["detail"].as_str().unwrap().contains("finished 3 days ago"), "{item}");
    // A Klaviyo send found after the run began: not current either.
    let started: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT started_at FROM klaviyo_imports WHERE id = $1").bind(id).fetch_one(&h.state.db).await.unwrap();
    let sent_during = (started + Duration::minutes(1)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query("UPDATE klaviyo_imports SET finished_at = now(), source_counts = source_counts || jsonb_build_object('last_klaviyo_send', $2::text) WHERE id = $1")
        .bind(id)
        .bind(&sent_during)
        .execute(&h.state.db)
        .await
        .unwrap();
    let cl = h.ok("GET", "/api/klaviyo", None).await["checklist"]["items"].clone();
    let item = cl.as_array().unwrap().iter().find(|i| i["key"] == "unsubscribes_current").unwrap().clone();
    assert_eq!(item["done"], false);
    assert!(item["detail"].as_str().unwrap().contains("while the last import ran"), "{item}");
    // A metric with no answer at all still has a line.
    sqlx::query("UPDATE klaviyo_imports SET report_cached_at = NULL, source_counts = source_counts #- '{metrics,MPO}' WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(line(&r, "Events", "Placed Order")["state"], "not_verified", "{}", serde_json::to_string_pretty(&r["lines"]).unwrap());
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
    for n in ["klaviyo_survey", "klaviyo_start_import", "klaviyo_import_status", "klaviyo_cancel_import", "klaviyo_comparison_report"] {
        assert!(names.iter().any(|x| x == n), "{n}");
    }
    for gone in ["klaviyo_sync_now", "klaviyo_set_sync"] {
        assert!(!names.iter().any(|x| x == gone), "the ongoing sync is gone: {gone}");
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
//
// Every expected outcome below is written out by hand from the rules (docs/MOVING_FROM_KLAVIYO.md,
// "Consent, exactly"), not computed: (consent, suppression reason, held back from marketing).

use dumbmail::features::klaviyo_import::{runner::Run, store};

/// An import row to run profile writes against (no Klaviyo calls are made).
async fn bare_import(h: &Harness, never: &str) -> store::Import {
    let acct: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    let id = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO klaviyo_imports (id, account_id, status, options, klaviyo_account, started_at) VALUES ($1, $2, 'importing', $3, '{\"id\": \"ACCT1\"}', now())")
        .bind(id)
        .bind(acct)
        .bind(json!({"never_subscribed": never}))
        .execute(&h.state.db)
        .await
        .unwrap();
    store::load(&h.state.db, id).await.unwrap().unwrap()
}

/// A run that owns its import, as the job would.
async fn owned_run<'a>(h: &'a Harness, imp: store::Import) -> Run<'a> {
    let run = Run::new(&h.state, imp, "pk_unused");
    assert!(run.claim().await.unwrap());
    run
}

/// The current import of a run is finished; the next run is a re-run of the same Klaviyo account.
async fn finish_bare(h: &Harness, imp: &store::Import) {
    sqlx::query("UPDATE klaviyo_imports SET status = 'done', lease_owner = NULL, finished_at = now() WHERE id = $1").bind(imp.id).execute(&h.state.db).await.unwrap();
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

/// Klaviyo's email marketing record: consent, reachability (None: the flag is left out), suppression reason.
fn em(consent: &str, can: Option<bool>, sup: &str) -> Option<Value> {
    let mut v = json!({"consent": consent, "consent_timestamp": "2023-01-01T00:00:00+00:00", "list_suppressions": [],
        "suppression": if sup.is_empty() { json!([]) } else { json!([{"reason": sup, "timestamp": "2024-02-01T00:00:00+00:00"}]) }});
    if let Some(c) = can {
        v["can_receive_email_marketing"] = json!(c);
    }
    Some(v)
}

fn sm(consent: &str, can: bool) -> Option<Value> {
    Some(json!({"consent": consent, "can_receive_sms_marketing": can, "method": "TEXT"}))
}

async fn email_state(h: &Harness, ext: &str) -> (Option<String>, String, Option<String>, bool) {
    sqlx::query_as("SELECT email, email_consent, email_suppressed_reason, marketing_hold FROM profiles WHERE external_id = $1").bind(ext).fetch_one(&h.state.db).await.unwrap()
}

async fn sms_state(h: &Harness, ext: &str) -> (Option<String>, String, bool) {
    sqlx::query_as("SELECT phone_number, sms_consent, marketing_hold FROM profiles WHERE external_id = $1").bind(ext).fetch_one(&h.state.db).await.unwrap()
}

/// A DumbMail profile made before the import (through DumbMail itself).
async fn dm_profile(h: &Harness, ext: &str, email: Option<&str>, phone: Option<&str>, ec: &str, sc: &str, sup: Option<&str>) {
    let acct: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts ORDER BY created_at DESC LIMIT 1").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO profiles (id, account_id, email, phone_number, external_id, email_consent, sms_consent, email_suppressed_reason) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(uuid::Uuid::now_v7())
        .bind(acct)
        .bind(email)
        .bind(phone)
        .bind(ext)
        .bind(ec)
        .bind(sc)
        .bind(sup)
        .execute(&h.state.db)
        .await
        .unwrap();
}

/// (name, Klaviyo email on the second run, expected (consent, suppression, held))
type RerunEmail = (&'static str, Option<Value>, (&'static str, Option<&'static str>, bool));
/// (name, DumbMail SMS consent before or new, Klaviyo SMS (consent, reachable), expected (consent, held))
type SmsCase = (&'static str, Option<&'static str>, (&'static str, bool), (&'static str, bool));
type EmailCase = (&'static str, Option<(&'static str, Option<&'static str>)>, (&'static str, Option<bool>, &'static str), bool, (&'static str, Option<&'static str>, bool));

#[tokio::test]
async fn email_consent_case_by_case() {
    let mut h = harness!();
    h.signup().await;
    // (name, DumbMail before (consent, suppression) or new, Klaviyo (consent, reachable, suppression; "-" = no record),
    //  hold back never-subscribed, expected (consent, suppression, held))
    let cases: &[EmailCase] = &[
        ("new: subscribed and reachable", None, ("SUBSCRIBED", Some(true), ""), false, ("subscribed", None, false)),
        ("new: subscribed but Klaviyo can't email them", None, ("SUBSCRIBED", Some(false), ""), false, ("subscribed", Some("klaviyo_suppressed"), false)),
        ("new: reachability flag left out", None, ("SUBSCRIBED", None, ""), false, ("subscribed", Some("klaviyo_suppressed"), false)),
        ("new: unsubscribed", None, ("UNSUBSCRIBED", Some(false), ""), false, ("unsubscribed", None, false)),
        ("new: never subscribed, kept emailable", None, ("NEVER_SUBSCRIBED", Some(true), ""), false, ("never_subscribed", None, false)),
        ("new: never subscribed, held back by choice", None, ("NEVER_SUBSCRIBED", Some(true), ""), true, ("never_subscribed", None, true)),
        ("new: never subscribed, not emailable", None, ("NEVER_SUBSCRIBED", Some(false), ""), false, ("never_subscribed", Some("klaviyo_suppressed"), false)),
        ("new: no consent record", None, ("-", None, ""), false, ("never_subscribed", None, true)),
        ("new: unrecognised consent", None, ("SOMETHING_NEW", Some(true), ""), false, ("never_subscribed", None, true)),
        ("new: hard bounce", None, ("SUBSCRIBED", Some(false), "HARD_BOUNCE"), false, ("subscribed", Some("hard_bounce"), false)),
        ("new: spam complaint", None, ("SUBSCRIBED", Some(false), "SPAM_COMPLAINT"), false, ("unsubscribed", Some("complaint"), false)),
        ("new: invalid email", None, ("NEVER_SUBSCRIBED", Some(false), "INVALID_EMAIL"), false, ("never_subscribed", Some("invalid_email"), false)),
        ("new: suppressed by hand", None, ("SUBSCRIBED", Some(false), "USER_SUPPRESSED"), false, ("subscribed", Some("manual"), false)),
        ("new: unsubscribe suppression", None, ("SUBSCRIBED", Some(true), "UNSUBSCRIBE"), false, ("unsubscribed", None, false)),
        ("new: unrecognised suppression, flag says reachable", None, ("SUBSCRIBED", Some(true), "SOME_NEW_REASON"), false, ("subscribed", Some("klaviyo_suppressed"), false)),
        ("here never subscribed, Klaviyo subscribed", Some(("never_subscribed", None)), ("SUBSCRIBED", Some(true), ""), false, ("subscribed", None, false)),
        ("here never subscribed, held back by choice", Some(("never_subscribed", None)), ("NEVER_SUBSCRIBED", Some(true), ""), true, ("never_subscribed", None, true)),
        ("own DumbMail opt-in, Klaviyo never subscribed", Some(("subscribed", None)), ("NEVER_SUBSCRIBED", Some(true), ""), true, ("subscribed", None, false)),
        ("own DumbMail opt-in, Klaviyo unrecognised", Some(("subscribed", None)), ("SOMETHING_NEW", Some(true), ""), false, ("subscribed", None, false)),
        ("own DumbMail opt-in, no Klaviyo record", Some(("subscribed", None)), ("-", None, ""), false, ("subscribed", None, false)),
        ("own DumbMail opt-in, Klaviyo unsubscribed", Some(("subscribed", None)), ("UNSUBSCRIBED", Some(false), ""), false, ("unsubscribed", None, false)),
        ("unsubscribed here, Klaviyo subscribed", Some(("unsubscribed", None)), ("SUBSCRIBED", Some(true), ""), false, ("unsubscribed", None, false)),
        ("pending here, Klaviyo subscribed", Some(("pending", None)), ("SUBSCRIBED", Some(true), ""), false, ("pending", None, false)),
        ("pending here, Klaviyo unsubscribed", Some(("pending", None)), ("UNSUBSCRIBED", Some(false), ""), false, ("unsubscribed", None, false)),
        ("bounced here, Klaviyo subscribed", Some(("subscribed", Some("hard_bounce"))), ("SUBSCRIBED", Some(true), ""), false, ("subscribed", Some("hard_bounce"), false)),
        ("suppressed here, never subscribed, Klaviyo subscribed", Some(("never_subscribed", Some("manual"))), ("SUBSCRIBED", Some(true), ""), false, ("never_subscribed", Some("manual"), false)),
        ("here never subscribed, Klaviyo bounced", Some(("never_subscribed", None)), ("SUBSCRIBED", Some(false), "HARD_BOUNCE"), false, ("subscribed", Some("hard_bounce"), false)),
    ];
    for hold in [false, true] {
        let imp = bare_import(&h, if hold { "hold" } else { "send" }).await;
        let run = owned_run(&h, imp.clone()).await;
        let mine: Vec<(usize, &EmailCase)> = cases.iter().enumerate().filter(|(_, c)| c.3 == hold).collect();
        for (i, (_, dm, _, _, _)) in &mine {
            if let Some((c, sup)) = dm {
                dm_profile(&h, &format!("E{i}"), Some(&format!("e{i}@case.test")), None, c, "never_subscribed", *sup).await;
            }
        }
        let page: Vec<Value> = mine
            .iter()
            .map(|(i, (_, _, (k, can, sup), _, _))| kres(&format!("KE{i}"), Some(&format!("e{i}@case.test")), None, &format!("E{i}"), if *k == "-" { None } else { em(k, *can, sup) }, None))
            .collect();
        run.upsert_profiles(&page).await.unwrap();
        for (i, (name, _, _, _, (ec, sup, held))) in &mine {
            let got = email_state(&h, &format!("E{i}")).await;
            assert_eq!((got.1.as_str(), got.2.as_deref(), got.3), (*ec, *sup, *held), "{name}");
        }
        finish_bare(&h, &imp).await;
    }
    h.cleanup().await;
}

/// A subscription an earlier import set is Klaviyo's opt-in: on a later run, what Klaviyo says now governs it.
#[tokio::test]
async fn a_subscription_the_import_set_follows_klaviyo_on_the_next_run() {
    let mut h = harness!();
    h.signup().await;
    // (name, Klaviyo email on the second run, expected (consent, suppression, held))
    let email_cases: &[RerunEmail] = &[
        ("now unsubscribed", em("UNSUBSCRIBED", Some(false), ""), ("unsubscribed", None, false)),
        ("now unrecognised", em("SOMETHING_NEW", Some(true), ""), ("never_subscribed", None, true)),
        ("now no record", None, ("never_subscribed", None, true)),
        ("now subscribed but can't receive", em("SUBSCRIBED", Some(false), ""), ("subscribed", Some("klaviyo_suppressed"), false)),
        ("now never subscribed", em("NEVER_SUBSCRIBED", Some(true), ""), ("never_subscribed", None, false)),
        ("still subscribed", em("SUBSCRIBED", Some(true), ""), ("subscribed", None, false)),
    ];
    let sms_cases: &[(&str, Option<Value>, (&str, bool))] = &[
        ("now unsubscribed", sm("UNSUBSCRIBED", false), ("unsubscribed", false)),
        ("now subscribed but unreachable", sm("SUBSCRIBED", false), ("never_subscribed", false)),
        ("now unrecognised", sm("SOMETHING_NEW", true), ("never_subscribed", true)),
        ("now blank", sm("", true), ("never_subscribed", true)),
        ("now never subscribed", sm("NEVER_SUBSCRIBED", false), ("never_subscribed", false)),
        ("still subscribed", sm("SUBSCRIBED", true), ("subscribed", false)),
    ];
    // First run: Klaviyo says subscribed and reachable on both channels; the import subscribes them.
    let first = bare_import(&h, "send").await;
    let run = owned_run(&h, first.clone()).await;
    let mut page: Vec<Value> = (0..email_cases.len()).map(|i| kres(&format!("KR{i}"), Some(&format!("r{i}@case.test")), None, &format!("R{i}"), em("SUBSCRIBED", Some(true), ""), None)).collect();
    page.extend((0..sms_cases.len()).map(|i| kres(&format!("KS{i}"), None, Some(&format!("+1503555{i:04}")), &format!("S{i}"), None, sm("SUBSCRIBED", true))));
    run.upsert_profiles(&page).await.unwrap();
    for i in 0..email_cases.len() {
        assert_eq!(email_state(&h, &format!("R{i}")).await.1, "subscribed");
    }
    for i in 0..sms_cases.len() {
        assert_eq!(sms_state(&h, &format!("S{i}")).await.1, "subscribed");
    }
    // The same page again in the same run (a replay) adds no second history entry.
    run.upsert_profiles(&page[..1]).await.unwrap();
    finish_bare(&h, &first).await;
    // Second run: Klaviyo says something else.
    let second = bare_import(&h, "send").await;
    let run = owned_run(&h, second.clone()).await;
    let mut page: Vec<Value> = email_cases.iter().enumerate().map(|(i, (_, e, _))| kres(&format!("KR{i}"), Some(&format!("r{i}@case.test")), None, &format!("R{i}"), e.clone(), None)).collect();
    page.extend(sms_cases.iter().enumerate().map(|(i, (_, s, _))| kres(&format!("KS{i}"), None, Some(&format!("+1503555{i:04}")), &format!("S{i}"), None, s.clone())));
    run.upsert_profiles(&page).await.unwrap();
    for (i, (name, _, (ec, sup, held))) in email_cases.iter().enumerate() {
        let got = email_state(&h, &format!("R{i}")).await;
        assert_eq!((got.1.as_str(), got.2.as_deref(), got.3), (*ec, *sup, *held), "email {name}");
    }
    for (i, (name, _, (sc, held))) in sms_cases.iter().enumerate() {
        let got = sms_state(&h, &format!("S{i}")).await;
        assert_eq!((got.1.as_str(), got.2), (*sc, *held), "sms {name}");
    }
    // What Klaviyo said is kept per run, appended: two entries, one per run, newest last.
    let hist: Value = sqlx::query_scalar("SELECT properties->'klaviyo_consent_history' FROM profiles WHERE external_id = 'R0'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(hist.as_array().unwrap().len(), 2, "{hist}");
    assert_eq!(hist[0]["run"], json!(first.id));
    assert_eq!(hist[0]["email"]["status"], "SUBSCRIBED");
    assert_eq!(hist[1]["run"], json!(second.id));
    assert_eq!(hist[1]["email"]["status"], "UNSUBSCRIBED");
    finish_bare(&h, &second).await;
    // ...and bounded: after six more runs only the last five remain.
    for _ in 0..6 {
        let imp = bare_import(&h, "send").await;
        let run = owned_run(&h, imp.clone()).await;
        run.upsert_profiles(&page[..1]).await.unwrap();
        finish_bare(&h, &imp).await;
    }
    let n: i64 = sqlx::query_scalar("SELECT jsonb_array_length(properties->'klaviyo_consent_history')::bigint FROM profiles WHERE external_id = 'R0'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 5);
    h.cleanup().await;
}

#[tokio::test]
async fn sms_consent_case_by_case() {
    let mut h = harness!();
    h.signup().await;
    // (name, DumbMail SMS consent before or new, Klaviyo SMS (consent, reachable; "-" = no record), expected (consent, held))
    let cases: &[SmsCase] = &[
        ("new: subscribed and reachable", None, ("SUBSCRIBED", true), ("subscribed", false)),
        ("new: subscribed but unreachable", None, ("SUBSCRIBED", false), ("never_subscribed", false)),
        ("new: unsubscribed", None, ("UNSUBSCRIBED", false), ("unsubscribed", false)),
        ("new: never subscribed", None, ("NEVER_SUBSCRIBED", false), ("never_subscribed", false)),
        ("new: no record", None, ("-", false), ("never_subscribed", false)),
        ("new: unrecognised", None, ("SOMETHING_NEW", true), ("never_subscribed", true)),
        ("new: present but blank", None, ("", true), ("never_subscribed", true)),
        ("own DumbMail opt-in, Klaviyo never subscribed", Some("subscribed"), ("NEVER_SUBSCRIBED", false), ("subscribed", false)),
        ("own DumbMail opt-in, Klaviyo unreachable", Some("subscribed"), ("SUBSCRIBED", false), ("subscribed", false)),
        ("own DumbMail opt-in, Klaviyo unsubscribed", Some("subscribed"), ("UNSUBSCRIBED", false), ("unsubscribed", false)),
        ("STOP here, Klaviyo subscribed", Some("unsubscribed"), ("SUBSCRIBED", true), ("unsubscribed", false)),
        ("pending here, Klaviyo subscribed", Some("pending"), ("SUBSCRIBED", true), ("pending", false)),
        ("never here, Klaviyo subscribed", Some("never_subscribed"), ("SUBSCRIBED", true), ("subscribed", false)),
    ];
    let imp = bare_import(&h, "send").await;
    let run = owned_run(&h, imp).await;
    for (i, (_, dm, _, _)) in cases.iter().enumerate() {
        if let Some(c) = dm {
            dm_profile(&h, &format!("T{i}"), None, Some(&format!("+1971555{i:04}")), "never_subscribed", c, None).await;
        }
    }
    let page: Vec<Value> =
        cases.iter().enumerate().map(|(i, (_, _, (k, can), _))| kres(&format!("KT{i}"), None, Some(&format!("+1971555{i:04}")), &format!("T{i}"), None, if *k == "-" { None } else { sm(k, *can) })).collect();
    run.upsert_profiles(&page).await.unwrap();
    for (i, (name, _, _, (sc, held))) in cases.iter().enumerate() {
        let got = sms_state(&h, &format!("T{i}")).await;
        assert_eq!((got.1.as_str(), got.2), (*sc, *held), "{name}");
    }
    h.cleanup().await;
}

/// Klaviyo's profile matched DumbMail profile A (same external id) but carries B's email and number.
/// A keeps its own address and consent; Klaviyo's opt-outs for B's address and number apply to B; the
/// clash is reported with both values partly hidden, in the message and the label.
#[tokio::test]
async fn an_opt_out_follows_the_address_to_whichever_profile_has_it() {
    let mut h = harness!();
    h.signup().await;
    dm_profile(&h, "XA", Some("anna@case.test"), Some("+15035550111"), "never_subscribed", "never_subscribed", None).await;
    dm_profile(&h, "XB", Some("bert@case.test"), Some("+15035550122"), "subscribed", "subscribed", None).await;
    let imp = bare_import(&h, "send").await;
    let run = owned_run(&h, imp.clone()).await;
    let k = kres("KX", Some("bert@case.test"), Some("+15035550122"), "XA", em("UNSUBSCRIBED", Some(false), ""), sm("UNSUBSCRIBED", false));
    run.upsert_profiles(&[k]).await.unwrap();
    let a = email_state(&h, "XA").await;
    assert_eq!((a.0.as_deref(), a.1.as_str(), a.2.as_deref(), a.3), (Some("anna@case.test"), "never_subscribed", None, false), "A keeps its own address and consent");
    assert_eq!(sms_state(&h, "XA").await, (Some("+15035550111".to_string()), "never_subscribed".to_string(), false));
    let b = email_state(&h, "XB").await;
    assert_eq!((b.1.as_str(), b.2.as_deref()), ("unsubscribed", None), "Klaviyo's email opt-out applies to B, who has the address");
    assert_eq!(sms_state(&h, "XB").await.1, "unsubscribed", "and its SMS opt-out to B's number");
    let (item, msg): (String, String) = sqlx::query_as("SELECT item, message FROM klaviyo_import_issues WHERE import_id = $1 AND kind = 'conflict'").bind(imp.id).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(item, "b***@case.test", "the label is masked");
    assert!(msg.contains("a***@case.test") && msg.contains("b***@case.test") && msg.contains("+1•••••0111") && msg.contains("+1•••••0122"), "{msg}");
    assert!(!msg.contains("anna@") && !msg.contains("bert@") && !msg.contains("5550111"), "no whole address or number: {msg}");
    // A suppression follows the address the same way.
    dm_profile(&h, "XC", Some("cleo@case.test"), None, "subscribed", "never_subscribed", None).await;
    let k = kres("KY", Some("cleo@case.test"), None, "XA", em("SUBSCRIBED", Some(false), "HARD_BOUNCE"), None);
    run.upsert_profiles(&[k]).await.unwrap();
    let c = email_state(&h, "XC").await;
    assert_eq!((c.1.as_str(), c.2.as_deref()), ("subscribed", Some("hard_bounce")));
    h.cleanup().await;
}

/// A STOP or unsubscribe that lands after the importer read the profile and before it writes is kept.
#[tokio::test]
async fn an_opt_out_between_read_and_write_wins() {
    let mut h = harness!();
    h.signup().await;
    let imp = bare_import(&h, "send").await;
    let pid = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, account_id, email, phone_number, email_consent, sms_consent) VALUES ($1, $2, 'race@x.test', '+15035550199', 'never_subscribed', 'never_subscribed')")
        .bind(pid)
        .bind(imp.account_id)
        .execute(&h.state.db)
        .await
        .unwrap();
    // Klaviyo says: subscribed to both.
    let k = kres("KR", Some("race@x.test"), Some("+15035550199"), "XR", em("SUBSCRIBED", Some(true), ""), sm("SUBSCRIBED", true));
    // The person replies STOP and unsubscribes: committed while the importer is between its read and write.
    let mut stop = h.state.db.begin().await.unwrap();
    sqlx::query("UPDATE profiles SET email_consent = 'unsubscribed', sms_consent = 'unsubscribed' WHERE id = $1").bind(pid).execute(&mut *stop).await.unwrap();
    let state = h.state.clone();
    let task = tokio::spawn(async move {
        let run = Run::new(&state, imp, "pk_unused");
        assert!(run.claim().await.unwrap());
        run.upsert_profiles(&[k]).await.unwrap();
    });
    // Wait until the importer is blocked taking the row's lock, before it reads it to merge.
    let dbname = h.dbname.clone();
    let mut blocked = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND wait_event_type = 'Lock' AND query LIKE 'SELECT p.id FROM profiles p%FOR UPDATE'")
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
    let key = import_with(&mut h, quiet(dataset()), json!({"what": ["lists", "segments"]})).await;
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
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": ["lists", "segments"]}))).await;
    settle(&h).await;
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "done");
    assert!(!on_list(&h, "Newsletter", "bob@example.com").await, "left in DumbMail: a re-run doesn't add him back");
    assert!(!on_list(&h, "VIP", "ann@example.com").await, "removed in Klaviyo: a re-run takes her off");
    assert!(!on_list(&h, "VIP", "dan@example.com").await, "a list opt-out in Klaviyo: off that list");
    assert!(on_list(&h, "Newsletter", "ann@example.com").await, "other lists untouched");
    let dan: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'dan@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(dan, "subscribed", "a list opt-out stays scoped to the list");
    // Another re-run adds nobody back and replaces the snapshot.
    assert_eq!(count(&h, "SELECT count(*) FROM klaviyo_imports WHERE api_key_enc IS NOT NULL").await, 0, "no key is kept after a run");
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
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

async fn list_id(h: &Harness, name: &str) -> uuid::Uuid {
    sqlx::query_scalar("SELECT id FROM lists WHERE name = $1").bind(name).fetch_one(&h.state.db).await.unwrap()
}

async fn pid(h: &Harness, email: &str) -> uuid::Uuid {
    sqlx::query_scalar("SELECT id FROM profiles WHERE email = $1").bind(email).fetch_one(&h.state.db).await.unwrap()
}

async fn rerun(h: &mut Harness, key: &str, what: Value) {
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": what}))).await;
    settle(h).await;
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "done");
}

fn edit(key: &str, f: impl FnOnce(&mut Value)) {
    let mut g = mock().1.lock().unwrap();
    f(g.sets.get_mut(key).unwrap());
}

fn set_list_suppressions(d: &mut Value, profile: &str, lists: &[&str]) {
    let p = d["profiles"].as_array_mut().unwrap().iter_mut().find(|p| p["id"] == profile).unwrap();
    p["attributes"]["subscriptions"]["email"]["marketing"]["list_suppressions"] =
        json!(lists.iter().map(|l| json!({"list_id": l, "reason": "USER_SUPPRESSED", "timestamp": "2025-06-01T00:00:00+00:00"})).collect::<Vec<_>>());
    p["attributes"]["updated"] = json!(Utc::now().to_rfc3339());
}

#[tokio::test]
async fn list_leaves_rejoins_and_opt_outs_follow_their_rules() {
    let mut h = harness!();
    h.signup().await;
    let key = import_with(&mut h, quiet(dataset()), json!({"what": ["lists"]})).await;
    let (news, vip) = (list_id(&h, "Newsletter").await, list_id(&h, "VIP").await);
    assert!(on_list(&h, "Newsletter", "ann@example.com").await && on_list(&h, "VIP", "ann@example.com").await && on_list(&h, "Newsletter", "bob@example.com").await);

    // Bob leaves the Newsletter from the dashboard: the leave is recorded with the removal.
    let bob = pid(&h, "bob@example.com").await;
    h.ok("DELETE", &format!("/api/lists/{news}/members"), Some(json!({"profile_ids": [bob]}))).await;
    let left: (String, String) = sqlx::query_as("SELECT state, left_via FROM klaviyo_memberships WHERE list_id = $1 AND profile_id = $2").bind(news).bind(bob).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(left, ("left".into(), "dumbmail".into()));
    // In Klaviyo: Ann opts out of the Newsletter only (still subscribed to email); Dan is taken off VIP.
    edit(&key, |d| {
        set_list_suppressions(d, "P1", &["L1"]);
        d["members"]["L1"] = json!([["P2", "2024-01-02T00:00:00+00:00"], ["P3", "2024-01-03T00:00:00+00:00"]]);
        d["members"]["L2"] = json!([["P1", "2024-02-01T00:00:00+00:00"]]);
    });
    rerun(&mut h, &key, json!(["lists"])).await;
    assert!(!on_list(&h, "Newsletter", "bob@example.com").await, "left in DumbMail: not added back");
    assert!(!on_list(&h, "Newsletter", "ann@example.com").await, "a list opt-out takes her off that list");
    assert!(on_list(&h, "VIP", "ann@example.com").await, "...and only that list");
    let ann: String = sqlx::query_scalar("SELECT email_consent FROM profiles WHERE email = 'ann@example.com'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ann, "subscribed", "a list opt-out on top of a global subscription leaves the subscription");
    assert!(!on_list(&h, "VIP", "dan@example.com").await, "taken off in Klaviyo: off here");

    // Dan is put back on VIP directly in DumbMail. A list opt-out for another list doesn't touch it.
    let dan = pid(&h, "dan@example.com").await;
    h.ok("POST", &format!("/api/lists/{vip}/members"), Some(json!({"profile_ids": [dan]}))).await;
    // Klaviyo: Dan opts out of the Newsletter; Ann's Newsletter opt-out is gone and she's back on it there;
    // Bob is on the Newsletter again in Klaviyo too.
    edit(&key, |d| {
        set_list_suppressions(d, "P4", &["L1"]);
        set_list_suppressions(d, "P1", &[]);
        d["members"]["L1"] = json!([["P1", "2025-07-01T00:00:00+00:00"], ["P2", "2024-01-02T00:00:00+00:00"], ["P3", "2024-01-03T00:00:00+00:00"]]);
    });
    rerun(&mut h, &key, json!(["lists"])).await;
    assert!(on_list(&h, "VIP", "dan@example.com").await, "an opt-out from the Newsletter leaves his VIP membership alone");
    assert!(on_list(&h, "Newsletter", "ann@example.com").await, "a leave Klaviyo recorded is lifted once Klaviyo has her back with no opt-out");
    assert!(!on_list(&h, "Newsletter", "bob@example.com").await, "a leave recorded in DumbMail is never lifted by an import");
    assert!(refused(&key).is_empty(), "{:?}", refused(&key));

    // A list Klaviyo answers 404 for is "not reconciled": nobody is taken off it on that account.
    edit(&key, |d| d["gone_lists"] = json!(["L2"]));
    rerun(&mut h, &key, json!(["lists"])).await;
    assert!(on_list(&h, "VIP", "ann@example.com").await && on_list(&h, "VIP", "dan@example.com").await);
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    let vip_line = line(&r, "Lists", "VIP");
    assert_eq!(vip_line["state"], "not_verified");
    assert!(vip_line["note"].as_str().unwrap().contains("Not reconciled"), "{vip_line}");
    assert_eq!(r["all_ok"], false);
    h.cleanup().await;
}

/// A person leaving a list in DumbMail while the import is adding them stays off it.
#[tokio::test]
async fn a_leave_while_the_import_adds_the_same_person_wins() {
    let mut h = harness!();
    h.signup().await;
    import_with(&mut h, quiet(dataset()), json!({"what": ["lists"]})).await;
    let news = list_id(&h, "Newsletter").await;
    let acct: uuid::Uuid = sqlx::query_scalar("SELECT id FROM accounts").fetch_one(&h.state.db).await.unwrap();
    let gus = pid(&h, "gus@example.com").await;
    // The import's statement for Gus, mid-transaction: his membership row recorded and the list row added,
    // not yet committed (exactly what the import's page write does, holding the same row lock).
    let mut import = h.state.db.begin().await.unwrap();
    sqlx::query(
        "WITH j AS (INSERT INTO klaviyo_memberships (account_id, list_id, profile_id, state, scan) VALUES ($1, $2, $3, 'member', 1)
                    ON CONFLICT (list_id, profile_id) DO UPDATE SET state = 'member', scan = 1 WHERE klaviyo_memberships.state = 'member' RETURNING profile_id)
         INSERT INTO list_members (list_id, profile_id) SELECT $2, profile_id FROM j ON CONFLICT DO NOTHING",
    )
    .bind(acct)
    .bind(news)
    .bind(gus)
    .execute(&mut *import)
    .await
    .unwrap();
    // Gus leaves through the dashboard at that moment.
    let state = h.state.clone();
    let leave = tokio::spawn(async move { dumbmail::features::klaviyo_import::remove_members(&state.db, acct, news, &[gus]).await.unwrap() });
    let dbname = h.dbname.clone();
    let mut waiting = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND wait_event_type = 'Lock' AND query LIKE 'INSERT INTO klaviyo_memberships%'")
            .bind(&dbname)
            .fetch_one(&h.admin)
            .await
            .unwrap();
        if n > 0 {
            waiting = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(waiting, "the leave waits for the import's row");
    import.commit().await.unwrap();
    assert_eq!(leave.await.unwrap().len(), 1, "the removal saw the import's row");
    assert!(!on_list(&h, "Newsletter", "gus@example.com").await);
    // The other order: the leave first, then the import's statement adds nobody.
    let fay = pid(&h, "fay@example.com").await;
    dumbmail::features::klaviyo_import::remove_members(&h.state.db, acct, news, &[fay]).await.unwrap();
    let added = sqlx::query(
        "WITH j AS (INSERT INTO klaviyo_memberships (account_id, list_id, profile_id, state, scan) VALUES ($1, $2, $3, 'member', 1)
                    ON CONFLICT (list_id, profile_id) DO UPDATE SET state = 'member', scan = 1 WHERE klaviyo_memberships.state = 'member' OR klaviyo_memberships.left_via IN ('klaviyo', 'klaviyo_list_unsubscribe') RETURNING profile_id)
         INSERT INTO list_members (list_id, profile_id) SELECT $2, profile_id FROM j ON CONFLICT DO NOTHING",
    )
    .bind(acct)
    .bind(news)
    .bind(fay)
    .execute(&h.state.db)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(added, 0);
    h.cleanup().await;
}

/// A list of a few thousand that shrinks: the departed come off in SQL, in batches.
#[tokio::test]
async fn a_big_list_that_shrinks_loses_exactly_the_departed() {
    let mut ds = quiet(dataset());
    let many: Vec<Value> = (0..3000).map(|i| prof(&format!("B{i}"), Some(&format!("b{i}@bulk.test")), "SUBSCRIBED", true, &[], json!({}))).collect();
    ds["profiles"] = json!(many);
    ds["members"] = json!({"L1": (0..3000).map(|i| json!([format!("B{i}"), "2024-01-01T00:00:00+00:00"])).collect::<Vec<_>>()});
    ds["counts"] = json!({"L1": 3000, "L2": 0});
    ds["page_max"] = json!(100);
    let mut h = harness!();
    h.signup().await;
    let key = import_with(&mut h, ds, json!({"what": ["lists"]})).await;
    assert_eq!(count(&h, "SELECT count(*) FROM list_members m JOIN lists l ON l.id = m.list_id WHERE l.name = 'Newsletter'").await, 3000);
    edit(&key, |d| {
        d["members"]["L1"] = json!((0..1000).map(|i| json!([format!("B{i}"), "2024-01-01T00:00:00+00:00"])).collect::<Vec<_>>());
        d["counts"]["L1"] = json!(1000);
    });
    rerun(&mut h, &key, json!(["lists"])).await;
    assert_eq!(count(&h, "SELECT count(*) FROM list_members m JOIN lists l ON l.id = m.list_id WHERE l.name = 'Newsletter'").await, 1000);
    assert_eq!(count(&h, "SELECT count(*) FROM list_members m JOIN lists l ON l.id = m.list_id JOIN profiles p ON p.id = m.profile_id WHERE l.name = 'Newsletter' AND p.email LIKE 'b1___@bulk.test'").await, 0, "B1000-B1999 are gone");
    assert_eq!(count(&h, "SELECT count(*) FROM klaviyo_memberships WHERE state = 'left' AND left_via = 'klaviyo'").await, 2000);
    let r = h.ok("GET", "/api/klaviyo/report", None).await;
    assert_eq!(line(&r, "Lists", "Newsletter")["state"], "match");
    h.cleanup().await;
}

/// A page whose response arrives after a cancel writes nothing.
#[tokio::test]
async fn cancelling_while_a_page_is_in_flight_writes_nothing() {
    let mut ds = quiet(dataset());
    ds["delay_ms"] = json!({"/api/profiles": 1500});
    let key = register(ds).await;
    let mut h = harness!();
    h.signup().await;
    // The survey reads profiles too; let it finish with no delay first.
    edit(&key, |d| d["delay_ms"] = json!({}));
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": []}))).await;
    edit(&key, |d| d["delay_ms"] = json!({"/api/profiles": 1500}));
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap();
    sqlx::query("DELETE FROM jobs WHERE kind = 'klaviyo_import_run'").execute(&h.state.db).await.unwrap();
    let before = calls(&key).iter().filter(|c| c.starts_with("GET /api/profiles?")).count();
    let state = h.state.clone();
    let run = tokio::spawn(async move { dumbmail::features::klaviyo_import::runner::run(&state, id).await });
    for _ in 0..200 {
        if calls(&key).iter().filter(|c| c.starts_with("GET /api/profiles?")).count() > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(calls(&key).iter().filter(|c| c.starts_with("GET /api/profiles?")).count() > before, "the profile page was requested");
    h.ok("POST", "/api/klaviyo/cancel", None).await;
    run.await.unwrap().unwrap();
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 0, "the page that came back after the cancel wrote nothing");
    assert_eq!(count(&h, "SELECT count(*) FROM klaviyo_refs WHERE kind = 'profile'").await, 0);
    assert_eq!(h.ok("GET", "/api/klaviyo", None).await["import"]["status"], "cancelled");
    h.cleanup().await;
}

/// A crash after a page is written but before its progress is saved: the replay neither duplicates nor
/// counts twice.
#[tokio::test]
async fn a_crash_between_writing_a_page_and_saving_it_is_replayed_cleanly() {
    let mut h = harness!();
    h.signup().await;
    let key = register(quiet(dataset())).await;
    h.ok("POST", "/api/klaviyo/connect", Some(json!({"api_key": key}))).await;
    settle(&h).await;
    h.ok("POST", "/api/klaviyo/start", Some(json!({"confirm": true, "what": []}))).await;
    sqlx::query("DELETE FROM jobs WHERE kind = 'klaviyo_import_run'").execute(&h.state.db).await.unwrap();
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM klaviyo_imports").fetch_one(&h.state.db).await.unwrap();
    // By hand: claim the run, do the metrics step and save, then write the first page of profiles and
    // "crash" before saving.
    let imp = store::load(&h.state.db, id).await.unwrap().unwrap();
    let mut run = Run::new(&h.state, imp, &key);
    assert!(run.claim().await.unwrap());
    assert!(run.step_once().await.unwrap(), "metrics in one page");
    drop(run);
    sqlx::query("UPDATE klaviyo_imports SET phase = 'profiles', lease_owner = NULL, lease_until = NULL WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
    let imp = store::load(&h.state.db, id).await.unwrap().unwrap();
    let mut run = Run::new(&h.state, imp, &key);
    assert!(run.claim().await.unwrap());
    assert!(!run.step_once().await.unwrap(), "first page of profiles written");
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 3, "the page's three profiles are in");
    drop(run); // the crash: nothing saved
    // The worker's lease runs out; the import is picked up again and finishes.
    sqlx::query("UPDATE klaviyo_imports SET lease_until = now() - interval '1 second' WHERE id = $1").bind(id).execute(&h.state.db).await.unwrap();
    store::enqueue(&h.state.db, store::JOB_RUN, id, None).await.unwrap();
    settle(&h).await;
    let st = h.ok("GET", "/api/klaviyo", None).await;
    assert_eq!(st["import"]["status"], "done");
    assert_eq!(st["import"]["progress"]["profiles"]["done"], 10, "each profile counted once: {}", st["import"]["progress"]);
    assert_eq!(st["import"]["progress"]["profiles"]["skipped"], 1);
    assert_eq!(count(&h, "SELECT count(*) FROM profiles").await, 10, "nobody twice");
    h.cleanup().await;
}

#[tokio::test]
async fn segment_flows_absorb_people_who_enter_during_history_import() {
    let mut h = harness!();
    h.signup().await;
    import_with(&mut h, quiet(dataset()), json!({"what": ["lists"]})).await;
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
    let absorbing = || {
        let db = pool.clone();
        async move { sqlx::query_scalar::<_, bool>("SELECT absorb_entries FROM segments WHERE id = $1").bind(sid).fetch_one(&db).await.unwrap() }
    };
    // A run writing a page of history opens the window (as `step_events` does, in the page's transaction).
    let imp = bare_import(&h, "send").await;
    let run = owned_run(&h, imp.clone()).await;
    let mut tx = run.begin().await.unwrap();
    dumbmail::features::klaviyo_import::history::open(&mut tx, imp.id, imp.account_id).await.unwrap();
    tx.commit().await.unwrap();
    assert!(absorbing().await);
    enter("bob@example.com").await;
    dumbmail::flows::refresh_segment(&h.state, sid).await.unwrap();
    assert_eq!(runs().await, 0, "entered while history was written: taken in quietly");
    // The run is paused: the window closes at once and the segment is re-baselined (quietly) right away.
    enter("gus@example.com").await;
    h.ok("POST", "/api/klaviyo/pause", None).await;
    let open: bool = sqlx::query_scalar("SELECT history_open FROM klaviyo_imports WHERE id = $1").bind(imp.id).fetch_one(&h.state.db).await.unwrap();
    assert!(!open);
    h.fast_forward().await;
    assert!(!absorbing().await, "re-baselined and back to normal");
    assert_eq!(runs().await, 0, "Gus (moved in by history) is part of the new baseline");
    // After that, new members start the flow as usual.
    enter("fay@example.com").await;
    dumbmail::flows::refresh_segment(&h.state, sid).await.unwrap();
    assert_eq!(runs().await, 1);
    // Cancelling also closes it.
    sqlx::query("UPDATE klaviyo_imports SET status = 'importing', lease_owner = NULL, lease_until = NULL WHERE id = $1").bind(imp.id).execute(&h.state.db).await.unwrap();
    let run = owned_run(&h, store::load(&h.state.db, imp.id).await.unwrap().unwrap()).await;
    let mut tx = run.begin().await.unwrap();
    dumbmail::features::klaviyo_import::history::open(&mut tx, imp.id, imp.account_id).await.unwrap();
    tx.commit().await.unwrap();
    assert!(absorbing().await);
    h.ok("POST", "/api/klaviyo/cancel", None).await;
    h.fast_forward().await;
    assert!(!absorbing().await);
    h.cleanup().await;
}

async fn mcp_call(h: &Harness, token: &str, name: &str, args: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let (s, _, b) = h.raw("POST", "/mcp", &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json")], &body.to_string()).await;
    assert_eq!(s, 200, "{b}");
    serde_json::from_str::<Value>(&b).unwrap()["result"].clone()
}

#[tokio::test]
async fn imported_items_that_need_review_cant_send_or_go_live() {
    let mut ds = quiet(dataset());
    // The draft text campaign excludes a list that isn't imported and went to half its audience.
    ds["audiences"][1]["attributes"]["definition"]["excluded"] = json!(["LGONE"]);
    ds["campaign_messages"][1]["attributes"]["definition"]["send_options"]["recipient_percentage"] = json!(50);
    // The VIP list isn't on Klaviyo's count endpoint: that report line can't be verified.
    ds["counts"].as_object_mut().unwrap().remove("L2");
    // The form signs people up to two lists, depending on the version; a back-in-stock button carries a third.
    ds["forms"][0]["attributes"]["definition"]["versions"] = json!([
        {"status": "live", "steps": [{"components": [{"type": "button", "action": {"type": "next_step", "submit": true, "properties": {"list_id": "L1"}}},
                                                    {"type": "button", "action": {"type": "submit_back_in_stock", "submit": true, "properties": {"list_id": "L9"}}}]}]},
        {"status": "live", "steps": [{"components": [{"type": "button", "action": {"type": "redirect", "submit": true, "properties": {"list_id": "L2", "url": "x"}}}]}]}]);
    let mut h = harness!();
    h.signup().await;
    sqlx::query("UPDATE accounts SET address = '1 Main St', from_email = 'hi@acme.test', sms_from = '+15035550000'").execute(&h.state.db).await.unwrap();
    import_with(&mut h, ds, json!({})).await;
    let cid: uuid::Uuid = sqlx::query_scalar("SELECT id FROM campaigns WHERE status = 'draft'").fetch_one(&h.state.db).await.unwrap();
    let st = h.ok("GET", "/api/klaviyo", None).await;
    let rev = st["reviews"].as_array().unwrap().iter().find(|r| r["kind"] == "campaign").unwrap().clone();
    assert_eq!(rev["id"], json!(cid));
    let notes = rev["notes"].to_string();
    assert!(notes.contains("LGONE") && notes.contains("50%"), "{notes}");
    let refused_with_review = |(s, v): (axum::http::StatusCode, Value)| {
        assert_eq!(s, 400, "{v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("needs review"), "{v}");
    };
    // Send, schedule and test send are all refused.
    refused_with_review(h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await);
    refused_with_review(h.req("POST", &format!("/api/campaigns/{cid}/schedule"), Some(json!({"send_at": (Utc::now() + Duration::days(1)).to_rfc3339()}))).await);
    refused_with_review(h.req("POST", &format!("/api/campaigns/{cid}/test"), Some(json!({"to": ["+15035550101"]}))).await);
    // An edit that drops the marker from the content doesn't clear the review: it's put back.
    let c = h.ok("PATCH", &format!("/api/campaigns/{cid}"), Some(json!({"content": {"body": "New text"}}))).await;
    assert!(c["content"]["imported"]["review"].as_array().is_some_and(|a| !a.is_empty()), "{c}");
    refused_with_review(h.req("POST", &format!("/api/campaigns/{cid}/send"), None).await);
    // A duplicate needs review too.
    let dup = h.ok("POST", &format!("/api/campaigns/{cid}/duplicate"), None).await;
    let dup_id = dup["id"].as_str().unwrap().to_string();
    refused_with_review(h.req("POST", &format!("/api/campaigns/{dup_id}/send"), None).await);
    // Even with the column and the marker gone, a campaign already sending stops at the send job... (column only)
    sqlx::query("UPDATE campaigns SET content = content #- '{imported,review}', status = 'sending' WHERE id = $1").bind(cid).execute(&h.state.db).await.unwrap();
    dumbmail::campaigns::prepare_send(&h.state, cid).await.unwrap();
    let c: (String, i64) = sqlx::query_as("SELECT status, (SELECT count(*) FROM messages WHERE campaign_id = $1) FROM campaigns WHERE id = $1").bind(cid).fetch_one(&h.state.db).await.unwrap();
    assert_eq!(c, ("failed".into(), 0), "the review column alone stops it, nothing is sent");
    sqlx::query("UPDATE campaigns SET status = 'draft', error = NULL WHERE id = $1").bind(cid).execute(&h.state.db).await.unwrap();

    // Imported template content travels: a new campaign (or an A/B version) made from it needs review.
    let tpl: uuid::Uuid = sqlx::query_scalar("SELECT local_id FROM klaviyo_refs WHERE kind = 'template'").fetch_one(&h.state.db).await.unwrap();
    let t = h.ok("GET", &format!("/api/templates/{tpl}"), None).await;
    assert!(t["content"]["imported"]["review"].is_array(), "{t}");
    let list: uuid::Uuid = sqlx::query_scalar("SELECT id FROM lists WHERE name = 'Newsletter'").fetch_one(&h.state.db).await.unwrap();
    let from_tpl = h.ok("POST", "/api/campaigns", Some(json!({"name": "From template", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": t["content"]}))).await;
    refused_with_review(h.req("POST", &format!("/api/campaigns/{}/send", from_tpl["id"].as_str().unwrap()), None).await);
    let plain = json!({"subject": "Plain", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]});
    let ab = h.ok("POST", "/api/campaigns", Some(json!({"name": "A/B", "channel": "email", "audience": {"include": [{"type": "list", "id": list}]}, "content": plain,
        "ab_test": {"variants": [{"id": "b", "content": t["content"]}], "test_percent": 50, "wait_hours": 4, "metric": "open_rate", "pick": "auto"}}))).await;
    refused_with_review(h.req("POST", &format!("/api/campaigns/{}/send", ab["id"].as_str().unwrap()), None).await);
    // Editing the template can't drop its marker either.
    let t2 = h.ok("PATCH", &format!("/api/templates/{tpl}"), Some(json!({"content": {"subject": "x", "blocks": [{"type": "text", "html": "<p>x</p>"}]}}))).await;
    assert!(t2["content"]["imported"]["review"].is_array(), "{t2}");

    // Flows: the imported one can't go live; a copy can't either; a live flow can't take in imported content.
    let fid: uuid::Uuid = sqlx::query_scalar("SELECT local_id FROM klaviyo_refs WHERE kind = 'flow' AND local_id IS NOT NULL").fetch_one(&h.state.db).await.unwrap();
    refused_with_review(h.req("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await);
    let f = h.ok("GET", &format!("/api/flows/{fid}"), None).await;
    let copy = h.ok("POST", "/api/flows", Some(json!({"name": "Copy", "trigger": f["trigger"], "definition": f["definition"]}))).await;
    refused_with_review(h.req("POST", &format!("/api/flows/{}/status", copy["id"].as_str().unwrap()), Some(json!({"status": "live"}))).await);
    let live = h.ok("POST", "/api/flows", Some(json!({"name": "Live", "trigger": {"type": "metric", "metric": "Placed Order"},
        "definition": {"entry": "e", "nodes": [{"id": "e", "type": "email", "content": plain, "next": null}]}}))).await;
    let live_id = live["id"].as_str().unwrap().to_string();
    h.ok("POST", &format!("/api/flows/{live_id}/status"), Some(json!({"status": "live"}))).await;
    refused_with_review(h.req("PATCH", &format!("/api/flows/{live_id}"), Some(json!({"definition": {"entry": "e", "nodes": [{"id": "e", "type": "email", "content": t["content"], "next": null}]}}))).await);

    // Forms: the one with two possible lists can't be published until a list is chosen.
    let form: (uuid::Uuid, Option<uuid::Uuid>) = sqlx::query_as("SELECT id, list_id FROM forms").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(form.1, None, "two sign-up lists (the back-in-stock one doesn't count): none chosen");
    let ids: Value = sqlx::query_scalar("SELECT detail->'list_ids' FROM klaviyo_refs WHERE kind = 'form'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(ids, json!(["L1", "L2"]));
    let (s, v) = h.req("PATCH", &format!("/api/forms/{}", form.0), Some(json!({"status": "live"}))).await;
    assert_eq!(s, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("Choose the list"), "{v}");
    h.ok("PATCH", &format!("/api/forms/{}", form.0), Some(json!({"status": "live", "list_id": list}))).await;

    // Clearing a review over MCP takes confirm: without it, a dry run; with it, cleared and audited.
    let token = h.ok("POST", "/api/api-keys", Some(json!({"name": "mcp"}))).await["key"].as_str().unwrap().to_string();
    let r = mcp_call(&h, &token, "klaviyo_clear_review", json!({"kind": "flow", "id": fid})).await;
    assert_eq!(r["structuredContent"]["dry_run"], true, "{r}");
    refused_with_review(h.req("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await);
    let r = mcp_call(&h, &token, "klaviyo_clear_review", json!({"kind": "flow", "id": fid, "confirm": true})).await;
    assert_eq!(r["isError"], false, "{r}");
    let (_, v) = h.req("POST", &format!("/api/flows/{fid}/status"), Some(json!({"status": "live"}))).await;
    assert!(!v.to_string().contains("needs review"), "{v}");
    h.ok("POST", "/api/klaviyo/review", Some(json!({"kind": "campaign", "id": cid}))).await;
    let c = h.ok("GET", &format!("/api/campaigns/{cid}"), None).await;
    assert!(c["content"]["imported"]["review"].is_null() && c["import_review"].is_null(), "{c}");
    assert_eq!(count(&h, "SELECT count(*) FROM audit_log WHERE action = 'klaviyo.review_clear'").await, 2);
    assert!(count(&h, "SELECT count(*) FROM audit_log WHERE action IN ('klaviyo.connect', 'klaviyo.start')").await >= 2);
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

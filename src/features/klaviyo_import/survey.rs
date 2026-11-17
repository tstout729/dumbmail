//! Look at a Klaviyo account before importing anything: what's there, roughly how long it will take,
//! and which read scopes the key is missing. Klaviyo has no count endpoints, so counts come from paging
//! ids (bounded; past the bound the count is a floor) and event volume from metric aggregates.

use chrono::Utc;
use serde_json::{json, Value};

use super::client::{next_link, q, Client, KlaviyoError};
use super::runner::DEFAULT_METRICS;

/// Count a collection by paging, up to `max_pages`. Returns (count, exact).
async fn count(k: &mut Client, first: &str, max_pages: usize) -> Result<(u64, bool), KlaviyoError> {
    let mut url = first.to_string();
    let mut n = 0u64;
    for _ in 0..max_pages {
        let page = k.get(&url).await?;
        n += page["data"].as_array().map(|a| a.len()).unwrap_or(0) as u64;
        match next_link(&page) {
            Some(nx) => url = nx,
            None => return Ok((n, true)),
        }
    }
    Ok((n, false))
}

/// What has been counted so far. Saved after a section finishes, so a late 429 resumes the survey where
/// it stopped instead of starting over.
struct Tally<'a> {
    out: &'a mut serde_json::Map<String, Value>,
}

impl Tally<'_> {
    fn done(&self, key: &str) -> bool {
        self.out.contains_key(key)
    }

    fn missing(&mut self, scope: &str) {
        let m = self.out.entry("missing_scopes").or_insert(json!([]));
        if let Some(a) = m.as_array_mut() {
            if !a.iter().any(|x| x == scope) {
                a.push(json!(scope));
            }
        }
    }

    fn put(&mut self, key: &str, scope: &str, r: Result<(u64, bool), KlaviyoError>) -> Result<(), KlaviyoError> {
        match r {
            Ok((n, exact)) => {
                self.out.insert(key.into(), json!({"count": n, "exact": exact}));
                Ok(())
            }
            Err(KlaviyoError::MissingScope(_)) => {
                self.out.insert(key.into(), json!({"count": null, "exact": false, "error": "no access"}));
                self.missing(scope);
                Ok(())
            }
            Err(KlaviyoError::Status { status: 404, .. }) => {
                self.out.insert(key.into(), json!({"count": 0, "exact": true}));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn n(&self, key: &str) -> u64 {
        self.out.get(key).and_then(|v| v["count"].as_u64()).unwrap_or(0)
    }

    fn exact(&self, key: &str) -> bool {
        self.out.get(key).is_none_or(|v| v["exact"] != false || v["count"].is_null())
    }
}

/// Count everything. `done` holds the sections already counted (and receives each as it finishes).
pub async fn survey(k: &mut Client, events_since: Option<chrono::DateTime<Utc>>, tz: &str, done: &mut serde_json::Map<String, Value>) -> Result<Value, KlaviyoError> {
    let mut t = Tally { out: done };
    let sections: [(&str, &str, &str, usize); 9] = [
        ("profiles", "profiles:read", "/api/profiles?page%5Bsize%5D=100&fields%5Bprofile%5D=email", 100),
        ("lists", "lists:read", "/api/lists?page%5Bsize%5D=10&fields%5Blist%5D=name", 100),
        ("segments", "segments:read", "/api/segments?page%5Bsize%5D=10&fields%5Bsegment%5D=name", 100),
        ("templates", "templates:read", "/api/templates?page%5Bsize%5D=10&fields%5Btemplate%5D=name", 100),
        ("campaigns", "campaigns:read", "/api/campaigns?page%5Bsize%5D=100", 20),
        ("flows", "flows:read", "/api/flows?page%5Bsize%5D=50&fields%5Bflow%5D=name", 20),
        ("forms", "forms:read", "/api/forms?page%5Bsize%5D=100&fields%5Bform%5D=name", 10),
        ("catalog_items", "catalogs:read", "/api/catalog-items?page%5Bsize%5D=100&fields%5Bcatalog-item%5D=title", 20),
        ("coupons", "coupons:read", "/api/coupons?fields%5Bcoupon%5D=external_id", 20),
    ];
    for (key, scope, url, pages) in sections {
        if !t.done(key) {
            let r = count(k, url, pages).await;
            t.put(key, scope, r)?;
        }
    }
    // Suppressions, by reason.
    if !t.done("suppressions") {
        let mut sup = serde_json::Map::new();
        let mut sup_total = 0u64;
        let mut sup_exact = true;
        for reason in ["HARD_BOUNCE", "SPAM_COMPLAINT", "USER_SUPPRESSED", "INVALID_EMAIL", "UNSUBSCRIBE"] {
            let f = q(&format!("equals(subscriptions.email.marketing.suppression.reason,'{reason}')"));
            match count(k, &format!("/api/profiles?page%5Bsize%5D=100&fields%5Bprofile%5D=email&filter={f}"), 20).await {
                Ok((n, exact)) => {
                    sup.insert(reason.to_lowercase(), json!({"count": n, "exact": exact}));
                    sup_total += n;
                    sup_exact &= exact;
                }
                Err(KlaviyoError::MissingScope(_)) => sup_exact = false,
                // Not every account can filter on every reason; leave it out.
                Err(KlaviyoError::Status { status, .. }) if status < 500 => sup_exact = false,
                Err(e) => return Err(e),
            }
        }
        t.out.insert("suppressions".into(), json!({"count": sup_total, "exact": sup_exact, "by_reason": sup}));
    }
    // Metrics, and an estimate of the events in the chosen range for the ones imported by default.
    if !t.done("all_metrics") {
        let mut metrics = Vec::new();
        let mut url = "/api/metrics".to_string();
        for _ in 0..10 {
            match k.get(&url).await {
                Ok(page) => {
                    for m in page["data"].as_array().into_iter().flatten() {
                        if let (Some(id), Some(name)) = (m["id"].as_str(), m["attributes"]["name"].as_str()) {
                            metrics.push(json!({"id": id, "name": name, "integration": m["attributes"]["integration"]["name"].as_str().unwrap_or("")}));
                        }
                    }
                    match next_link(&page) {
                        Some(n) => url = n,
                        None => break,
                    }
                }
                Err(KlaviyoError::MissingScope(_)) => {
                    t.missing("metrics:read");
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        t.out.insert("metrics".into(), json!({"count": metrics.len(), "exact": true}));
        t.out.insert("all_metrics".into(), json!(metrics));
    }
    let now = Utc::now();
    if !t.done("events") {
        let since = events_since.unwrap_or(now - chrono::Duration::days(365 * 5));
        // One year sampled, scaled to the chosen range.
        let sample_start = (now - chrono::Duration::days(365)).max(since);
        let scale = ((now - since).num_days() as f64 / (now - sample_start).num_days().max(1) as f64).max(1.0);
        let mut per_metric = Vec::new();
        let mut events_total = 0f64;
        let metrics: Vec<(String, String, String)> = t.out["all_metrics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some((m["id"].as_str()?.to_string(), m["name"].as_str()?.to_string(), m["integration"].as_str().unwrap_or("").to_string())))
            .collect();
        for (id, name, integration) in metrics.iter().filter(|(_, n, _)| DEFAULT_METRICS.iter().any(|d| d.eq_ignore_ascii_case(n))).take(25) {
            let body = json!({"data": {"type": "metric-aggregate", "attributes": {"metric_id": id, "measurements": ["count"], "interval": "month", "timezone": tz,
                "filter": [format!("greater-or-equal(datetime,{})", sample_start.format("%Y-%m-%dT%H:%M:%S")), format!("less-than(datetime,{})", now.format("%Y-%m-%dT%H:%M:%S"))]}}});
            match k.post("/api/metric-aggregates", &body).await {
                Ok(v) => {
                    let n: f64 = v["data"]["attributes"]["data"].as_array().into_iter().flatten().flat_map(|row| row["measurements"]["count"].as_array().cloned().unwrap_or_default()).filter_map(|x| x.as_f64()).sum();
                    let est = (n * scale).round();
                    events_total += est;
                    per_metric.push(json!({"id": id, "name": name, "integration": integration, "estimate": est}));
                }
                Err(KlaviyoError::MissingScope(_)) | Err(KlaviyoError::Status { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        t.out.insert("events".into(), json!({"estimate": events_total, "since": events_since.map(|s| s.format("%Y-%m-%d").to_string()), "by_metric": per_metric}));
    }
    // Time: profiles ~4,000/min (plus as many again for list members), events ~20,000/min, each list
    // and segment size check 4 seconds, each flow and campaign about a second. Capped counts make the
    // estimate a lower bound, and it says so.
    let profiles = t.n("profiles") as f64;
    let events_total = t.out["events"]["estimate"].as_f64().unwrap_or(0.0);
    let minutes = profiles / 4000.0 * 2.0
        + events_total / 20000.0
        + (t.n("lists") + t.n("segments")) as f64 * 8.0 / 60.0
        + (t.n("campaigns") + t.n("flows") + t.n("forms")) as f64 * 1.5 / 60.0
        + t.n("templates") as f64 * 0.1 / 60.0;
    let lower_bound = ["profiles", "lists", "segments", "templates", "campaigns", "flows", "forms"].iter().any(|k| !t.exact(k));
    t.out.insert("estimate_minutes".into(), json!(minutes.ceil().max(1.0)));
    t.out.insert("estimate_is_lower_bound".into(), json!(lower_bound));
    if !t.out.contains_key("missing_scopes") {
        t.out.insert("missing_scopes".into(), json!([]));
    }
    t.out.insert("surveyed_at".into(), json!(now));
    Ok(Value::Object(t.out.clone()))
}

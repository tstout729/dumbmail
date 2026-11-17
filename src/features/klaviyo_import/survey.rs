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

struct Tally {
    out: serde_json::Map<String, Value>,
    missing: Vec<String>,
}

impl Tally {
    fn put(&mut self, key: &str, scope: &str, r: Result<(u64, bool), KlaviyoError>) -> Result<(), KlaviyoError> {
        match r {
            Ok((n, exact)) => {
                self.out.insert(key.into(), json!({"count": n, "exact": exact}));
                Ok(())
            }
            Err(KlaviyoError::MissingScope(_)) => {
                self.out.insert(key.into(), json!({"count": null, "exact": false, "error": "no access"}));
                if !self.missing.iter().any(|m| m == scope) {
                    self.missing.push(scope.into());
                }
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
}

pub async fn survey(k: &mut Client, events_since: Option<chrono::DateTime<Utc>>, tz: &str) -> Result<Value, KlaviyoError> {
    let mut t = Tally { out: Default::default(), missing: Vec::new() };
    let r = count(k, "/api/profiles?page%5Bsize%5D=100&fields%5Bprofile%5D=email", 100).await;
    t.put("profiles", "profiles:read", r)?;
    let r = count(k, "/api/lists?page%5Bsize%5D=10&fields%5Blist%5D=name", 100).await;
    t.put("lists", "lists:read", r)?;
    let r = count(k, "/api/segments?page%5Bsize%5D=10&fields%5Bsegment%5D=name", 100).await;
    t.put("segments", "segments:read", r)?;
    let r = count(k, "/api/templates?page%5Bsize%5D=10&fields%5Btemplate%5D=name", 100).await;
    t.put("templates", "templates:read", r)?;
    let r = count(k, "/api/campaigns?page%5Bsize%5D=100", 20).await;
    t.put("campaigns", "campaigns:read", r)?;
    let r = count(k, "/api/flows?page%5Bsize%5D=50&fields%5Bflow%5D=name", 20).await;
    t.put("flows", "flows:read", r)?;
    let r = count(k, "/api/forms?page%5Bsize%5D=100&fields%5Bform%5D=name", 10).await;
    t.put("forms", "forms:read", r)?;
    let r = count(k, "/api/catalog-items?page%5Bsize%5D=100&fields%5Bcatalog-item%5D=title", 20).await;
    t.put("catalog_items", "catalogs:read", r)?;
    let r = count(k, "/api/coupons?fields%5Bcoupon%5D=external_id", 20).await;
    t.put("coupons", "coupons:read", r)?;
    // Suppressions, by reason.
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
    // Metrics, and an estimate of the events in the chosen range for the ones imported by default.
    let mut metrics = Vec::new();
    let mut url = "/api/metrics".to_string();
    for _ in 0..10 {
        match k.get(&url).await {
            Ok(page) => {
                for m in page["data"].as_array().into_iter().flatten() {
                    if let (Some(id), Some(name)) = (m["id"].as_str(), m["attributes"]["name"].as_str()) {
                        metrics.push((id.to_string(), name.to_string(), m["attributes"]["integration"]["name"].as_str().unwrap_or("").to_string()));
                    }
                }
                match next_link(&page) {
                    Some(n) => url = n,
                    None => break,
                }
            }
            Err(KlaviyoError::MissingScope(_)) => {
                t.missing.push("metrics:read".into());
                break;
            }
            Err(e) => return Err(e),
        }
    }
    t.out.insert("metrics".into(), json!({"count": metrics.len(), "exact": true}));
    let now = Utc::now();
    let since = events_since.unwrap_or(now - chrono::Duration::days(365 * 5));
    // One year sampled, scaled to the chosen range.
    let sample_start = (now - chrono::Duration::days(365)).max(since);
    let scale = ((now - since).num_days() as f64 / (now - sample_start).num_days().max(1) as f64).max(1.0);
    let mut per_metric = Vec::new();
    let mut events_total = 0f64;
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
    t.out.insert("all_metrics".into(), json!(metrics.iter().map(|(id, n, i)| json!({"id": id, "name": n, "integration": i})).collect::<Vec<_>>()));
    // Time: profiles ~4,000/min (plus as many again for list members), events ~20,000/min, each list
    // and segment size check 4 seconds, each flow and campaign about a second.
    let profiles = t.n("profiles") as f64;
    let minutes = profiles / 4000.0 * 2.0
        + events_total / 20000.0
        + (t.n("lists") + t.n("segments")) as f64 * 8.0 / 60.0
        + (t.n("campaigns") + t.n("flows") + t.n("forms")) as f64 * 1.5 / 60.0
        + t.n("templates") as f64 * 0.1 / 60.0;
    t.out.insert("estimate_minutes".into(), json!(minutes.ceil().max(1.0)));
    t.out.insert("missing_scopes".into(), json!(t.missing));
    t.out.insert("surveyed_at".into(), json!(now));
    Ok(Value::Object(t.out))
}

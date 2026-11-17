//! Deliverability by inbox provider and sending domain, a simple health score with plain-language
//! flags, and benchmarks against published industry figures and the account's own history.

use std::collections::HashMap;

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use super::query::{self, Arg, Sql, Window};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const PROVIDERS: &[(&str, &str)] = &[("gmail", "Gmail"), ("outlook", "Outlook and Hotmail"), ("yahoo", "Yahoo and AOL"), ("apple", "Apple iCloud"), ("other", "Other")];

const GMAIL: &[&str] = &["gmail.com", "googlemail.com"];
const OUTLOOK: &[&str] = &[
    "outlook.com", "hotmail.com", "live.com", "msn.com", "passport.com", "windowslive.com", "hotmail.co.uk", "hotmail.fr", "hotmail.de", "hotmail.it", "hotmail.es",
    "hotmail.ca", "outlook.fr", "outlook.de", "outlook.es", "outlook.it", "live.co.uk", "live.ca", "live.com.au", "live.fr", "live.de", "live.nl",
];
const YAHOO: &[&str] = &[
    "yahoo.com", "yahoo.co.uk", "yahoo.ca", "yahoo.fr", "yahoo.de", "yahoo.es", "yahoo.it", "yahoo.com.au", "yahoo.co.in", "yahoo.com.br", "yahoo.co.jp",
    "ymail.com", "rocketmail.com", "aol.com", "aim.com", "verizon.net", "att.net", "sbcglobal.net", "bellsouth.net",
];
const APPLE: &[&str] = &["icloud.com", "me.com", "mac.com"];

/// The provider for a recipient domain from the built-in list (None: look at MX records).
pub fn known_provider(domain: &str) -> Option<&'static str> {
    let d = domain.to_ascii_lowercase();
    [("gmail", GMAIL), ("outlook", OUTLOOK), ("yahoo", YAHOO), ("apple", APPLE)].iter().find(|(_, list)| list.contains(&d.as_str())).map(|(p, _)| *p)
}

/// The provider behind a domain's MX hosts (Google Workspace, Microsoft 365, Yahoo, iCloud).
pub fn provider_from_mx(records: &[String]) -> &'static str {
    for r in records {
        let host = r.split_whitespace().last().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
        if host.ends_with(".google.com") || host.ends_with(".googlemail.com") || host == "smtp.google.com" {
            return "gmail";
        }
        if host.ends_with(".outlook.com") || host.ends_with(".hotmail.com") {
            return "outlook";
        }
        if host.ends_with(".yahoodns.net") || host.ends_with(".aol.com") {
            return "yahoo";
        }
        if host.ends_with(".icloud.com") || host.ends_with(".me.com") {
            return "apple";
        }
    }
    "other"
}

/// SQL for a message's provider: the built-in list, then the MX cache (`ipc`), else other.
fn provider_sql(q: &mut Sql) -> String {
    let lists: Vec<String> = [GMAIL, OUTLOOK, YAHOO, APPLE].iter().map(|l| q.bind(Arg::TextArr(l.iter().map(|s| s.to_string()).collect()))).collect();
    format!(
        "CASE WHEN dom = ANY({}) THEN 'gmail' WHEN dom = ANY({}) THEN 'outlook' WHEN dom = ANY({}) THEN 'yahoo' WHEN dom = ANY({}) THEN 'apple' ELSE COALESCE(ipc.provider, 'other') END",
        lists[0], lists[1], lists[2], lists[3]
    )
}

const SENT: &str = "m.status NOT IN ('queued','sending','failed','skipped')";

fn aggregates() -> String {
    format!(
        "count(*) FILTER (WHERE {SENT})::float8 AS sent,
         count(*) FILTER (WHERE m.status IN ('sent','delivered') OR m.opened_at IS NOT NULL)::float8 AS delivered,
         count(m.opened_at)::float8 AS opens, count(m.clicked_at)::float8 AS clicks, count(m.unsubscribed_at)::float8 AS unsubscribes,
         count(*) FILTER (WHERE m.status = 'bounced')::float8 AS bounces,
         count(*) FILTER (WHERE m.status = 'bounced' AND m.bounce_type = 'Permanent')::float8 AS hard_bounces,
         count(*) FILTER (WHERE m.status = 'complained')::float8 AS complaints"
    )
}

const RAW: &[&str] = &["sent", "delivered", "opens", "clicks", "unsubscribes", "bounces", "hard_bounces", "complaints"];

fn read_raw(r: &sqlx::postgres::PgRow) -> HashMap<&'static str, f64> {
    RAW.iter().map(|k| (*k, r.try_get::<Option<f64>, _>(*k).ok().flatten().unwrap_or(0.0))).collect()
}

fn rates(raw: &HashMap<&'static str, f64>) -> Value {
    let g = |k: &str| raw.get(k).copied().unwrap_or(0.0);
    let r = |n: f64, d: f64| if d > 0.0 { ((n / d) * 1e6).round() / 1e6 } else { 0.0 };
    json!({
        "sent": g("sent") as i64, "delivered": g("delivered") as i64, "opens": g("opens") as i64, "clicks": g("clicks") as i64,
        "unsubscribes": g("unsubscribes") as i64, "bounces": g("bounces") as i64, "hard_bounces": g("hard_bounces") as i64,
        "soft_bounces": (g("bounces") - g("hard_bounces")) as i64, "complaints": g("complaints") as i64,
        "delivery_rate": r(g("delivered"), g("sent")), "open_rate": r(g("opens"), g("delivered")), "click_rate": r(g("clicks"), g("delivered")),
        "bounce_rate": r(g("bounces"), g("sent")), "hard_bounce_rate": r(g("hard_bounces"), g("sent")), "soft_bounce_rate": r(g("bounces") - g("hard_bounces"), g("sent")),
        "complaint_rate": r(g("complaints"), g("sent")), "unsubscribe_rate": r(g("unsubscribes"), g("sent")),
    })
}

/// Fewer emails than this and rates swing too much to flag.
const MIN_VOLUME: f64 = 100.0;

fn pct(x: f64) -> String {
    let p = x * 100.0;
    if p < 1.0 { format!("{p:.2}%") } else { format!("{p:.1}%") }
}

/// A 0-100 score and plain-language flags for one provider, domain or the whole account.
pub fn health(name: &str, raw: &HashMap<&'static str, f64>) -> Value {
    let g = |k: &str| raw.get(k).copied().unwrap_or(0.0);
    let sent = g("sent");
    if sent < MIN_VOLUME {
        return json!({"score": null, "grade": "not_enough_data", "flags": [], "note": format!("Fewer than {MIN_VOLUME} emails sent; not enough to judge yet.")});
    }
    let (complaint, bounce, unsub) = (g("complaints") / sent, g("bounces") / sent, g("unsubscribes") / sent);
    let open = if g("delivered") > 0.0 { g("opens") / g("delivered") } else { 0.0 };
    let mut score: f64 = 100.0;
    let mut flags: Vec<Value> = Vec::new();
    let mut flag = |level: &str, text: String| flags.push(json!({"level": level, "text": text}));
    if complaint >= 0.003 {
        score -= 45.0;
        flag("critical", format!("{name} spam complaints are {}, at or over Gmail's 0.3% limit. Stop emailing people who haven't opened or clicked in months.", pct(complaint)));
    } else if complaint >= 0.001 {
        score -= 20.0;
        flag("warning", format!("{name} complaints above 0.1% ({}). Mailbox providers start filtering to spam around here.", pct(complaint)));
    }
    if bounce >= 0.05 {
        score -= 35.0;
        flag("critical", format!("{name} bounces are {}. Amazon SES reviews senders at 5%. Check how these addresses were collected.", pct(bounce)));
    } else if bounce >= 0.02 {
        score -= 15.0;
        flag("warning", format!("{name} bounces above 2% ({}). Remove old or mistyped addresses.", pct(bounce)));
    }
    if unsub >= 0.01 {
        score -= 15.0;
        flag("warning", format!("{name} unsubscribes are high ({}). Send less often, or only to people who engage.", pct(unsub)));
    } else if unsub >= 0.005 {
        score -= 5.0;
        flag("info", format!("{name} unsubscribes above 0.5% ({}).", pct(unsub)));
    }
    if g("delivered") >= 500.0 && open < 0.1 {
        score -= 10.0;
        flag("warning", format!("Few opens at {name} ({}). Your email may be landing in spam there.", pct(open)));
    }
    let score = score.clamp(0.0, 100.0).round() as i64;
    let grade = if score >= 85 { "good" } else if score >= 65 { "fair" } else { "poor" };
    json!({"score": score, "grade": grade, "flags": flags})
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverabilityQuery {
    /// Last `days` days (default 30, at most 365).
    pub days: Option<i64>,
    /// `day` or `week` (default: day up to 31 days, else week).
    pub interval: Option<String>,
}

fn auth_status(records: &Value) -> Value {
    let arr = records.as_array().cloned().unwrap_or_default();
    let of = |purpose: &str| -> &'static str {
        let rs: Vec<&Value> = arr.iter().filter(|r| r["purpose"] == purpose).collect();
        if rs.is_empty() {
            return "missing";
        }
        if rs.iter().all(|r| r["status"] == "verified") {
            "pass"
        } else if purpose == "dmarc" && rs.iter().any(|r| r["status"] == "recommended") {
            "missing"
        } else {
            "pending"
        }
    };
    json!({"dkim": of("dkim"), "spf": of("spf"), "dmarc": of("dmarc")})
}

pub async fn report(state: &AppState, account_id: Uuid, q: DeliverabilityQuery) -> AppResult<Value> {
    let days = q.days.unwrap_or(30);
    if !(1..=365).contains(&days) {
        return Err(AppError::bad("days must be 1-365"));
    }
    let interval = match q.interval.as_deref() {
        Some(i @ ("day" | "week")) => i,
        Some(_) => return Err(AppError::bad("interval must be day or week")),
        None if days <= 31 => "day",
        None => "week",
    };
    let (tz_name, _) = query::account_tz(state, account_id).await?;
    let now = Utc::now();
    let w = Window { start: now - chrono::Duration::days(days), end: now };

    let base = |q: &mut Sql| -> String {
        let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
        format!(
            "FROM (SELECT m.*, lower(split_part(m.to_address, '@', 2)) AS dom FROM messages m
                   WHERE m.account_id = $1 AND m.channel = 'email' AND NOT m.is_test AND m.status <> 'skipped' AND m.created_at >= {s} AND m.created_at < {e}) m
             LEFT JOIN inbox_provider_cache ipc ON ipc.domain = m.dom"
        )
    };
    let mut tx = query::timed(state).await?;

    let mut by_provider = Sql::new(account_id);
    let from = base(&mut by_provider);
    let prov = provider_sql(&mut by_provider);
    by_provider.push(&format!("SELECT {prov} AS provider, {} {from} GROUP BY 1", aggregates()));
    let rows = by_provider.fetch_all(&mut tx).await?;
    let mut raw_by: HashMap<String, HashMap<&'static str, f64>> = rows.iter().map(|r| (r.try_get::<String, _>("provider").unwrap_or_default(), read_raw(r))).collect();
    let mut overall: HashMap<&'static str, f64> = RAW.iter().map(|k| (*k, 0.0)).collect();
    for raw in raw_by.values() {
        for (k, v) in raw {
            *overall.entry(k).or_default() += v;
        }
    }
    let providers: Vec<Value> = PROVIDERS
        .iter()
        .map(|(key, label)| {
            let raw = raw_by.remove(*key).unwrap_or_else(|| RAW.iter().map(|k| (*k, 0.0)).collect());
            let mut v = rates(&raw);
            v["provider"] = json!(key);
            v["label"] = json!(label);
            v["health"] = health(label, &raw);
            v
        })
        .collect();

    let mut series = Sql::new(account_id);
    let tzp = series.bind(Arg::Text(tz_name.clone()));
    let from = base(&mut series);
    let prov = provider_sql(&mut series);
    series.push(&format!(
        "SELECT to_char(date_trunc('{interval}', m.created_at AT TIME ZONE {tzp}), 'YYYY-MM-DD') AS bucket, {prov} AS provider, {} {from} GROUP BY 1, 2 ORDER BY 1",
        aggregates()
    ));
    let series_rows: Vec<Value> = series
        .fetch_all(&mut tx)
        .await?
        .iter()
        .map(|r| {
            let mut v = rates(&read_raw(r));
            v["date"] = json!(r.try_get::<String, _>("bucket").unwrap_or_default());
            v["provider"] = json!(r.try_get::<String, _>("provider").unwrap_or_default());
            v
        })
        .collect();

    let mut by_domain = Sql::new(account_id);
    let from = base(&mut by_domain);
    by_domain.push(&format!("SELECT COALESCE(m.from_domain, '') AS domain, {} {from} GROUP BY 1 ORDER BY 2 DESC LIMIT 50", aggregates()));
    let domain_rows = by_domain.fetch_all(&mut tx).await?;
    tx.commit().await?;

    let auth: Vec<(String, String, Value, Option<chrono::DateTime<Utc>>)> =
        sqlx::query_as("SELECT domain, status, records, verified_at FROM sending_domains WHERE account_id = $1 ORDER BY created_at").bind(account_id).fetch_all(&state.db).await?;
    let auth_for = |d: &str| auth.iter().find(|a| d == a.0 || d.ends_with(&format!(".{}", a.0)));
    let domains: Vec<Value> = domain_rows
        .iter()
        .map(|r| {
            let d: String = r.try_get("domain").unwrap_or_default();
            let raw = read_raw(r);
            let mut v = rates(&raw);
            let name = if d.is_empty() { "Unrecorded sender".to_string() } else { d.clone() };
            v["domain"] = json!(if d.is_empty() { Value::Null } else { json!(d) });
            v["label"] = json!(if d.is_empty() { "Not recorded (sent before this report started)".to_string() } else { d.clone() });
            v["health"] = health(&name, &raw);
            v["authentication"] = auth_for(&d).map(|a| auth_status(&a.2)).unwrap_or(Value::Null);
            v
        })
        .collect();
    let authentication: Vec<Value> = auth
        .iter()
        .map(|(d, status, records, verified_at)| {
            let mut a = auth_status(records);
            a["domain"] = json!(d);
            a["status"] = json!(status);
            a["verified_at"] = json!(verified_at);
            a
        })
        .collect();

    // Look up custom recipient domains' MX records in the background for next time.
    queue_mx_lookup(state, account_id).await?;

    let mut all = rates(&overall);
    all["health"] = health("Your", &overall);
    Ok(json!({
        "days": days, "interval": interval, "timezone": tz_name, "range": {"start": w.start, "end": w.end},
        "overall": all, "providers": providers, "series": series_rows, "sending_domains": domains, "authentication": authentication,
        "provider_note": "Providers come from the recipient's email domain. Custom domains (like a company's own) are matched by their MX records when they point at Google, Microsoft, Yahoo or iCloud.",
    }))
}

async fn queue_mx_lookup(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let queued: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'reports_mx_lookup' AND payload->>'account_id' = $1 AND run_at <> 'infinity')")
        .bind(account_id.to_string())
        .fetch_one(&state.db)
        .await?;
    if !queued {
        crate::jobs::enqueue(&state.db, "reports_mx_lookup", json!({"account_id": account_id}), None).await?;
    }
    Ok(())
}

fn plausible_domain(d: &str) -> bool {
    d.contains('.') && d.len() < 254 && d.split('.').all(|l| !l.is_empty() && l.len() < 64 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// Job: classify up to 50 of the account's busiest custom recipient domains by MX record. Results
/// are public DNS facts, cached for 30 days and shared by all accounts.
pub async fn mx_lookup(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    let domains: Vec<String> = sqlx::query_scalar(
        "SELECT d FROM (SELECT lower(split_part(to_address, '@', 2)) AS d, count(*) AS n FROM messages
                        WHERE account_id = $1 AND channel = 'email' AND NOT is_test AND created_at > now() - interval '30 days'
                        GROUP BY 1 HAVING count(*) >= 3) x
         WHERE NOT EXISTS (SELECT 1 FROM inbox_provider_cache c WHERE c.domain = x.d AND c.checked_at > now() - interval '30 days')
         ORDER BY n DESC LIMIT 200",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let mut done = 0;
    for d in domains {
        if done >= 50 {
            break;
        }
        if known_provider(&d).is_some() || !plausible_domain(&d) {
            continue;
        }
        done += 1;
        let mx = crate::domains::lookup(state, &d, "MX").await;
        let provider = provider_from_mx(&mx);
        sqlx::query("INSERT INTO inbox_provider_cache (domain, provider, checked_at) VALUES ($1, $2, now()) ON CONFLICT (domain) DO UPDATE SET provider = $2, checked_at = now()")
            .bind(&d)
            .bind(provider)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

/// Record the domain an email was sent from (for deliverability by sending domain).
pub async fn note_sender(db: &sqlx::PgPool, message_id: Uuid, from_email: &str) -> sqlx::Result<()> {
    let domain = from_email.rsplit_once('@').map(|(_, d)| d.trim().to_ascii_lowercase()).filter(|d| !d.is_empty() && d.len() < 254);
    sqlx::query("UPDATE messages SET from_domain = $2 WHERE id = $1").bind(message_id).bind(domain).execute(db).await?;
    Ok(())
}

pub const BENCHMARKS_JSON: &str = include_str!("benchmarks.json");

pub fn benchmark_table() -> Value {
    serde_json::from_str(BENCHMARKS_JSON).unwrap_or_default()
}

/// The account's email rates over the last 30 days next to its last 90 days and published ranges.
pub async fn benchmarks(state: &AppState, account_id: Uuid, industry: Option<&str>) -> AppResult<Value> {
    let table = benchmark_table();
    let industries = table["engagement"]["industries"].as_array().cloned().unwrap_or_default();
    let key = industry.unwrap_or("all");
    let chosen = industries.iter().find(|i| i["key"] == key).cloned().ok_or_else(|| AppError::bad("unknown industry"))?;
    let now = Utc::now();
    let mut tx = query::timed(state).await?;
    let window = |days: i64| {
        let mut q = Sql::new(account_id);
        let since = q.bind(Arg::Ts(now - chrono::Duration::days(days)));
        q.push(&format!(
            "SELECT {} FROM messages m WHERE m.account_id = $1 AND m.channel = 'email' AND NOT m.is_test AND m.status <> 'skipped' AND m.created_at >= {since}",
            aggregates()
        ));
        q
    };
    let recent = window(30).fetch_all(&mut tx).await?;
    let trailing = window(90).fetch_all(&mut tx).await?;
    tx.commit().await?;
    let recent = recent.first().map(read_raw).unwrap_or_default();
    let trailing = trailing.first().map(read_raw).unwrap_or_default();
    let (r, t) = (rates(&recent), rates(&trailing));
    let range = &table["engagement"]["range"];
    let limits = &table["limits"];
    let metric = |k: &str, label: &str, up_is_good: bool| {
        let you = r[k].as_f64().unwrap_or(0.0);
        let mut v = json!({"key": k, "label": label, "you": you, "trailing_90_days": t[k], "up_is_good": up_is_good});
        if let Some(b) = chosen.get(k) {
            v["industry"] = b.clone();
            v["industry_range"] = range[k].clone();
            let typical = b.as_f64().unwrap_or(0.0);
            v["verdict"] = json!(if recent.get("sent").copied().unwrap_or(0.0) < MIN_VOLUME {
                "not_enough_data"
            } else if (you >= typical) == up_is_good {
                "better"
            } else {
                "worse"
            });
        }
        if let Some(l) = limits.get(k) {
            v["limit"] = l.clone();
            let good = l["good_below"].as_f64().unwrap_or(0.0);
            v["verdict"] = json!(if recent.get("sent").copied().unwrap_or(0.0) < MIN_VOLUME { "not_enough_data" } else if you < good { "better" } else { "worse" });
        }
        v
    };
    Ok(json!({
        "industry": chosen,
        "industries": industries.iter().map(|i| json!({"key": i["key"], "label": i["label"]})).collect::<Vec<_>>(),
        "sent_last_30_days": r["sent"], "sent_last_90_days": t["sent"],
        "metrics": [
            metric("open_rate", "Open rate", true),
            metric("click_rate", "Click rate", true),
            metric("unsubscribe_rate", "Unsubscribe rate", false),
            metric("bounce_rate", "Bounce rate", false),
            metric("complaint_rate", "Spam complaint rate", false),
        ],
        "sources": {
            "engagement": {"name": table["engagement"]["source"], "url": table["engagement"]["url"], "published": table["engagement"]["published"],
                           "period": table["engagement"]["period"], "note": table["engagement"]["note"]},
            "bounce_rate": limits["bounce_rate"], "complaint_rate": limits["complaint_rate"],
        },
        "note": "Benchmarks come from the published reports cited here, never from other DumbMail accounts.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_from_domains_and_mx() {
        assert_eq!(known_provider("GMail.com"), Some("gmail"));
        assert_eq!(known_provider("aol.com"), Some("yahoo"));
        assert_eq!(known_provider("me.com"), Some("apple"));
        assert_eq!(known_provider("acme.com"), None);
        assert_eq!(provider_from_mx(&["1 aspmx.l.google.com".into()]), "gmail");
        assert_eq!(provider_from_mx(&["0 acme-com.mail.protection.outlook.com".into()]), "outlook");
        assert_eq!(provider_from_mx(&["1 mta5.am0.yahoodns.net".into()]), "yahoo");
        assert_eq!(provider_from_mx(&["10 mx01.mail.icloud.com".into()]), "apple");
        assert_eq!(provider_from_mx(&["10 mx.acme.com".into()]), "other");
        assert_eq!(provider_from_mx(&[]), "other");
    }

    #[test]
    fn health_flags_in_plain_words() {
        let raw = |sent: f64, complaints: f64, bounces: f64| -> HashMap<&'static str, f64> {
            [("sent", sent), ("delivered", sent - bounces), ("opens", sent * 0.3), ("complaints", complaints), ("bounces", bounces)].into_iter().collect()
        };
        let h = health("Gmail", &raw(10_000.0, 15.0, 50.0));
        assert_eq!(h["flags"][0]["text"], "Gmail complaints above 0.1% (0.15%). Mailbox providers start filtering to spam around here.");
        assert_eq!(h["score"], 80);
        let bad = health("Yahoo", &raw(1000.0, 5.0, 80.0));
        assert_eq!(bad["grade"], "poor");
        assert_eq!(health("Apple", &raw(50.0, 5.0, 5.0))["grade"], "not_enough_data");
        let t = benchmark_table();
        assert!(t["engagement"]["industries"].as_array().unwrap().iter().any(|i| i["key"] == "all"));
        assert!(t["engagement"]["url"].as_str().unwrap().starts_with("https://"));
    }
}

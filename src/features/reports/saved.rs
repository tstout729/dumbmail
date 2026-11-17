//! Saved reports, the ready-made ones that ship in the product, and emailed schedules.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::custom::{self, Spec};
use super::query::{self, Mode};
use super::recipients;
use crate::error::{AppError, AppResult};
use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

const MAX_SAVED: i64 = 200;
const MAX_RECIPIENTS: usize = 10;

pub struct Builtin {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub spec: fn() -> Value,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        key: "campaign_performance",
        name: "Campaign performance",
        description: "Every campaign from the last 90 days side by side: opens, clicks, unsubscribes and the revenue each one earned.",
        spec: || json!({"source": "campaigns", "group_by": ["campaign"], "range": {"days": 90}, "sort": {"by": "revenue", "desc": true},
                       "measures": ["sent", "open_rate", "click_rate", "unsubscribe_rate", "conversions", "revenue", "revenue_per_recipient"]}),
    },
    Builtin {
        key: "flow_steps",
        name: "Flow performance by step",
        description: "Each email and text in your flows over the last 90 days, so you can see which steps earn and which lose people.",
        spec: || json!({"source": "flow_messages", "group_by": ["message"], "range": {"days": 90}, "sort": {"by": "revenue", "desc": true},
                       "measures": ["sent", "open_rate", "click_rate", "unsubscribe_rate", "conversions", "revenue", "revenue_per_recipient"]}),
    },
    Builtin {
        key: "list_growth",
        name: "List growth and churn",
        description: "People joining your lists each week against people unsubscribing, hard bouncing or marking you as spam.",
        spec: || json!({"source": "list_growth", "group_by": ["week"], "range": {"days": 90}, "compare": true,
                       "measures": ["joined", "unsubscribed", "bounced", "complaints", "net_change"]}),
    },
    Builtin {
        key: "revenue_by_source",
        name: "Revenue by channel and source",
        description: "Store revenue in the last 30 days split by what earned it: campaigns or flows, email or text, or no message at all.",
        spec: || json!({"source": "revenue", "group_by": ["source", "channel"], "range": {"days": 30}, "compare": true, "sort": {"by": "revenue", "desc": true},
                       "measures": ["conversions", "revenue", "average_order_value"]}),
    },
    Builtin {
        key: "form_conversion",
        name: "Sign-up form conversion",
        description: "Views, sign-ups and sign-up rate for each form over the last 30 days.",
        spec: || json!({"source": "forms", "group_by": ["form"], "range": {"days": 30}, "compare": true, "sort": {"by": "sign_ups", "desc": true},
                       "measures": ["views", "sign_ups", "sign_up_rate"]}),
    },
    Builtin {
        key: "engagement_tiers",
        name: "Engagement tiers",
        description: "Everyone you can email, by when they last opened or clicked: active, at risk or unengaged.",
        spec: || json!({"source": "engagement", "group_by": ["tier"], "measures": ["profiles", "share"]}),
    },
    Builtin {
        key: "repeat_purchase",
        name: "Repeat purchase rate",
        description: "Customers each month over the last year, and how many of them had bought before.",
        spec: || json!({"source": "customers", "group_by": ["month"], "range": {"days": 365},
                       "measures": ["customers", "returning_customers", "repeat_purchase_rate", "orders", "revenue_per_customer"]}),
    },
    Builtin {
        key: "send_heatmap",
        name: "Send volume by day and hour",
        description: "When your messages went out over the last 90 days, by day of week and hour, with open and click rates.",
        spec: || json!({"source": "messages", "group_by": ["weekday", "hour"], "range": {"days": 90},
                       "measures": ["sent", "open_rate", "click_rate"]}),
    },
];

pub fn builtin(key: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.key == key)
}

fn builtin_json(b: &Builtin) -> Value {
    json!({"id": format!("builtin:{}", b.key), "builtin": true, "name": b.name, "description": b.description, "spec": (b.spec)(),
           "schedule": null, "recipients": [], "recipient_status": [], "next_run_at": null, "last_sent_at": null, "last_error": null})
}

type Row = (Uuid, String, String, Value, Option<String>, Vec<Uuid>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<String>, DateTime<Utc>, DateTime<Utc>);
const COLS: &str = "id, name, description, spec, schedule, schedule_recipients, next_run_at, last_sent_at, last_error, created_at, updated_at";

type Statuses = std::collections::HashMap<Uuid, (String, Option<String>, &'static str)>;

/// `recipient_status` lists each recipient with `confirmed` or `waiting` (for their confirmation).
fn row_json(r: Row, st: &Statuses) -> Value {
    json!({"id": r.0, "builtin": false, "name": r.1, "description": r.2, "spec": r.3, "schedule": r.4,
           "recipient_status": recipients::recipient_json(&r.5, st), "recipients": r.5,
           "next_run_at": r.6, "last_sent_at": r.7, "last_error": r.8, "created_at": r.9, "updated_at": r.10})
}

async fn rows_json(state: &AppState, account_id: Uuid, rows: Vec<Row>) -> AppResult<Vec<Value>> {
    let mut ids: Vec<Uuid> = rows.iter().flat_map(|r| r.5.iter().copied()).collect();
    ids.sort();
    ids.dedup();
    let st = recipients::statuses(state, account_id, &ids).await?;
    Ok(rows.into_iter().map(|r| row_json(r, &st)).collect())
}

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM saved_reports WHERE account_id = $1 ORDER BY updated_at DESC LIMIT {MAX_SAVED}"))
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    Ok(json!({"builtin": BUILTINS.iter().map(builtin_json).collect::<Vec<_>>(), "saved": rows_json(state, account_id, rows).await?}))
}

/// A saved report (`<uuid>`) or a ready-made one (`builtin:<key>`).
pub async fn get(state: &AppState, account_id: Uuid, id: &str) -> AppResult<Value> {
    if let Some(key) = id.strip_prefix("builtin:") {
        return builtin(key).map(builtin_json).ok_or_else(|| AppError::not_found("report"));
    }
    let id: Uuid = id.parse().map_err(|_| AppError::not_found("report"))?;
    let r: Option<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM saved_reports WHERE id = $1 AND account_id = $2"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let r = r.ok_or_else(|| AppError::not_found("report"))?;
    Ok(rows_json(state, account_id, vec![r]).await?.remove(0))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: Option<String>,
    pub description: Option<String>,
    pub spec: Option<Spec>,
    /// `weekly`, `monthly` or null to stop emailing it.
    #[serde(default, deserialize_with = "double_option")]
    pub schedule: Option<Option<String>>,
    /// User ids of account members who get the scheduled email.
    pub recipients: Option<Vec<Uuid>>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

fn clean_name(n: &str) -> AppResult<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 120 {
        return Err(AppError::bad("give the report a name of 1-120 characters"));
    }
    Ok(n.to_string())
}

fn clean_description(d: &str) -> AppResult<String> {
    let d = d.trim();
    if d.chars().count() > 500 {
        return Err(AppError::bad("keep the description under 500 characters"));
    }
    Ok(d.to_string())
}

/// Members of the account (for schedule recipients), with `report_emails`: `confirmed`, `waiting`
/// (a confirmation link was sent) or `not_asked`.
pub async fn members(state: &AppState, account_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<(Uuid, String, Option<String>, String)> = sqlx::query_as(
        "SELECT u.id, u.email, u.name, m.role FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let st = recipients::statuses(state, account_id, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|(id, email, name, role)| {
            let status = st.get(&id).map_or("not_asked", |s| s.2);
            json!({"id": id, "email": email, "name": name, "role": role, "report_emails": status})
        })
        .collect())
}

async fn check_recipients(state: &AppState, account_id: Uuid, ids: &[Uuid]) -> AppResult<Vec<Uuid>> {
    let mut ids = ids.to_vec();
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_RECIPIENTS {
        return Err(AppError::bad(format!("send a scheduled report to at most {MAX_RECIPIENTS} people")));
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE account_id = $1 AND user_id = ANY($2)")
        .bind(account_id)
        .bind(&ids)
        .fetch_one(&state.db)
        .await?;
    if n != ids.len() as i64 {
        return Err(AppError::bad("scheduled reports can only go to members of this account"));
    }
    Ok(ids)
}

/// The next send time: Mondays (weekly) or the 1st (monthly) at 8 a.m. in the account's time zone.
pub fn next_run(schedule: &str, tz: chrono_tz::Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let today = now.with_timezone(&tz).date_naive();
    let at8 = |d: NaiveDate| query::day_start(tz, d) + Duration::hours(8);
    let mut d = match schedule {
        "monthly" => today.with_day(1).unwrap_or(today),
        _ => today - Duration::days(today.weekday().num_days_from_monday() as i64),
    };
    while at8(d) <= now {
        d = match schedule {
            "monthly" => {
                let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
                NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d + Duration::days(31))
            }
            _ => d + Duration::days(7),
        };
    }
    at8(d)
}

async fn schedule_fields(state: &AppState, account_id: Uuid, schedule: Option<String>, recipients: Vec<Uuid>) -> AppResult<(Option<String>, Vec<Uuid>, Option<DateTime<Utc>>)> {
    match schedule.as_deref() {
        None => Ok((None, recipients, None)),
        Some(s @ ("weekly" | "monthly")) => {
            let recipients = check_recipients(state, account_id, &recipients).await?;
            if recipients.is_empty() {
                return Err(AppError::bad("pick at least one member to email the report to"));
            }
            let (_, tz) = query::account_tz(state, account_id).await?;
            Ok((Some(s.to_string()), recipients, Some(next_run(s, tz, Utc::now()))))
        }
        Some(_) => Err(AppError::bad("schedule must be weekly, monthly or null")),
    }
}

pub async fn create(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, b: Input) -> AppResult<Value> {
    let name = clean_name(b.name.as_deref().unwrap_or(""))?;
    let description = clean_description(b.description.as_deref().unwrap_or(""))?;
    let spec = custom::validate(b.spec.ok_or_else(|| AppError::bad("spec is required"))?)?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM saved_reports WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    if count >= MAX_SAVED {
        return Err(AppError::bad(format!("an account can save up to {MAX_SAVED} reports; delete one first")));
    }
    let (schedule, recipients, next) = schedule_fields(state, account_id, b.schedule.flatten(), b.recipients.unwrap_or_default()).await?;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO saved_reports (id, account_id, name, description, spec, schedule, schedule_recipients, next_run_at, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(account_id)
    .bind(&name)
    .bind(&description)
    .bind(serde_json::to_value(&spec)?)
    .bind(&schedule)
    .bind(&recipients)
    .bind(next)
    .bind(user_id)
    .execute(&state.db)
    .await?;
    if schedule.is_some() {
        recipients::ask(state, account_id, &recipients).await?;
    }
    get(state, account_id, &id.to_string()).await
}

pub async fn update(state: &AppState, account_id: Uuid, id: &str, b: Input) -> AppResult<Value> {
    if id.starts_with("builtin:") {
        return Err(AppError::bad("ready-made reports can't be changed; duplicate it first"));
    }
    let cur = get(state, account_id, id).await?;
    let id: Uuid = id.parse().map_err(|_| AppError::not_found("report"))?;
    let name = clean_name(b.name.as_deref().unwrap_or(cur["name"].as_str().unwrap_or("")))?;
    let description = clean_description(b.description.as_deref().unwrap_or(cur["description"].as_str().unwrap_or("")))?;
    let spec = match b.spec {
        Some(s) => custom::validate(s)?,
        None => custom::validate(serde_json::from_value(cur["spec"].clone())?)?,
    };
    let schedule = match b.schedule {
        Some(s) => s,
        None => cur["schedule"].as_str().map(str::to_string),
    };
    let recipients = match b.recipients {
        Some(r) => r,
        None => serde_json::from_value(cur["recipients"].clone()).unwrap_or_default(),
    };
    let schedule_changed = schedule.as_deref() != cur["schedule"].as_str();
    let (schedule, recipients, next) = schedule_fields(state, account_id, schedule, recipients).await?;
    // Keep the next send time unless the frequency changed.
    let next = if schedule_changed { next } else { cur["next_run_at"].as_str().and_then(|s| s.parse().ok()).or(next) };
    sqlx::query(
        "UPDATE saved_reports SET name = $3, description = $4, spec = $5, schedule = $6, schedule_recipients = $7, next_run_at = $8,
             last_error = CASE WHEN $6 IS NULL THEN NULL ELSE last_error END, updated_at = now()
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .bind(&name)
    .bind(&description)
    .bind(serde_json::to_value(&spec)?)
    .bind(&schedule)
    .bind(&recipients)
    .bind(next)
    .execute(&state.db)
    .await?;
    if schedule.is_some() {
        recipients::ask(state, account_id, &recipients).await?;
    }
    get(state, account_id, &id.to_string()).await
}

pub async fn duplicate(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, id: &str, name: Option<String>) -> AppResult<Value> {
    let cur = get(state, account_id, id).await?;
    let name = name.unwrap_or_else(|| format!("{} (copy)", cur["name"].as_str().unwrap_or("Report")));
    create(
        state,
        account_id,
        user_id,
        Input {
            name: Some(name.chars().take(120).collect()),
            description: cur["description"].as_str().map(str::to_string),
            spec: Some(serde_json::from_value(cur["spec"].clone())?),
            schedule: None,
            recipients: None,
        },
    )
    .await
}

pub async fn delete(state: &AppState, account_id: Uuid, id: &str) -> AppResult<()> {
    if id.starts_with("builtin:") {
        return Err(AppError::bad("ready-made reports can't be deleted"));
    }
    let id: Uuid = id.parse().map_err(|_| AppError::not_found("report"))?;
    let n = sqlx::query("DELETE FROM saved_reports WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("report"));
    }
    Ok(())
}

/// A report result as CSV: one column per group and measure. Rates are percentages.
pub fn result_csv(result: &Value) -> String {
    let mut w = csv::Writer::from_writer(Vec::new());
    let cols = result["columns"].as_array().cloned().unwrap_or_default();
    let header: Vec<String> = cols
        .iter()
        .map(|c| {
            let label = c["label"].as_str().unwrap_or("");
            match c["kind"].as_str() {
                Some("rate") => format!("{label} (%)"),
                _ => label.to_string(),
            }
        })
        .collect();
    let _ = w.write_record(&header);
    let groups = cols.iter().filter(|c| c["kind"] == "group").count();
    let cell = |c: &Value, v: &Value| -> String {
        let n = v.as_f64().unwrap_or(0.0);
        match c["kind"].as_str() {
            Some("rate") => format!("{:.2}", n * 100.0),
            Some("currency") => format!("{n:.2}"),
            Some("count") => format!("{}", n.round() as i64),
            _ => format!("{}", (n * 100.0).round() / 100.0),
        }
    };
    for row in result["rows"].as_array().cloned().unwrap_or_default() {
        let mut rec: Vec<String> = (0..groups).map(|i| super::exports::safe_cell(row["labels"][i].as_str().unwrap_or(""))).collect();
        for c in cols.iter().skip(groups) {
            rec.push(cell(c, &row["values"][c["key"].as_str().unwrap_or("")]));
        }
        let _ = w.write_record(&rec);
    }
    let mut total: Vec<String> = (0..groups).map(|i| if i == 0 { "Total".into() } else { String::new() }).collect();
    if groups > 0 {
        for c in cols.iter().skip(groups) {
            total.push(cell(c, &result["totals"][c["key"].as_str().unwrap_or("")]));
        }
        let _ = w.write_record(&total);
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn fmt(kind: &str, v: &Value) -> String {
    let n = v.as_f64().unwrap_or(0.0);
    match kind {
        "rate" => format!("{:.1}%", n * 100.0),
        "currency" => format!("${n:.2}"),
        "count" => {
            let s = (n.round() as i64).to_string();
            let mut out = String::new();
            for (i, ch) in s.chars().rev().enumerate() {
                if i > 0 && i % 3 == 0 && ch != '-' {
                    out.push(',');
                }
                out.push(ch);
            }
            out.chars().rev().collect()
        }
        _ => format!("{:.2}", n),
    }
}

/// Run a saved report and email it, with a link to the CSV, to its recipients who confirmed their
/// address. Called by the job for the send that was due at `occurrence`. Each recipient's delivery is
/// written down before the email goes out, so if the job runs again (a worker restarted) nobody gets
/// it twice.
pub async fn send_scheduled(state: &AppState, report_id: Uuid, occurrence: DateTime<Utc>) -> anyhow::Result<()> {
    let row: Option<(Uuid, String, Value, Vec<Uuid>)> =
        sqlx::query_as("SELECT account_id, name, spec, schedule_recipients FROM saved_reports WHERE id = $1 AND schedule IS NOT NULL")
            .bind(report_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((account_id, name, spec, recipient_ids)) = row else { return Ok(()) };
    let fail = |msg: String| async move {
        sqlx::query("UPDATE saved_reports SET last_error = $2 WHERE id = $1").bind(report_id).bind(&msg).execute(&state.db).await.map(|_| ())
    };
    let Some(from) = state.cfg.support_email.clone().filter(|s| s.contains('@')) else {
        tracing::warn!(report = %report_id, "no SUPPORT_EMAIL set; scheduled report not emailed");
        fail("Scheduled emails need SUPPORT_EMAIL set on the server.".into()).await?;
        return Ok(());
    };
    // Only people who are still members and confirmed their address get it.
    let to = recipients::confirmed(state, account_id, &recipient_ids).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if to.is_empty() {
        fail("No one gets this report yet: each recipient has to confirm their address from the email we sent them.".into()).await?;
        return Ok(());
    }
    let spec: Spec = serde_json::from_value(spec)?;
    // A report grouped by people's own values goes only to members who may see profiles, as
    // checked now (roles change after a report is scheduled).
    let to = if spec.people_values().is_some() {
        let ids: Vec<Uuid> = to.iter().map(|(id, _)| *id).collect();
        let roles: Vec<(Uuid, String)> = sqlx::query_as("SELECT user_id, role FROM memberships WHERE account_id = $1 AND user_id = ANY($2)").bind(account_id).bind(&ids).fetch_all(&state.db).await?;
        let may: Vec<Uuid> = roles
            .into_iter()
            .filter(|(_, r)| crate::features::team::Role::parse(r).is_some_and(|r| r.has(crate::features::team::Perm::ProfilesRead)))
            .map(|(id, _)| id)
            .collect();
        to.into_iter().filter(|(id, _)| may.contains(id)).collect::<Vec<_>>()
    } else {
        to
    };
    if to.is_empty() {
        fail("No one gets this report: it's grouped by people's own details, and none of its recipients may see profiles.".into()).await?;
        return Ok(());
    }
    let result = match custom::run(state, account_id, spec, Mode::Job).await {
        Ok(r) => r,
        // Another report is running for the account, or the server is busy: try again shortly.
        Err(e @ AppError::Busy(_)) => return Err(anyhow::anyhow!(e.public_message())),
        Err(e) => {
            fail(e.public_message()).await?;
            return Ok(());
        }
    };
    let csv = result_csv(&result);
    let (_, tz) = query::account_tz(state, account_id).await?;
    let today = Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string();
    let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    // One file per scheduled send, even if the job runs again.
    let params = json!({"report_id": report_id, "occurrence": occurrence});
    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM exports WHERE account_id = $1 AND kind = 'report' AND params = $2 LIMIT 1")
        .bind(account_id)
        .bind(&params)
        .fetch_optional(&state.db)
        .await?;
    let export_id = match existing {
        Some(id) => id,
        None => super::exports::store_file(state, account_id, "report", params, &format!("{}-{today}.csv", slug.trim_matches('-')), csv.as_bytes()).await?,
    };

    let account_name: String = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let link = format!("{}/analytics/exports", state.cfg.public_url());
    let cols = result["columns"].as_array().cloned().unwrap_or_default();
    let groups = cols.iter().filter(|c| c["kind"] == "group").count();
    let mut text = format!("{name}\n{account_name} · {today}\n\n");
    let mut html = format!(
        "<div style=\"font-family:system-ui,sans-serif;font-size:14px;line-height:1.5;color:#18181b\"><h2 style=\"font-size:18px;margin:0 0 4px\">{}</h2><p style=\"margin:0 0 16px;color:#71717a\">{} · {today}</p><table style=\"border-collapse:collapse\">",
        esc(&name),
        esc(&account_name)
    );
    for c in cols.iter().skip(groups) {
        let k = c["key"].as_str().unwrap_or("");
        let v = fmt(c["kind"].as_str().unwrap_or(""), &result["totals"][k]);
        let label = c["label"].as_str().unwrap_or("");
        text.push_str(&format!("{label}: {v}\n"));
        html.push_str(&format!("<tr><td style=\"padding:2px 16px 2px 0;color:#52525b\">{}</td><td style=\"padding:2px 0;font-weight:600\">{}</td></tr>", esc(label), esc(&v)));
    }
    html.push_str("</table>");
    let rows = result["rows"].as_array().cloned().unwrap_or_default();
    if groups > 0 && !rows.is_empty() {
        text.push_str("\nTop rows:\n");
        html.push_str("<table style=\"border-collapse:collapse;margin-top:16px;font-size:13px\"><tr>");
        for c in &cols {
            html.push_str(&format!("<th style=\"text-align:left;padding:4px 12px 4px 0;border-bottom:1px solid #e4e4e7\">{}</th>", esc(c["label"].as_str().unwrap_or(""))));
        }
        html.push_str("</tr>");
        for r in rows.iter().take(10) {
            let labels: Vec<String> = (0..groups).map(|i| r["labels"][i].as_str().unwrap_or("").to_string()).collect();
            let vals: Vec<String> = cols.iter().skip(groups).map(|c| fmt(c["kind"].as_str().unwrap_or(""), &r["values"][c["key"].as_str().unwrap_or("")])).collect();
            text.push_str(&format!("- {}: {}\n", labels.join(" · "), vals.join(", ")));
            html.push_str("<tr>");
            for v in labels.iter().chain(vals.iter()) {
                html.push_str(&format!("<td style=\"padding:4px 12px 4px 0;border-bottom:1px solid #f4f4f5\">{}</td>", esc(v)));
            }
            html.push_str("</tr>");
        }
        html.push_str("</table>");
    }
    text.push_str(&format!("\nThe full report is a CSV file in DumbMail for the next 7 days (sign in to download it): {link}\n"));
    html.push_str(&format!(
        "<p style=\"margin-top:20px\">The full report is a CSV file in DumbMail for the next 7 days. <a href=\"{}\">Sign in to download it</a>.</p><p style=\"color:#a1a1aa;font-size:12px\">You get this because a teammate scheduled it in DumbMail. Turn it off on the report's page.</p></div>",
        esc(&link)
    ));
    let mut errors = Vec::new();
    let mut sent = 0;
    for (user_id, addr) in &to {
        let claimed = sqlx::query("INSERT INTO report_deliveries (report_id, occurrence, user_id, email) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(report_id)
            .bind(occurrence)
            .bind(user_id)
            .bind(addr)
            .execute(&state.db)
            .await?
            .rows_affected();
        if claimed == 0 {
            continue; // Already sent (or tried) for this occurrence.
        }
        let email = OutgoingEmail {
            message_id: Uuid::now_v7(),
            to: addr.clone(),
            from_name: "DumbMail reports".into(),
            from_email: from.clone(),
            reply_to: None,
            subject: format!("{} · {today}", name.chars().take(80).collect::<String>()),
            html: html.clone(),
            text: text.clone(),
            unsubscribe_url: None,
        };
        let (status, error) = match state.email.send(&email).await {
            Ok(_) => {
                sent += 1;
                ("sent", None)
            }
            Err(e) => {
                tracing::error!(report = %report_id, error = %format!("{e:#}"), "couldn't email a scheduled report");
                errors.push(addr.clone());
                ("failed", Some(format!("{e:#}").chars().take(500).collect::<String>()))
            }
        };
        sqlx::query("UPDATE report_deliveries SET status = $4, error = $5 WHERE report_id = $1 AND occurrence = $2 AND user_id = $3")
            .bind(report_id)
            .bind(occurrence)
            .bind(user_id)
            .bind(status)
            .bind(error)
            .execute(&state.db)
            .await?;
    }
    let err = (!errors.is_empty()).then(|| format!("Couldn't email {}.", errors.join(", ")));
    sqlx::query("UPDATE saved_reports SET last_sent_at = now(), last_error = $2 WHERE id = $1").bind(report_id).bind(err).execute(&state.db).await?;
    tracing::info!(report = %report_id, export = %export_id, recipients = sent, "scheduled report sent");
    Ok(())
}

/// Queue the reports that are due, moving each one's next send time forward first so a slow run
/// never sends twice.
pub async fn enqueue_due(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<(Uuid, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT r.id, r.schedule, a.timezone, r.next_run_at FROM saved_reports r JOIN accounts a ON a.id = r.account_id
         WHERE r.schedule IS NOT NULL AND r.next_run_at <= now() AND a.closed_at IS NULL ORDER BY r.next_run_at LIMIT 50",
    )
    .fetch_all(&state.db)
    .await?;
    for (id, schedule, tz, due_at) in due {
        let tz = tz.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::America::Los_Angeles);
        let next = next_run(&schedule, tz, Utc::now());
        let mut tx = state.db.begin().await?;
        let claimed = sqlx::query("UPDATE saved_reports SET next_run_at = $2 WHERE id = $1 AND next_run_at = $3")
            .bind(id)
            .bind(next)
            .bind(due_at)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if claimed == 1 {
            // The time it was due names this send, so a replayed job is recognised.
            crate::jobs::enqueue(&mut *tx, "reports_scheduled_send", json!({"report_id": id, "occurrence": due_at}), None).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_land_at_8am_local() {
        let tz = chrono_tz::America::Los_Angeles;
        let now: DateTime<Utc> = "2026-11-17T20:00:00Z".parse().unwrap(); // Tuesday, noon Pacific
        assert_eq!(next_run("weekly", tz, now).to_rfc3339(), "2026-11-23T16:00:00+00:00");
        assert_eq!(next_run("monthly", tz, now).to_rfc3339(), "2026-12-01T16:00:00+00:00");
        // Monday 7 a.m. Pacific: today at 8.
        let early: DateTime<Utc> = "2026-11-23T15:00:00Z".parse().unwrap();
        assert_eq!(next_run("weekly", tz, early).to_rfc3339(), "2026-11-23T16:00:00+00:00");
        for b in BUILTINS {
            custom::validate(serde_json::from_value((b.spec)()).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", b.key));
        }
    }

    #[test]
    fn csv_has_headers_totals_and_safe_cells() {
        let result = json!({
            "columns": [{"key": "campaign", "label": "Campaign", "kind": "group"}, {"key": "sent", "label": "Sent", "kind": "count"}, {"key": "open_rate", "label": "Open rate", "kind": "rate"}],
            "rows": [{"keys": ["x"], "labels": ["=HYPERLINK(\"evil\")"], "values": {"sent": 10, "open_rate": 0.25}}],
            "totals": {"sent": 10, "open_rate": 0.25}
        });
        let csv = result_csv(&result);
        assert_eq!(csv, "Campaign,Sent,Open rate (%)\n\"'=HYPERLINK(\"\"evil\"\")\",10,25.00\nTotal,10,25.00\n");
    }
}

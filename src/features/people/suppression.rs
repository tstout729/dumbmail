//! The suppression list: everyone who can't be emailed or texted, and why. Adding addresses by hand
//! is always allowed; lifting a suppression is only possible for ones added by hand.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use super::consent::{self, Channel, ConsentSource};
use crate::audience::ProfileInput;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone};
use crate::state::AppState;

pub const REASONS: &[&str] = &["unsubscribed", "hard_bounce", "complaint", "manual", "pending", "not_subscribed"];
/// Addresses suppressed in the request itself; more go to a background job.
pub const INLINE_MAX: usize = 200;
pub const REQUEST_MAX: usize = 100_000;
pub const EXPORT_MAX: i64 = 100_000;

#[derive(Debug, Deserialize, Default)]
pub struct SuppressionQuery {
    pub q: Option<String>,
    pub channel: Option<String>,
    pub reason: Option<String>,
    /// Account-time-zone dates (YYYY-MM-DD), inclusive.
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

fn rows_sql(qb: &mut QueryBuilder<'_, sqlx::Postgres>, account_id: Uuid, tz: &str, q: &SuppressionQuery) -> AppResult<()> {
    qb.push(
        "FROM (
            SELECT p.id, 'email' AS channel, p.email AS address, p.first_name, p.last_name, p.updated_at,
                   COALESCE(p.email_suppressed_reason, CASE p.email_consent WHEN 'unsubscribed' THEN 'unsubscribed' ELSE 'pending' END) AS reason
            FROM profiles p WHERE p.account_id = ",
    )
    .push_bind(account_id)
    .push(
        " AND p.email IS NOT NULL AND (p.email_suppressed_reason IS NOT NULL OR p.email_consent IN ('unsubscribed', 'pending'))
            UNION ALL
            SELECT p.id, 'sms', p.phone_number, p.first_name, p.last_name, p.updated_at,
                   COALESCE(p.sms_suppressed_reason, CASE p.sms_consent WHEN 'unsubscribed' THEN 'unsubscribed' ELSE 'not_subscribed' END)
            FROM profiles p WHERE p.account_id = ",
    )
    .push_bind(account_id)
    .push(
        " AND p.phone_number IS NOT NULL AND (p.sms_suppressed_reason IS NOT NULL OR p.sms_consent <> 'subscribed')
         ) s
         LEFT JOIN LATERAL (
            SELECT e.occurred_at, e.method, e.message_id, e.actor_user_id FROM consent_events e
            WHERE e.profile_id = s.id AND e.channel = s.channel AND e.new_value = s.reason AND e.field IN ('consent', 'suppression')
            ORDER BY e.occurred_at DESC LIMIT 1
         ) ev ON true
         WHERE true",
    );
    if let Some(c) = q.channel.as_deref().filter(|c| !c.is_empty()) {
        Channel::parse(c)?;
        qb.push(" AND s.channel = ").push_bind(c.to_string());
    }
    if let Some(r) = q.reason.as_deref().filter(|r| !r.is_empty()) {
        if !REASONS.contains(&r) {
            return Err(AppError::bad(format!("reason must be one of {}", REASONS.join(", "))));
        }
        qb.push(" AND s.reason = ").push_bind(r.to_string());
    }
    if let Some(s) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let like = format!("%{}%", s.chars().take(200).collect::<String>().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        qb.push(" AND (s.address ILIKE ").push_bind(like.clone()).push(" OR (coalesce(s.first_name,'') || ' ' || coalesce(s.last_name,'')) ILIKE ").push_bind(like).push(")");
    }
    let day = |d: &str| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| AppError::bad("dates must look like 2026-11-17"));
    if let Some(f) = q.from.as_deref().filter(|s| !s.is_empty()) {
        let d = day(f)?;
        qb.push(" AND COALESCE(ev.occurred_at, s.updated_at) >= (").push_bind(d).push("::date::timestamp AT TIME ZONE ").push_bind(tz.to_string()).push(")");
    }
    if let Some(t) = q.to.as_deref().filter(|s| !s.is_empty()) {
        let d = day(t)?;
        qb.push(" AND COALESCE(ev.occurred_at, s.updated_at) < ((").push_bind(d).push("::date + 1)::timestamp AT TIME ZONE ").push_bind(tz.to_string()).push(")");
    }
    Ok(())
}

type SupRow = (Uuid, String, String, Option<String>, Option<String>, String, Option<DateTime<Utc>>, Option<String>, Option<Uuid>, Option<String>);

async fn fetch(db: &PgPool, account_id: Uuid, q: &SuppressionQuery, limit: i64, offset: i64) -> AppResult<(Vec<SupRow>, i64, String)> {
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(db).await?;
    let mut cq = QueryBuilder::new("SELECT count(*) ");
    rows_sql(&mut cq, account_id, &tz, q)?;
    let total: i64 = cq.build_query_scalar().fetch_one(db).await?;
    // "since" is the recorded change; a block from before consent history started has none.
    let mut dq = QueryBuilder::new("SELECT s.id, s.channel, s.address, s.first_name, s.last_name, s.reason, ev.occurred_at, ev.method, ev.message_id, (SELECT u.email FROM users u WHERE u.id = ev.actor_user_id) ");
    rows_sql(&mut dq, account_id, &tz, q)?;
    dq.push(" ORDER BY COALESCE(ev.occurred_at, s.updated_at) DESC, s.id, s.channel LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);
    let rows: Vec<SupRow> = dq.build_query_as().fetch_all(db).await?;
    Ok((rows, total, tz))
}

fn reason_text(channel: &str, reason: &str) -> &'static str {
    match (channel, reason) {
        ("email", "unsubscribed") => "Unsubscribed from email",
        ("sms", "unsubscribed") => "Opted out of texts",
        (_, "hard_bounce") => "Hard bounce",
        (_, "complaint") => "Spam complaint",
        (_, "manual") => "Suppressed by hand",
        (_, "pending") => "Hasn't confirmed sign-up",
        (_, "not_subscribed") => "Never agreed to texts",
        _ => "Suppressed",
    }
}

pub async fn list(db: &PgPool, account_id: Uuid, q: &SuppressionQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let (rows, total, _) = fetch(db, account_id, q, limit, q.offset.unwrap_or(0).max(0)).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|r| {
            let ch = if r.1 == "sms" { Channel::Sms } else { Channel::Email };
            let consent = match r.5.as_str() {
                "unsubscribed" => "unsubscribed",
                "pending" => "pending",
                "not_subscribed" => "never_subscribed",
                _ => "",
            };
            let suppressed = (!matches!(r.5.as_str(), "unsubscribed" | "pending" | "not_subscribed")).then_some(r.5.as_str());
            let why_not = consent::why_not_unsuppressible(ch, suppressed, consent);
            json!({
                "profile_id": r.0, "channel": r.1, "address": r.2, "first_name": r.3, "last_name": r.4, "reason": r.5,
                "reason_text": reason_text(&r.1, &r.5), "since": r.6,
                "how": r.7.as_deref().map(|m| consent::describe_how(m, r.9.as_deref(), None, None, &Value::Null)),
                "can_unsuppress": why_not.is_none(), "why_not": why_not,
            })
        })
        .collect();
    let counts: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT 'email', COALESCE(email_suppressed_reason, CASE email_consent WHEN 'unsubscribed' THEN 'unsubscribed' ELSE 'pending' END), count(*)
         FROM profiles WHERE account_id = $1 AND email IS NOT NULL AND (email_suppressed_reason IS NOT NULL OR email_consent IN ('unsubscribed', 'pending')) GROUP BY 2
         UNION ALL
         SELECT 'sms', COALESCE(sms_suppressed_reason, CASE sms_consent WHEN 'unsubscribed' THEN 'unsubscribed' ELSE 'not_subscribed' END), count(*)
         FROM profiles WHERE account_id = $1 AND phone_number IS NOT NULL AND (sms_suppressed_reason IS NOT NULL OR sms_consent <> 'subscribed') GROUP BY 2",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let deleted: i64 = sqlx::query_scalar("SELECT count(*) FROM suppression_tombstones WHERE account_id = $1").bind(account_id).fetch_one(db).await?;
    Ok(json!({
        "data": data, "total": total,
        "counts": counts.iter().map(|c| json!({"channel": c.0, "reason": c.1, "count": c.2})).collect::<Vec<_>>(),
        "deleted_profiles_suppressed": deleted,
    }))
}

fn csv_field(s: &str) -> String {
    // Leading =, +, - or @ would run as a formula in a spreadsheet.
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

pub fn csv_line(fields: &[&str]) -> String {
    let mut line = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
    line.push('\n');
    line
}

/// The list as CSV (at most `EXPORT_MAX` rows), with dates in the account's time zone.
pub async fn export_csv(db: &PgPool, account_id: Uuid, q: &SuppressionQuery) -> AppResult<String> {
    let (rows, _, tz) = fetch(db, account_id, q, EXPORT_MAX, 0).await?;
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let mut out = csv_line(&["address", "channel", "reason", "since", "first_name", "last_name", "profile_id"]);
    for r in rows {
        let since = r.6.map(|d| d.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "before history".into());
        out.push_str(&csv_line(&[&r.2, &r.1, &r.5, &since, r.3.as_deref().unwrap_or(""), r.4.as_deref().unwrap_or(""), &r.0.to_string()]));
    }
    Ok(out)
}

#[derive(Debug, Deserialize, Default)]
pub struct SuppressBody {
    /// Email addresses and/or phone numbers.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// CSV text: a column named email or phone (or the first column) is read.
    #[serde(default)]
    pub csv: Option<String>,
    #[serde(default)]
    pub confirm: bool,
}

pub enum Address {
    Email(String),
    Phone(String),
}

/// Read pasted addresses and CSV text into valid addresses (deduplicated) and the ones that aren't.
pub fn parse_addresses(b: &SuppressBody) -> AppResult<(Vec<Address>, Vec<String>)> {
    let mut raw: Vec<String> = b.addresses.iter().flat_map(|a| a.split([',', ';', '\n', '\r', '\t'])).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if let Some(csv) = b.csv.as_deref().filter(|c| !c.trim().is_empty()) {
        let mut rdr = csv::ReaderBuilder::new().flexible(true).has_headers(false).trim(csv::Trim::All).from_reader(csv.as_bytes());
        let mut col: Option<Vec<usize>> = None;
        for (i, rec) in rdr.records().enumerate() {
            let rec = rec.map_err(|e| AppError::bad(format!("could not read the CSV: {e}")))?;
            if i == 0 {
                let named: Vec<usize> = rec.iter().enumerate().filter(|(_, h)| {
                    let k: String = h.trim_start_matches('\u{feff}').to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
                    matches!(k.as_str(), "email" | "emailaddress" | "phone" | "phonenumber" | "mobile" | "sms")
                }).map(|(j, _)| j).collect();
                if !named.is_empty() {
                    col = Some(named);
                    continue;
                }
            }
            match &col {
                Some(cols) => raw.extend(cols.iter().filter_map(|j| rec.get(*j)).filter(|s| !s.is_empty()).map(String::from)),
                None => raw.extend(rec.get(0).filter(|s| !s.is_empty()).map(String::from)),
            }
            if raw.len() > REQUEST_MAX {
                break;
            }
        }
    }
    if raw.len() > REQUEST_MAX {
        return Err(AppError::bad(format!("at most {REQUEST_MAX} addresses at a time; split the file")));
    }
    let mut seen = std::collections::HashSet::new();
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for a in raw {
        let parsed = if a.contains('@') { normalize_email(&a).map(Address::Email) } else { normalize_phone(&a).map(Address::Phone) };
        match parsed {
            Some(p) => {
                let key = match &p {
                    Address::Email(e) | Address::Phone(e) => e.clone(),
                };
                if seen.insert(key) {
                    ok.push(p);
                }
            }
            None => bad.push(a.chars().take(100).collect()),
        }
    }
    Ok((ok, bad))
}

/// Suppress one address by hand (creating the profile if needed). Returns whether it changed.
pub async fn suppress_address(state: &AppState, account_id: Uuid, a: &Address, src: &ConsentSource) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    let changed = suppress_address_in(state, &mut tx, account_id, a, src).await?;
    tx.commit().await?;
    Ok(changed)
}

/// [`suppress_address`] with the suppression written in the caller's transaction. A profile it
/// has to create is created (never subscribed) and committed first.
pub async fn suppress_address_in(state: &AppState, conn: &mut sqlx::PgConnection, account_id: Uuid, a: &Address, src: &ConsentSource) -> AppResult<bool> {
    let (input, ch) = match a {
        Address::Email(e) => (ProfileInput { email: Some(e.clone()), ..Default::default() }, Channel::Email),
        Address::Phone(p) => (ProfileInput { phone_number: Some(p.clone()), ..Default::default() }, Channel::Sms),
    };
    let profile = match crate::audience::find_profile(&state.db, account_id, &input).await? {
        Some(p) => p,
        None => consent::with_source(src.clone(), crate::audience::upsert_profile(&state.db, account_id, &input)).await?.profile,
    };
    Ok(consent::suppress_in(conn, account_id, profile.id, ch, "manual", false, false, src).await?)
}

/// Suppress many addresses: a few at once, more in a background job.
pub async fn suppress_many(state: &AppState, account_id: Uuid, b: &SuppressBody, src: &ConsentSource) -> AppResult<Value> {
    let (ok, bad) = parse_addresses(b)?;
    if ok.is_empty() {
        return Err(AppError::bad(if bad.is_empty() { "add at least one email address or phone number".to_string() } else { format!("none of these look like an email address or phone number: {}", bad.iter().take(5).cloned().collect::<Vec<_>>().join(", ")) }));
    }
    if !b.confirm {
        return Ok(json!({"dry_run": true, "count": ok.len(), "invalid": bad.iter().take(50).collect::<Vec<_>>(), "invalid_count": bad.len(),
            "note": "Nothing suppressed yet. Suppressed people get no marketing on that channel until someone lifts the suppression here. Call again with confirm=true once the user approves."}));
    }
    if ok.len() <= INLINE_MAX {
        let mut changed = 0;
        for a in &ok {
            if suppress_address(state, account_id, a, src).await? {
                changed += 1;
            }
        }
        return Ok(json!({"suppressed": changed, "already_suppressed": ok.len() - changed, "invalid": bad.iter().take(50).collect::<Vec<_>>(), "invalid_count": bad.len()}));
    }
    let addresses: Vec<String> = ok.iter().map(|a| match a {
        Address::Email(e) | Address::Phone(e) => e.clone(),
    }).collect();
    let job = super::bulk::start_address_job(state, account_id, &addresses, src).await?;
    Ok(json!({"bulk_action": job, "count": addresses.len(), "invalid": bad.iter().take(50).collect::<Vec<_>>(), "invalid_count": bad.len()}))
}

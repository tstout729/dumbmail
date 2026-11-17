//! Privacy requests: export everything held about one person, or delete them fully while keeping a
//! minimal suppression record (a salted one-way code of their address and the reason) so they stay
//! suppressed if they come back in an import. Also the files people download after signing in.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::consent::{self, ConsentSource, ConsentState};
use super::merge::PROFILE_TABLES;
use crate::audience::ProfileInput;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone, Profile};
use crate::state::AppState;

pub const FILE_DAYS: i64 = 7;
const EXPORT_EVENTS: i64 = 10_000;
const EXPORT_MESSAGES: i64 = 10_000;

/// This account's random salt for one-way codes (made the first time it's needed).
pub async fn salt(db: &PgPool, account_id: Uuid) -> AppResult<String> {
    sqlx::query("INSERT INTO privacy_salts (account_id, salt) VALUES ($1, $2) ON CONFLICT (account_id) DO NOTHING")
        .bind(account_id)
        .bind(crate::auth::random_token(32))
        .execute(db)
        .await?;
    Ok(sqlx::query_scalar("SELECT salt FROM privacy_salts WHERE account_id = $1").bind(account_id).fetch_one(db).await?)
}

/// The one-way code for an address. Emails are reduced to the inbox they reach (case, +tags and
/// Gmail dots don't matter), so a re-import under a variant of the address still matches.
pub fn address_hash(salt: &str, channel: &str, address: &str) -> String {
    let key = if channel == "email" { crate::optin::email_key(address) } else { address.trim().to_string() };
    crate::auth::sha256_hex(&format!("{salt}|{channel}|{key}"))
}

/// What a profile's state says to keep after it's gone: (channel, address, reason).
fn tombstone_reasons(s: &ConsentState) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    if let Some(e) = &s.email {
        if let Some(r) = &s.email_suppressed_reason {
            out.push(("email", e.clone(), r.clone()));
        }
        if s.email_consent == "unsubscribed" {
            out.push(("email", e.clone(), "unsubscribed".to_string()));
        }
    }
    if let Some(p) = &s.phone_number {
        if let Some(r) = &s.sms_suppressed_reason {
            out.push(("sms", p.clone(), r.clone()));
        }
        if s.sms_consent == "unsubscribed" {
            out.push(("sms", p.clone(), "unsubscribed".to_string()));
        }
    }
    out
}

/// Keep the suppression record for addresses of `s` that are going away. `keep` lists addresses
/// that stay on a profile (a merge keeps some), which need no record.
pub async fn write_tombstones(conn: &mut PgConnection, account_id: Uuid, salt: &str, s: &ConsentState, keep: &[&str]) -> sqlx::Result<u64> {
    let mut n = 0;
    for (channel, address, reason) in tombstone_reasons(s) {
        if keep.contains(&address.as_str()) {
            continue;
        }
        n += sqlx::query("INSERT INTO suppression_tombstones (account_id, channel, address_hash, reason) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(account_id)
            .bind(channel)
            .bind(address_hash(salt, channel, &address))
            .bind(&reason)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    }
    Ok(n)
}

/// A new profile whose address matches a deleted person's suppression record gets it back. Bounces,
/// complaints and manual suppressions always come back; an unsubscribe gives way only to the person's
/// own new opt-in (a form sign-up), never to an import or an API call. Returns whether anything changed.
pub async fn apply_tombstones(db: &PgPool, account_id: Uuid, profile: &Profile, input: &ProfileInput, own_opt_in: bool) -> AppResult<bool> {
    if profile.email.is_none() && profile.phone_number.is_none() {
        return Ok(false);
    }
    let any: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM suppression_tombstones WHERE account_id = $1)").bind(account_id).fetch_one(db).await?;
    if !any {
        return Ok(false);
    }
    let salt = salt(db, account_id).await?;
    let hashes: Vec<String> = [("email", &profile.email), ("sms", &profile.phone_number)]
        .iter()
        .filter_map(|(c, a)| a.as_deref().map(|a| address_hash(&salt, c, a)))
        .collect();
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT channel, reason FROM suppression_tombstones WHERE account_id = $1 AND address_hash = ANY($2)")
        .bind(account_id)
        .bind(&hashes)
        .fetch_all(db)
        .await?;
    if rows.is_empty() {
        return Ok(false);
    }
    let has = |c: &str, r: &str| rows.iter().any(|(ch, re)| ch == c && re == r);
    let mut tx = db.begin().await?;
    let Some(before) = consent::load_state(&mut tx, account_id, profile.id, true).await? else { return Ok(false) };
    let mut after = before.clone();
    if before.email.is_some() {
        if after.email_suppressed_reason.is_none() {
            after.email_suppressed_reason = ["hard_bounce", "complaint", "manual"].iter().find(|r| has("email", r)).map(|r| r.to_string());
        }
        let opted_in_now = own_opt_in && input.email_consent.as_deref() == Some("subscribed");
        if (has("email", "unsubscribed") || has("email", "complaint")) && !opted_in_now {
            after.email_consent = "unsubscribed".into();
        }
    }
    if before.phone_number.is_some() {
        if after.sms_suppressed_reason.is_none() && has("sms", "manual") {
            after.sms_suppressed_reason = Some("manual".into());
        }
        let opted_in_now = own_opt_in && input.sms_consent.as_deref() == Some("subscribed");
        if has("sms", "unsubscribed") && !opted_in_now {
            after.sms_consent = "unsubscribed".into();
        }
    }
    let src = ConsentSource { method: "deleted_profile", ..consent::current_source() };
    let changed = consent::write_state(&mut tx, account_id, profile.id, &before, &after, &src).await?;
    tx.commit().await?;
    Ok(changed)
}

/// Delete a profile and every row keyed by it (see `merge::PROFILE_TABLES`), keeping the minimal
/// suppression record. Returns the rows removed per table, or None when there's no such profile.
pub async fn delete_profile_fully(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Option<Map<String, Value>>> {
    let salt = salt(db, account_id).await?;
    let mut tx = db.begin().await?;
    let Some(state) = consent::load_state(&mut tx, account_id, profile_id, true).await? else { return Ok(None) };
    let mut removed = Map::new();
    let kept = write_tombstones(&mut tx, account_id, &salt, &state, &[]).await?;
    for t in PROFILE_TABLES {
        let n = sqlx::query(&format!("DELETE FROM {} WHERE {} = $1", t.table, t.column)).bind(profile_id).execute(&mut *tx).await?.rows_affected();
        if n > 0 {
            let prev = removed.get(t.table).and_then(Value::as_u64).unwrap_or(0);
            removed.insert(t.table.into(), json!(prev + n));
        }
    }
    sqlx::query("DELETE FROM profiles WHERE id = $1 AND account_id = $2").bind(profile_id).bind(account_id).execute(&mut *tx).await?;
    tx.commit().await?;
    removed.insert("profiles".into(), json!(1));
    removed.insert("suppression_records_kept".into(), json!(kept));
    Ok(Some(removed))
}

#[derive(Deserialize, Default)]
pub struct PersonQuery {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub confirm: bool,
}

struct Person {
    identified_by: &'static str,
    subject: String,
    profile_ids: Vec<Uuid>,
}

async fn find_person(db: &PgPool, account_id: Uuid, q: &PersonQuery) -> AppResult<Person> {
    let email = q.email.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let phone = q.phone_number.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (identified_by, subject) = match (email, phone) {
        (Some(e), None) => ("email", normalize_email(e).ok_or_else(|| AppError::bad(format!("{e:?} is not a valid email")))?),
        (None, Some(p)) => ("phone_number", normalize_phone(p).ok_or_else(|| AppError::bad(format!("{p:?} is not a valid phone number (use +15035550100)")))?),
        _ => return Err(AppError::bad("give either an email or a phone_number")),
    };
    let col = if identified_by == "email" { "email" } else { "phone_number" };
    let profile_ids: Vec<Uuid> = sqlx::query_scalar(&format!("SELECT id FROM profiles WHERE account_id = $1 AND {col} = $2 ORDER BY created_at LIMIT 10"))
        .bind(account_id)
        .bind(&subject)
        .fetch_all(db)
        .await?;
    Ok(Person { identified_by, subject, profile_ids })
}

async fn log_request(db: &PgPool, account_id: Uuid, kind: &str, p: &Person, src: &ConsentSource, result: Value) -> AppResult<Uuid> {
    let salt = salt(db, account_id).await?;
    let id = Uuid::now_v7();
    let channel = if p.identified_by == "email" { "email" } else { "sms" };
    sqlx::query(
        "INSERT INTO privacy_requests (id, account_id, kind, subject_hash, identified_by, requested_by_user, via, profiles_found, result)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(account_id)
    .bind(kind)
    .bind(address_hash(&salt, channel, &p.subject))
    .bind(p.identified_by)
    .bind(src.actor_user_id)
    .bind(src.via)
    .bind(p.profile_ids.len() as i32)
    .bind(result)
    .execute(db)
    .await?;
    Ok(id)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Everything held about one profile.
pub async fn profile_export(state: &AppState, account_id: Uuid, profile_id: Uuid) -> AppResult<Value> {
    let db = &state.db;
    let detail = crate::api::audience::profile_detail(state, account_id, profile_id).await?;
    let extra: (Option<String>, Option<String>, bool, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT email_suppressed_reason, sms_suppressed_reason, ads_opt_out, ads_opt_out_at FROM profiles WHERE id = $1 AND account_id = $2")
            .bind(profile_id)
            .bind(account_id)
            .fetch_one(db)
            .await?;
    let mut profile = detail["profile"].clone();
    profile["email_suppressed_reason"] = json!(extra.0);
    profile["sms_suppressed_reason"] = json!(extra.1);
    profile["do_not_sell_or_share"] = json!(extra.2);
    profile["do_not_sell_or_share_since"] = json!(extra.3);
    let (history, _) = consent::history(db, account_id, profile_id, 500, 0).await?;
    let events: Vec<(String, Value, Option<f64>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT m.name, e.properties, e.value, e.occurred_at FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.account_id = $1 AND e.profile_id = $2 ORDER BY e.occurred_at DESC LIMIT $3",
    )
    .bind(account_id)
    .bind(profile_id)
    .bind(EXPORT_EVENTS)
    .fetch_all(db)
    .await?;
    let messages: Vec<(String, String, Option<String>, String, Option<String>, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT msg.channel, msg.to_address, msg.subject, msg.status, c.name, f.name, msg.sent_at, msg.delivered_at, msg.opened_at, msg.clicked_at, msg.unsubscribed_at, msg.created_at
         FROM messages msg LEFT JOIN campaigns c ON c.id = msg.campaign_id LEFT JOIN flows f ON f.id = msg.flow_id
         WHERE msg.account_id = $1 AND msg.profile_id = $2 AND NOT msg.is_test ORDER BY msg.created_at DESC LIMIT $3",
    )
    .bind(account_id)
    .bind(profile_id)
    .bind(EXPORT_MESSAGES)
    .fetch_all(db)
    .await?;
    let confirmations: Vec<(String, Option<String>, String, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT l.name, f.name, o.email, o.created_at, o.confirmed_at FROM optin_confirmations o JOIN lists l ON l.id = o.list_id
         LEFT JOIN forms f ON f.id = o.form_id WHERE o.account_id = $1 AND o.profile_id = $2 ORDER BY o.created_at DESC LIMIT 1000",
    )
    .bind(account_id)
    .bind(profile_id)
    .fetch_all(db)
    .await?;
    let notes: Vec<(String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT author_label, body, created_at FROM profile_notes WHERE account_id = $1 AND profile_id = $2 ORDER BY created_at DESC LIMIT 1000")
            .bind(account_id)
            .bind(profile_id)
            .fetch_all(db)
            .await?;
    let form_submissions: Vec<Value> = events
        .iter()
        .filter(|e| e.0 == "Subscribed to List" && e.1.get("form_id").is_some_and(|f| !f.is_null()))
        .map(|e| json!({"form": e.1.get("form"), "form_id": e.1.get("form_id"), "list_id": e.1.get("list_id"), "submitted_at": e.3, "double_opt_in": e.1.get("double_opt_in")}))
        .collect();
    Ok(json!({
        "profile": profile,
        "lists": detail["lists"].as_array().map(|a| a.iter().map(|l| json!({"id": l["id"], "name": l["name"]})).collect::<Vec<_>>()),
        "segments": detail["segments"].as_array().map(|a| a.iter().map(|s| json!({"id": s["id"], "name": s["name"]})).collect::<Vec<_>>()),
        "consent_history": history.iter().map(|h| json!({"at": h["occurred_at"], "what": h["summary"], "how": h["how"], "address": h["address"], "ip": h["ip"], "user_agent": h["user_agent"]})).collect::<Vec<_>>(),
        "form_submissions": form_submissions,
        "double_opt_in_confirmations": confirmations.iter().map(|c| json!({"list": c.0, "form": c.1, "email": c.2, "requested_at": c.3, "confirmed_at": c.4})).collect::<Vec<_>>(),
        "events": events.iter().map(|e| json!({"metric": e.0, "properties": e.1, "value": e.2, "occurred_at": e.3})).collect::<Vec<_>>(),
        "messages": messages.iter().map(|m| json!({"channel": m.0, "to": m.1, "subject": m.2, "status": m.3, "campaign": m.4, "flow": m.5, "sent_at": m.6, "delivered_at": m.7, "opened_at": m.8, "clicked_at": m.9, "unsubscribed_at": m.10, "created_at": m.11})).collect::<Vec<_>>(),
        "internal_notes": notes.iter().map(|n| json!({"author": n.0, "note": n.1, "written_at": n.2})).collect::<Vec<_>>(),
        "limits": {"events": EXPORT_EVENTS, "messages": EXPORT_MESSAGES, "events_truncated": events.len() as i64 >= EXPORT_EVENTS, "messages_truncated": messages.len() as i64 >= EXPORT_MESSAGES},
    }))
}

fn html_summary(account_name: &str, subject: &str, tz: &str, exported: &[Value]) -> String {
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let when = |v: &Value| v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()).map(|d| d.with_timezone(&tz).format("%b %-d, %Y %-I:%M %p %Z").to_string()).unwrap_or_else(|| "—".into());
    let text = |v: &Value| match v {
        Value::Null => "—".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut h = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Data held about {s}</title><style>body{{font:14px/1.5 system-ui,sans-serif;color:#18181b;max-width:900px;margin:32px auto;padding:0 16px}}h1{{font-size:22px}}h2{{font-size:17px;margin-top:28px}}table{{border-collapse:collapse;width:100%;margin:8px 0}}td,th{{border:1px solid #e4e4e7;padding:6px 8px;text-align:left;vertical-align:top}}th{{background:#fafafa}}.muted{{color:#71717a}}</style></head><body>\
         <h1>Data {a} holds about {s}</h1><p class=\"muted\">Exported {now}. The attached JSON file has the same data in full.</p>",
        s = esc(subject),
        a = esc(account_name),
        now = Utc::now().with_timezone(&tz).format("%b %-d, %Y %-I:%M %p %Z")
    );
    for (i, p) in exported.iter().enumerate() {
        if exported.len() > 1 {
            h.push_str(&format!("<h2>Profile {}</h2>", i + 1));
        }
        h.push_str("<h2>Profile</h2><table>");
        if let Some(o) = p["profile"].as_object() {
            for (k, v) in o {
                if k == "properties" {
                    continue;
                }
                h.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(&text(v))));
            }
            if let Some(props) = o.get("properties").and_then(|v| v.as_object()) {
                for (k, v) in props {
                    h.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(&text(v))));
                }
            }
        }
        h.push_str("</table>");
        let names = |k: &str| p[k].as_array().map(|a| a.iter().map(|x| esc(&text(&x["name"]))).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty()).unwrap_or_else(|| "None".into());
        h.push_str(&format!("<h2>Lists</h2><p>{}</p><h2>Segments</h2><p>{}</p>", names("lists"), names("segments")));
        h.push_str("<h2>Consent history</h2><table><tr><th>When</th><th>What</th><th>How</th></tr>");
        for c in p["consent_history"].as_array().into_iter().flatten() {
            h.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>", when(&c["at"]), esc(&text(&c["what"])), esc(&text(&c["how"]))));
        }
        h.push_str("</table><h2>Form sign-ups</h2><table><tr><th>When</th><th>Form</th></tr>");
        for f in p["form_submissions"].as_array().into_iter().flatten() {
            h.push_str(&format!("<tr><td>{}</td><td>{}</td></tr>", when(&f["submitted_at"]), esc(&text(&f["form"]))));
        }
        h.push_str("</table><h2>Messages sent</h2><table><tr><th>When</th><th>Channel</th><th>Subject</th><th>Outcome</th></tr>");
        for m in p["messages"].as_array().into_iter().flatten() {
            h.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", when(&m["created_at"]), esc(&text(&m["channel"])), esc(&text(&m["subject"])), esc(&text(&m["status"]))));
        }
        let events = p["events"].as_array().map(|a| a.len()).unwrap_or(0);
        h.push_str(&format!("</table><h2>Activity</h2><p>{events} recorded events (website visits, orders, email opens and clicks and so on). Each one is in the JSON file.</p>"));
        let notes = p["internal_notes"].as_array().map(|a| a.len()).unwrap_or(0);
        if notes > 0 {
            h.push_str("<h2>Internal notes</h2><table><tr><th>When</th><th>By</th><th>Note</th></tr>");
            for n in p["internal_notes"].as_array().into_iter().flatten() {
                h.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>", when(&n["written_at"]), esc(&text(&n["author"])), esc(&text(&n["note"]))));
            }
            h.push_str("</table>");
        }
    }
    h.push_str("</body></html>");
    h
}

pub async fn save_file(db: &PgPool, account_id: Uuid, kind: &str, filename: &str, content_type: &str, bytes: &[u8], request_id: Option<Uuid>) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO people_files (id, account_id, kind, filename, content_type, bytes, request_id, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8))")
        .bind(id)
        .bind(account_id)
        .bind(kind)
        .bind(filename)
        .bind(content_type)
        .bind(bytes)
        .bind(request_id)
        .bind(FILE_DAYS as i32)
        .execute(db)
        .await?;
    Ok(id)
}

pub fn file_url(id: Uuid) -> String {
    format!("/api/people/files/{id}")
}

/// Export everything held about one person (found by email or phone) as a JSON file and a readable
/// HTML page, downloadable after signing in for 7 days. The request is logged.
pub async fn export_person(state: &AppState, account_id: Uuid, q: &PersonQuery, src: &ConsentSource) -> AppResult<Value> {
    let p = find_person(&state.db, account_id, q).await?;
    if p.profile_ids.is_empty() {
        let id = log_request(&state.db, account_id, "export", &p, src, json!({"found": false})).await?;
        return Ok(json!({"request_id": id, "found": 0, "files": [], "note": "Nobody with that address is in this account, so there's nothing to export."}));
    }
    let mut exported = Vec::new();
    for pid in &p.profile_ids {
        exported.push(profile_export(state, account_id, *pid).await?);
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let request_id = log_request(&state.db, account_id, "export", &p, src, json!({"found": true})).await?;
    let data = json!({"exported_at": Utc::now(), "business": account.name, "person": p.subject, "profiles": exported});
    let stamp = Utc::now().format("%Y-%m-%d");
    let json_bytes = serde_json::to_vec_pretty(&data).map_err(|e| AppError::Internal(e.into()))?;
    let html = html_summary(&account.name, &p.subject, &account.timezone, &exported);
    let json_id = save_file(&state.db, account_id, "privacy_export", &format!("personal-data-{stamp}.json"), "application/json", &json_bytes, Some(request_id)).await?;
    let html_id = save_file(&state.db, account_id, "privacy_export", &format!("personal-data-{stamp}.html"), "text/html; charset=utf-8", html.as_bytes(), Some(request_id)).await?;
    let expires = Utc::now() + Duration::days(FILE_DAYS);
    Ok(json!({
        "request_id": request_id,
        "found": p.profile_ids.len(),
        "expires_at": expires,
        "files": [
            {"id": json_id, "name": format!("personal-data-{stamp}.json"), "url": file_url(json_id)},
            {"id": html_id, "name": format!("personal-data-{stamp}.html"), "url": file_url(html_id)},
        ],
        "data": data,
    }))
}

/// Delete everything held about one person (found by email or phone). Without `confirm` it only
/// says what would be deleted. Their old export files go too; the request is logged.
pub async fn delete_person(state: &AppState, account_id: Uuid, q: &PersonQuery, src: &ConsentSource) -> AppResult<Value> {
    let p = find_person(&state.db, account_id, q).await?;
    if !q.confirm {
        let mut profiles = Vec::new();
        for pid in &p.profile_ids {
            let counts: (i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM events WHERE profile_id = $1), (SELECT count(*) FROM messages WHERE profile_id = $1), (SELECT count(*) FROM list_members WHERE profile_id = $1)",
            )
            .bind(pid)
            .fetch_one(&state.db)
            .await?;
            profiles.push(json!({"profile_id": pid, "events": counts.0, "messages": counts.1, "lists": counts.2}));
        }
        return Ok(json!({
            "dry_run": true,
            "found": p.profile_ids.len(),
            "profiles": profiles,
            "note": if p.profile_ids.is_empty() { "Nobody with that address is in this account.".to_string() } else {
                "Nothing deleted yet. This deletes the profile with its events, messages, list memberships and consent history, and can't be undone. If they had unsubscribed or were suppressed, a one-way code of the address is kept so they stay suppressed if they're imported again. Call again with confirm=true once the user approves.".to_string()
            },
        }));
    }
    let mut removed = Vec::new();
    let mut addresses: Vec<String> = vec![p.subject.clone()];
    for pid in &p.profile_ids {
        let a: (Option<String>, Option<String>) = sqlx::query_as("SELECT email, phone_number FROM profiles WHERE id = $1").bind(pid).fetch_one(&state.db).await?;
        addresses.extend([a.0, a.1].into_iter().flatten());
        addresses.push(pid.to_string());
    }
    for pid in &p.profile_ids {
        if let Some(r) = delete_profile_fully(&state.db, account_id, *pid).await? {
            removed.push(Value::Object(r));
        }
    }
    // Earlier exports of this person's data go too.
    let salt = salt(&state.db, account_id).await?;
    let channel = if p.identified_by == "email" { "email" } else { "sms" };
    let files = sqlx::query(
        "DELETE FROM people_files WHERE account_id = $1 AND request_id IN (SELECT id FROM privacy_requests WHERE account_id = $1 AND kind = 'export' AND subject_hash = $2)",
    )
    .bind(account_id)
    .bind(address_hash(&salt, channel, &p.subject))
    .execute(&state.db)
    .await?
    .rows_affected();
    // Bulk exports that include them, saved searches that name them, and pending bulk targets.
    let bulk_files = sqlx::query("DELETE FROM people_files WHERE account_id = $1 AND kind = 'bulk_export' AND EXISTS (SELECT 1 FROM unnest($2::text[]) a WHERE position(convert_to(a, 'UTF8') IN bytes) > 0)")
        .bind(account_id)
        .bind(&addresses)
        .execute(&state.db)
        .await?
        .rows_affected();
    sqlx::query("UPDATE bulk_actions SET selection = selection - 'q' WHERE account_id = $1 AND EXISTS (SELECT 1 FROM unnest($2::text[]) a WHERE selection->>'q' ILIKE '%' || a || '%')")
        .bind(account_id)
        .bind(&addresses)
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM bulk_action_targets t USING bulk_actions b WHERE b.id = t.bulk_id AND b.account_id = $1 AND t.address = ANY($2)")
        .bind(account_id)
        .bind(&addresses)
        .execute(&state.db)
        .await?;
    let files = files + bulk_files;
    let id = log_request(&state.db, account_id, "delete", &p, src, json!({"deleted_profiles": removed.len(), "export_files_deleted": files})).await?;
    Ok(json!({"request_id": id, "deleted": removed.len(), "removed": removed, "export_files_deleted": files}))
}

/// The log of privacy requests, newest first.
pub async fn requests(db: &PgPool, account_id: Uuid, limit: i64, offset: i64) -> AppResult<(Vec<Value>, i64)> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_requests WHERE account_id = $1").bind(account_id).fetch_one(db).await?;
    let rows: Vec<(Uuid, String, String, Option<String>, String, i32, Value, DateTime<Utc>, Value)> = sqlx::query_as(
        "SELECT r.id, r.kind, r.identified_by, u.email, r.via, r.profiles_found, r.result, r.created_at,
                COALESCE((SELECT jsonb_agg(jsonb_build_object('id', f.id, 'name', f.filename, 'expires_at', f.expires_at) ORDER BY f.filename)
                          FROM people_files f WHERE f.request_id = r.id AND f.expires_at > now()), '[]'::jsonb)
         FROM privacy_requests r LEFT JOIN users u ON u.id = r.requested_by_user
         WHERE r.account_id = $1 ORDER BY r.created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(account_id)
    .bind(limit.clamp(1, 200))
    .bind(offset.max(0))
    .fetch_all(db)
    .await?;
    let data = rows
        .into_iter()
        .map(|r| {
            let files: Vec<Value> = r.8.as_array().cloned().unwrap_or_default().into_iter().map(|mut f| {
                if let Some(id) = f["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                    f["url"] = json!(file_url(id));
                }
                f
            }).collect();
            json!({"id": r.0, "kind": r.1, "identified_by": r.2, "requested_by": r.3, "via": r.4, "profiles_found": r.5, "result": r.6, "created_at": r.7, "files": files})
        })
        .collect();
    Ok((data, total))
}

/// A file for download, if it belongs to this account and hasn't expired.
pub async fn file(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<(String, String, Vec<u8>)> {
    sqlx::query_as("SELECT filename, content_type, bytes FROM people_files WHERE id = $1 AND account_id = $2 AND expires_at > now()")
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::NotFound("This download has expired or doesn't exist. Downloads last 7 days.".into()))
}

//! Inbox settings (sign-up disclosures, automatic replies, opening hours, email digests) and saved replies.

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::Account;

pub const MAX_REPLY_CHARS: usize = 480;
pub const MAX_SAVED_REPLIES: i64 = 200;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Settings {
    pub program_name: String,
    pub message_frequency: String,
    pub default_reply_enabled: bool,
    pub default_reply: String,
    pub away_reply_enabled: bool,
    pub away_reply: String,
    pub open_hour: i16,
    pub close_hour: i16,
    pub open_days: Vec<i16>,
    pub notify_user_ids: Vec<Uuid>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            program_name: String::new(),
            message_frequency: String::new(),
            default_reply_enabled: false,
            default_reply: String::new(),
            away_reply_enabled: false,
            away_reply: String::new(),
            open_hour: 9,
            close_hour: 17,
            open_days: vec![1, 2, 3, 4, 5],
            notify_user_ids: Vec::new(),
        }
    }
}

const COLS: &str = "program_name, message_frequency, default_reply_enabled, default_reply, away_reply_enabled, away_reply, open_hour, close_hour, open_days, notify_user_ids";

pub async fn load(db: &PgPool, account_id: Uuid) -> AppResult<Settings> {
    let q = format!("SELECT {COLS} FROM sms_inbox_settings WHERE account_id = $1");
    Ok(sqlx::query_as::<_, Settings>(&q).bind(account_id).fetch_optional(db).await?.unwrap_or_default())
}

impl Settings {
    /// Whether `now` falls inside opening hours, in the account's time zone.
    pub fn is_open(&self, account: &Account, now: DateTime<Utc>) -> bool {
        let local = now.with_timezone(&crate::sending::account_tz(account));
        let day = local.weekday().number_from_monday() as i16;
        let hour = local.hour() as i16;
        self.open_days.contains(&day) && hour >= self.open_hour && hour < self.close_hour
    }

    /// Text that ends every keyword sign-up confirmation: what carriers require.
    pub fn disclosure(&self) -> String {
        let freq = self.message_frequency.trim().trim_end_matches('.');
        format!("{freq}. Msg & data rates may apply. Reply HELP for help, STOP to cancel.")
    }
}

fn text(v: &Value, key: &str, max: usize) -> AppResult<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim().to_string();
            if s.chars().count() > max {
                return Err(AppError::bad(format!("{key} can be at most {max} characters")));
            }
            Ok(Some(s))
        }
        Some(_) => Err(AppError::bad(format!("{key} must be text"))),
    }
}

fn flag(v: &Value, key: &str) -> AppResult<Option<bool>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(AppError::bad(format!("{key} must be true or false"))),
    }
}

/// Check that a reply's personalization tags render, so a typo fails here rather than at send time.
pub fn check_template(account: &Account, body: &str, what: &str) -> AppResult<()> {
    let ctx = crate::render::context(account, None, None, "");
    crate::render::render_str(body, &ctx, false).map(|_| ()).map_err(|e| AppError::bad(format!("{what}: {}", e.public_message())))
}

/// Apply a partial update. Unknown keys are refused so typos don't silently do nothing.
pub async fn update(db: &PgPool, account: &Account, patch: &Value) -> AppResult<Settings> {
    let obj: &Map<String, Value> = patch.as_object().ok_or_else(|| AppError::bad("send the settings to change as an object"))?;
    const KEYS: &[&str] = &["program_name", "message_frequency", "default_reply_enabled", "default_reply", "away_reply_enabled", "away_reply", "open_hour", "close_hour", "open_days", "notify_user_ids"];
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(AppError::bad(format!("unknown setting {k:?}")));
    }
    let mut s = load(db, account.id).await?;
    if let Some(t) = text(patch, "program_name", 60)? {
        s.program_name = t;
    }
    if let Some(t) = text(patch, "message_frequency", 60)? {
        s.message_frequency = t;
    }
    if let Some(t) = text(patch, "default_reply", MAX_REPLY_CHARS)? {
        check_template(account, &t, "default reply")?;
        s.default_reply = t;
    }
    if let Some(t) = text(patch, "away_reply", MAX_REPLY_CHARS)? {
        check_template(account, &t, "away reply")?;
        s.away_reply = t;
    }
    if let Some(b) = flag(patch, "default_reply_enabled")? {
        s.default_reply_enabled = b;
    }
    if let Some(b) = flag(patch, "away_reply_enabled")? {
        s.away_reply_enabled = b;
    }
    let hour = |k: &str, lo: i64, hi: i64| -> AppResult<Option<i16>> {
        match patch.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_i64().filter(|h| (lo..=hi).contains(h)).map(|h| Some(h as i16)).ok_or_else(|| AppError::bad(format!("{k} must be a whole hour from {lo} to {hi}"))),
        }
    };
    if let Some(h) = hour("open_hour", 0, 23)? {
        s.open_hour = h;
    }
    if let Some(h) = hour("close_hour", 1, 24)? {
        s.close_hour = h;
    }
    if s.close_hour <= s.open_hour {
        return Err(AppError::bad("closing time must be after opening time"));
    }
    if let Some(v) = patch.get("open_days").filter(|v| !v.is_null()) {
        let days: Vec<i16> = v
            .as_array()
            .ok_or_else(|| AppError::bad("open_days must be a list of weekdays (1 = Monday … 7 = Sunday)"))?
            .iter()
            .map(|d| d.as_i64().filter(|d| (1..=7).contains(d)).map(|d| d as i16))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| AppError::bad("open_days must be a list of weekdays (1 = Monday … 7 = Sunday)"))?;
        let mut days = days;
        days.sort_unstable();
        days.dedup();
        s.open_days = days;
    }
    if let Some(v) = patch.get("notify_user_ids").filter(|v| !v.is_null()) {
        let ids: Vec<Uuid> = serde_json::from_value(v.clone()).map_err(|_| AppError::bad("notify_user_ids must be a list of member ids"))?;
        if ids.len() > 50 {
            return Err(AppError::bad("notify at most 50 members"));
        }
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE account_id = $1 AND user_id = ANY($2)").bind(account.id).bind(&ids).fetch_one(db).await?;
        let mut ids = ids;
        ids.sort_unstable();
        ids.dedup();
        if members != ids.len() as i64 {
            return Err(AppError::bad("notify_user_ids can only name members of this account (get_sms_inbox_settings lists them)"));
        }
        s.notify_user_ids = ids;
    }
    if s.default_reply_enabled && s.default_reply.is_empty() {
        return Err(AppError::bad("write the default reply before turning it on"));
    }
    if s.away_reply_enabled && s.away_reply.is_empty() {
        return Err(AppError::bad("write the away reply before turning it on"));
    }
    sqlx::query(
        "INSERT INTO sms_inbox_settings (account_id, program_name, message_frequency, default_reply_enabled, default_reply, away_reply_enabled, away_reply, open_hour, close_hour, open_days, notify_user_ids, notified_through)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
         ON CONFLICT (account_id) DO UPDATE SET program_name = $2, message_frequency = $3, default_reply_enabled = $4, default_reply = $5,
             away_reply_enabled = $6, away_reply = $7, open_hour = $8, close_hour = $9, open_days = $10, notify_user_ids = $11,
             -- Turning digests on starts from now, not from every text ever received.
             notified_through = CASE WHEN cardinality(sms_inbox_settings.notify_user_ids) = 0 THEN now() ELSE sms_inbox_settings.notified_through END,
             updated_at = now()",
    )
    .bind(account.id)
    .bind(&s.program_name)
    .bind(&s.message_frequency)
    .bind(s.default_reply_enabled)
    .bind(&s.default_reply)
    .bind(s.away_reply_enabled)
    .bind(&s.away_reply)
    .bind(s.open_hour)
    .bind(s.close_hour)
    .bind(&s.open_days)
    .bind(&s.notify_user_ids)
    .execute(db)
    .await?;
    Ok(s)
}

/// Settings plus the account's members (for choosing who gets digests).
pub async fn settings_json(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let s = load(db, account_id).await?;
    let members: Vec<(Uuid, String, Option<String>)> =
        sqlx::query_as("SELECT u.id, u.email, u.name FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 100")
            .bind(account_id)
            .fetch_all(db)
            .await?;
    let mut v = serde_json::to_value(&s).unwrap_or_default();
    v["members"] = json!(members.iter().map(|(id, email, name)| json!({"id": id, "email": email, "name": name})).collect::<Vec<_>>());
    v["disclosure_ready"] = json!(!s.program_name.is_empty() && !s.message_frequency.is_empty());
    Ok(v)
}

// ---------- Saved replies ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SavedReply {
    pub id: Uuid,
    pub name: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn saved_replies(db: &PgPool, account_id: Uuid) -> AppResult<Vec<SavedReply>> {
    Ok(sqlx::query_as("SELECT id, name, body, created_at, updated_at FROM sms_saved_replies WHERE account_id = $1 ORDER BY lower(name), id LIMIT 500")
        .bind(account_id)
        .fetch_all(db)
        .await?)
}

/// Create (no `id`) or update a saved reply.
pub async fn upsert_saved_reply(db: &PgPool, account: &Account, id: Option<Uuid>, v: &Value) -> AppResult<SavedReply> {
    let name = text(v, "name", 80)?;
    let body = text(v, "body", crate::features::inbox::replies::MAX_TEXT_CHARS)?;
    if let Some(b) = &body {
        check_template(account, b, "saved reply")?;
    }
    match id {
        Some(id) => {
            if name.as_deref() == Some("") || body.as_deref() == Some("") {
                return Err(AppError::bad("a saved reply needs a name and text"));
            }
            sqlx::query_as(
                "UPDATE sms_saved_replies SET name = COALESCE($3, name), body = COALESCE($4, body), updated_at = now()
                 WHERE id = $1 AND account_id = $2 RETURNING id, name, body, created_at, updated_at",
            )
            .bind(id)
            .bind(account.id)
            .bind(&name)
            .bind(&body)
            .fetch_optional(db)
            .await?
            .ok_or_else(|| AppError::not_found("saved reply"))
        }
        None => {
            let (Some(name), Some(body)) = (name.filter(|s| !s.is_empty()), body.filter(|s| !s.is_empty())) else {
                return Err(AppError::bad("a saved reply needs a name and text"));
            };
            let mut tx = db.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 62))").bind(account.id).execute(&mut *tx).await?;
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_saved_replies WHERE account_id = $1").bind(account.id).fetch_one(&mut *tx).await?;
            if count >= MAX_SAVED_REPLIES {
                return Err(AppError::Conflict(format!("an account can keep {MAX_SAVED_REPLIES} saved replies; delete some first")));
            }
            let r = sqlx::query_as(
                "INSERT INTO sms_saved_replies (id, account_id, name, body) VALUES ($1, $2, $3, $4) RETURNING id, name, body, created_at, updated_at",
            )
            .bind(Uuid::now_v7())
            .bind(account.id)
            .bind(&name)
            .bind(&body)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(r)
        }
    }
}

pub async fn delete_saved_reply(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM sms_saved_replies WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("saved reply"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn opening_hours_use_the_account_time_zone() {
        let s = Settings::default();
        let mut account = test_account();
        account.timezone = "America/Los_Angeles".into();
        // Tuesday 2026-11-17 18:00 UTC = 10:00 PST: open.
        assert!(s.is_open(&account, Utc.with_ymd_and_hms(2026, 11, 17, 18, 0, 0).unwrap()));
        // Tuesday 2026-11-17 02:00 UTC = Monday 18:00 PST: closed.
        assert!(!s.is_open(&account, Utc.with_ymd_and_hms(2026, 11, 17, 2, 0, 0).unwrap()));
        // Saturday 2026-11-21 18:00 UTC: closed.
        assert!(!s.is_open(&account, Utc.with_ymd_and_hms(2026, 11, 21, 18, 0, 0).unwrap()));
    }

    #[test]
    fn disclosure_has_the_required_parts() {
        let s = Settings { message_frequency: "Up to 4 msgs/month.".into(), ..Default::default() };
        let d = s.disclosure();
        assert!(d.starts_with("Up to 4 msgs/month. Msg & data rates may apply."));
        assert!(d.contains("HELP") && d.contains("STOP"));
    }

    fn test_account() -> Account {
        Account {
            id: Uuid::nil(),
            public_key: String::new(),
            name: "Acme".into(),
            website: None,
            from_name: None,
            from_email: None,
            reply_to: None,
            sms_from: None,
            timezone: "America/Los_Angeles".into(),
            address: None,
            brand_color: "#000".into(),
            logo_url: None,
            social_links: json!({}),
            plan: "free".into(),
            onboarding: json!({}),
            conversion_metric: "Placed Order".into(),
            created_at: Utc::now(),
            sending_paused_at: None,
            sending_paused_reason: None,
            email_daily_limit_override: None,
            email_sunset_enabled: true,
            link_domain: None,
            link_domain_status: String::new(),
            link_domain_records: json!([]),
            verified_domains: Vec::new(),
        }
    }
}

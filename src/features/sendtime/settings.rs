//! SMS quiet hours in each recipient's own time zone. Texts go out between 9:00 and 20:00 by
//! default; an account can narrow that window but never widen it past 8:00 to 21:00 (the hours US
//! telemarketing rules allow).

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::clock::{hhmm, parse_hhmm, window_wait};
use super::zone;
use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile};

pub const DEFAULT_SMS_WINDOW: (u32, u32) = (9 * 60, 20 * 60);
pub const LEGAL_SMS_WINDOW: (u32, u32) = (8 * 60, 21 * 60);

/// The account's SMS window, minutes after local midnight.
pub async fn sms_window(db: &PgPool, account_id: Uuid) -> sqlx::Result<(u32, u32)> {
    let row: Option<(i32, i32)> = sqlx::query_as("SELECT sms_start_minute, sms_end_minute FROM send_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?;
    Ok(row.map_or(DEFAULT_SMS_WINDOW, |(s, e)| clamp_window(s, e)))
}

/// Never outside the legal window, whatever is stored.
fn clamp_window(s: i32, e: i32) -> (u32, u32) {
    let s = (s.max(0) as u32).max(LEGAL_SMS_WINDOW.0);
    let e = (e.max(0) as u32).min(LEGAL_SMS_WINDOW.1);
    if s < e { (s, e) } else { DEFAULT_SMS_WINDOW }
}

/// When a text to this person may next go out, or None if it may go now. Uses the person's zone
/// (falling back to the account's) and the account's SMS window.
pub async fn sms_quiet_until(db: &PgPool, account: &Account, profile: &Profile, now: DateTime<Utc>) -> sqlx::Result<Option<DateTime<Utc>>> {
    let (s, e) = sms_window(db, account.id).await?;
    let zones = sms_zones(&profile.properties, profile.phone_number.as_deref(), crate::sending::account_tz(account));
    Ok(sms_wait(&zones, now, s, e))
}

/// The zones a text must respect. A `timezone` property can come from a browser, so it never makes
/// quiet hours looser than the phone number's own zone: when both are known and differ, both count.
pub fn sms_zones(props: &Value, phone: Option<&str>, account_tz: Tz) -> Vec<Tz> {
    let (tz, _) = zone::resolve_parts(props, phone, account_tz);
    match phone.and_then(zone::phone_zone) {
        Some(p) if p != tz => vec![p, tz],
        _ => vec![tz],
    }
}

/// When a text may go so that it's inside the window in every one of `zones` (the first is the
/// phone's). If their windows never overlap, the phone's zone alone decides.
pub fn sms_wait(zones: &[Tz], now: DateTime<Utc>, start: u32, end: u32) -> Option<DateTime<Utc>> {
    let mut at = now;
    for _ in 0..8 {
        match zones.iter().filter_map(|z| window_wait(*z, at, start, end)).max() {
            None => return (at > now).then_some(at),
            Some(next) => at = next,
        }
        if at > now + chrono::Duration::days(2) {
            break;
        }
    }
    zones.first().and_then(|z| window_wait(*z, now, start, end))
}

pub async fn settings_json(db: &PgPool, account_id: Uuid) -> sqlx::Result<Value> {
    let (s, e) = sms_window(db, account_id).await?;
    Ok(json!({
        "sms_window_start": hhmm(s),
        "sms_window_end": hhmm(e),
        "default_sms_window": {"start": hhmm(DEFAULT_SMS_WINDOW.0), "end": hhmm(DEFAULT_SMS_WINDOW.1)},
        "legal_sms_window": {"start": hhmm(LEGAL_SMS_WINDOW.0), "end": hhmm(LEGAL_SMS_WINDOW.1)},
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub sms_window_start: Option<String>,
    pub sms_window_end: Option<String>,
}

pub async fn update(db: &PgPool, account_id: Uuid, b: SettingsBody) -> AppResult<Value> {
    let (cur_s, cur_e) = sms_window(db, account_id).await?;
    let parse = |v: &Option<String>, cur: u32, field: &str| -> AppResult<u32> {
        match v {
            None => Ok(cur),
            Some(t) => parse_hhmm(t).ok_or_else(|| AppError::bad(format!("{field} must be a time like 09:00"))),
        }
    };
    let s = parse(&b.sms_window_start, cur_s, "sms_window_start")?;
    let e = parse(&b.sms_window_end, cur_e, "sms_window_end")?;
    if s < LEGAL_SMS_WINDOW.0 || e > LEGAL_SMS_WINDOW.1 {
        return Err(AppError::bad("texts can only go out between 08:00 and 21:00 in the recipient's time zone; you can make the window narrower, not wider"));
    }
    if e < s + 60 {
        return Err(AppError::bad("the SMS window must be at least an hour long"));
    }
    sqlx::query(
        "INSERT INTO send_settings (account_id, sms_start_minute, sms_end_minute) VALUES ($1, $2, $3)
         ON CONFLICT (account_id) DO UPDATE SET sms_start_minute = $2, sms_end_minute = $3, updated_at = now()",
    )
    .bind(account_id)
    .bind(s as i32)
    .bind(e as i32)
    .execute(db)
    .await?;
    Ok(settings_json(db, account_id).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_time_zone_never_loosens_quiet_hours() {
        use chrono::TimeZone;
        let la: Tz = chrono_tz::America::Los_Angeles;
        let utc = |h: u32| Utc.with_ymd_and_hms(2026, 11, 20, h, 0, 0).unwrap();
        // New York number claiming Los Angeles: both count. 17:00 UTC = 12:00 NY, 09:00 LA: open in both.
        let z = sms_zones(&serde_json::json!({"timezone": "America/Los_Angeles"}), Some("+12125550100"), la);
        assert_eq!(z.len(), 2);
        assert_eq!(sms_wait(&z, utc(17), 540, 1200), None);
        // 02:00 UTC = 21:00 NY, 18:00 LA: closed in New York, so it waits for 09:00 LA (= 12:00 NY) tomorrow.
        assert_eq!(sms_wait(&z, utc(2), 540, 1200), Some(utc(17)));
        // A New York number claiming Manila (13 hours ahead): the windows never overlap, so New York decides.
        let z = sms_zones(&serde_json::json!({"timezone": "Asia/Manila"}), Some("+12125550100"), la);
        assert_eq!(sms_wait(&z, utc(16), 540, 1200), None, "11:00 in New York");
        assert_eq!(sms_wait(&z, utc(3), 540, 1200), Some(utc(14)), "22:00 in New York waits for 09:00");
        // No phone zone: just the one.
        assert_eq!(sms_zones(&serde_json::json!({"timezone": "Asia/Kolkata"}), Some("+18505550100"), la).len(), 1);
    }

    #[test]
    fn stored_windows_stay_legal() {
        assert_eq!(clamp_window(540, 1200), (540, 1200));
        assert_eq!(clamp_window(300, 1380), (480, 1260));
        assert_eq!(clamp_window(1000, 900), DEFAULT_SMS_WINDOW);
    }
}

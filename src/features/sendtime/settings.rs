//! SMS quiet hours in each recipient's own time zone. Texts go out between 9:00 and 20:00 by
//! default; an account can narrow that window but never widen it past 8:00 to 21:00 (the hours US
//! telemarketing rules allow).

use chrono::{DateTime, Utc};
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
    let (tz, _) = zone::resolve(profile, crate::sending::account_tz(account));
    Ok(window_wait(tz, now, s, e))
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
    fn stored_windows_stay_legal() {
        assert_eq!(clamp_window(540, 1200), (540, 1200));
        assert_eq!(clamp_window(300, 1380), (480, 1260));
        assert_eq!(clamp_window(1000, 900), DEFAULT_SMS_WINDOW);
    }
}

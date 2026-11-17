//! A campaign's send options: when each recipient's message goes out, and smart sending.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::clock::{hhmm, parse_hhmm};
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Timing {
    /// Everyone at the send time (the default).
    #[default]
    AllAtOnce,
    /// At `local_time` in each person's time zone.
    LocalTime,
    /// At each person's best hour inside a window.
    BestTime,
    /// Spread evenly over `spread_hours`.
    Gradual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Late {
    /// People whose local time already passed when sending starts get it at that time the next day.
    #[default]
    NextDay,
    /// They get it right away.
    Immediately,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SendOptions {
    #[serde(default)]
    pub timing: Timing,
    /// local_time: the hour in each person's zone, "HH:MM".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_time: Option<String>,
    /// local_time: what happens when that time already passed for someone when sending starts.
    #[serde(default)]
    pub late: Late,
    /// best_time: the local hours a message may go out in, "HH:MM" to "HH:MM".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_end: Option<String>,
    /// best_time: how far ahead of the send time to look for each person's best hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_hours: Option<i64>,
    /// gradual: hours to spread the send over, in one equal batch per hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_hours: Option<i64>,
    /// Skip people who got a message on this channel in the last `smart_sending_hours`.
    #[serde(default)]
    pub smart_sending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_sending_hours: Option<i64>,
    /// local_time: the day it goes out, set when the campaign is scheduled or sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_date: Option<NaiveDate>,
}

pub const DEFAULT_SMART_HOURS: i64 = 16;
pub const DEFAULT_WITHIN_HOURS: i64 = 24;
pub const DEFAULT_WINDOW: (u32, u32) = (8 * 60, 20 * 60);

impl SendOptions {
    pub fn local_minute(&self) -> u32 {
        self.local_time.as_deref().and_then(parse_hhmm).unwrap_or(10 * 60)
    }
    pub fn window(&self) -> (u32, u32) {
        (
            self.window_start.as_deref().and_then(parse_hhmm).unwrap_or(DEFAULT_WINDOW.0),
            self.window_end.as_deref().and_then(parse_hhmm).unwrap_or(DEFAULT_WINDOW.1),
        )
    }
    pub fn within_hours(&self) -> i64 {
        self.within_hours.unwrap_or(DEFAULT_WITHIN_HOURS)
    }
    pub fn spread_hours(&self) -> i64 {
        self.spread_hours.unwrap_or(2)
    }
    /// Smart sending's window in hours, or None when it's off.
    pub fn smart_hours(&self) -> Option<i64> {
        self.smart_sending.then(|| self.smart_sending_hours.unwrap_or(DEFAULT_SMART_HOURS))
    }
    /// True when messages need a planned release time.
    pub fn plans(&self) -> bool {
        self.timing != Timing::AllAtOnce
    }
}

/// Validate send options and return them with the active mode's settings filled in and the other
/// modes' settings dropped. `null` (or nothing) means the defaults: everyone at once, no smart sending.
pub fn parse(v: &Value) -> AppResult<Option<SendOptions>> {
    if v.is_null() {
        return Ok(None);
    }
    let mut o: SendOptions = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid send_options: {e}")))?;
    let time = |field: &str, s: &Option<String>| -> AppResult<Option<u32>> {
        match s.as_deref() {
            None => Ok(None),
            Some(t) => parse_hhmm(t).map(Some).ok_or_else(|| AppError::bad(format!("send_options.{field} must be a time like 10:00"))),
        }
    };
    let local = time("local_time", &o.local_time)?;
    let (ws, we) = (time("window_start", &o.window_start)?, time("window_end", &o.window_end)?);
    let mut clean = SendOptions { timing: o.timing, smart_sending: o.smart_sending, ..Default::default() };
    match o.timing {
        Timing::AllAtOnce => {}
        Timing::LocalTime => {
            clean.local_time = Some(hhmm(local.unwrap_or(10 * 60)));
            clean.late = o.late;
            clean.local_date = o.local_date.take();
        }
        Timing::BestTime => {
            let (s, e) = (ws.unwrap_or(DEFAULT_WINDOW.0), we.unwrap_or(DEFAULT_WINDOW.1));
            if e <= s || e - s < 60 {
                return Err(AppError::bad("the best-time window must end at least an hour after it starts"));
            }
            let within = o.within_hours.unwrap_or(DEFAULT_WITHIN_HOURS);
            if !(6..=72).contains(&within) {
                return Err(AppError::bad("send_options.within_hours must be between 6 and 72"));
            }
            clean.window_start = Some(hhmm(s));
            clean.window_end = Some(hhmm(e));
            clean.within_hours = Some(within);
        }
        Timing::Gradual => {
            let h = o.spread_hours.unwrap_or(2);
            if !(2..=72).contains(&h) {
                return Err(AppError::bad("send_options.spread_hours must be between 2 and 72"));
            }
            clean.spread_hours = Some(h);
        }
    }
    if o.smart_sending {
        let h = o.smart_sending_hours.unwrap_or(DEFAULT_SMART_HOURS);
        if !(1..=168).contains(&h) {
            return Err(AppError::bad("send_options.smart_sending_hours must be between 1 and 168"));
        }
        clean.smart_sending_hours = Some(h);
    }
    if clean == SendOptions::default() {
        return Ok(None);
    }
    Ok(Some(clean))
}

/// The options stored on a campaign row (None when unset or unreadable).
pub fn of(stored: Option<&Value>) -> Option<SendOptions> {
    stored.filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// One plain sentence describing the options, for dry runs and the dashboard.
pub fn describe(o: Option<&SendOptions>) -> String {
    let Some(o) = o else { return "Goes to everyone at once.".into() };
    let when = match o.timing {
        Timing::AllAtOnce => "Goes to everyone at once.".to_string(),
        Timing::LocalTime => format!(
            "Goes out at {} in each person's time zone; people for whom it's already past {} when sending starts get it {}.",
            o.local_time.as_deref().unwrap_or("10:00"),
            o.local_time.as_deref().unwrap_or("10:00"),
            if o.late == Late::Immediately { "right away" } else { "at that time the next day" }
        ),
        Timing::BestTime => {
            let (s, e) = o.window();
            format!("Goes out at each person's best hour between {} and {} their time, within {} hours of the send time.", hhmm(s), hhmm(e), o.within_hours())
        }
        Timing::Gradual => format!("Spread evenly over {} hours, one batch an hour.", o.spread_hours()),
    };
    match o.smart_hours() {
        Some(h) => format!("{when} Skips people who got a message on this channel in the last {h} hours."),
        None => when,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_and_normalizes() {
        assert_eq!(parse(&Value::Null).unwrap(), None);
        assert_eq!(parse(&json!({})).unwrap(), None, "all defaults is the same as no options");
        let o = parse(&json!({"timing": "local_time", "local_time": "9:30", "spread_hours": 5})).unwrap().unwrap();
        assert_eq!(o.local_time.as_deref(), Some("09:30"));
        assert_eq!(o.spread_hours, None, "other modes' settings are dropped");
        assert_eq!(o.late, Late::NextDay);
        let b = parse(&json!({"timing": "best_time"})).unwrap().unwrap();
        assert_eq!((b.window_start.as_deref(), b.window_end.as_deref(), b.within_hours), (Some("08:00"), Some("20:00"), Some(24)));
        assert!(parse(&json!({"timing": "best_time", "window_start": "20:00", "window_end": "08:00"})).is_err());
        assert!(parse(&json!({"timing": "gradual", "spread_hours": 1})).is_err());
        assert!(parse(&json!({"timing": "gradual", "spread_hours": 73})).is_err());
        assert!(parse(&json!({"timing": "local_time", "local_time": "25:00"})).is_err());
        assert!(parse(&json!({"timing": "soon"})).is_err());
        assert!(parse(&json!({"smart": true})).is_err(), "unknown keys are rejected");
        let s = parse(&json!({"smart_sending": true})).unwrap().unwrap();
        assert_eq!(s.smart_hours(), Some(16));
        assert!(parse(&json!({"smart_sending": true, "smart_sending_hours": 0})).is_err());
    }
}

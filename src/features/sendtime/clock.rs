//! Wall-clock arithmetic in a person's time zone, safe across daylight-saving changes.

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// Minutes after midnight, parsed from "HH:MM" (00:00-23:59).
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    if h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60)
}

/// The instant the clock in `tz` shows `minute` (after midnight) on `date`. A time skipped by a
/// spring-forward change moves to the first minute that exists after it (the end of the gap: 02:07
/// on a 02:00-03:00 gap is 03:00); a time that happens twice in a fall-back change is its first occurrence.
pub fn local_instant(tz: Tz, date: NaiveDate, minute: u32) -> DateTime<Utc> {
    let mut naive = date.and_time(NaiveTime::from_hms_opt(minute / 60 % 24, minute % 60, 0).unwrap_or(NaiveTime::MIN));
    // Gaps are at most a couple of hours; step forward a minute at a time until the clock exists.
    for _ in 0..(4 * 60) {
        match tz.from_local_datetime(&naive) {
            LocalResult::Single(t) => return t.with_timezone(&Utc),
            LocalResult::Ambiguous(a, b) => return a.min(b).with_timezone(&Utc),
            LocalResult::None => naive += Duration::minutes(1),
        }
    }
    Utc.from_utc_datetime(&naive)
}

/// The first instant at or after `from`, on `not_before` or later (local date), when the clock in
/// `tz` shows `minute`.
pub fn next_local(tz: Tz, from: DateTime<Utc>, not_before: Option<NaiveDate>, minute: u32) -> DateTime<Utc> {
    let today = from.with_timezone(&tz).date_naive();
    let mut day = not_before.map_or(today, |d| d.max(today));
    loop {
        let at = local_instant(tz, day, minute);
        if at >= from {
            return at;
        }
        day = day.succ_opt().unwrap_or(day);
        if day > today + Duration::days(3) {
            return from;
        }
    }
}

/// Minutes after local midnight at `at` in `tz`.
pub fn local_minute(tz: Tz, at: DateTime<Utc>) -> u32 {
    let l = at.with_timezone(&tz);
    l.hour() * 60 + l.minute()
}

/// When a message to someone in `tz` may next go out if it may only go out between `start` and `end`
/// (minutes after local midnight, start < end). None: it may go now.
pub fn window_wait(tz: Tz, now: DateTime<Utc>, start: u32, end: u32) -> Option<DateTime<Utc>> {
    let m = local_minute(tz, now);
    if (start..end).contains(&m) {
        return None;
    }
    let today = now.with_timezone(&tz).date_naive();
    let day = if m >= end { today.succ_opt().unwrap_or(today) } else { today };
    Some(local_instant(tz, day, start).max(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::{America, Asia};

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_hhmm("10:00"), Some(600));
        assert_eq!(parse_hhmm("9:30"), Some(570));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("10:5"), None);
        assert_eq!(parse_hhmm("ten"), None);
        assert_eq!(hhmm(570), "09:30");
    }

    #[test]
    fn half_and_quarter_hour_zones() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        // 10:00 in Kolkata (+5:30) is 04:30 UTC; in Kathmandu (+5:45) it's 04:15 UTC.
        assert_eq!(local_instant(Asia::Kolkata, d, 600), utc(2026, 11, 20, 4, 30));
        assert_eq!(local_instant(Asia::Kathmandu, d, 600), utc(2026, 11, 20, 4, 15));
    }

    #[test]
    fn daylight_saving_changes() {
        // US spring forward 2026-03-08: 02:30 doesn't exist in New York; it becomes 03:00 EDT (07:00 UTC).
        let spring = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        assert_eq!(local_instant(America::New_York, spring, 150), utc(2026, 3, 8, 7, 0));
        // Not on a quarter hour: 02:07 is the first valid minute after the gap, 03:00, not 03:07.
        assert_eq!(local_instant(America::New_York, spring, 127), utc(2026, 3, 8, 7, 0));
        assert_eq!(local_instant(America::New_York, spring, 179), utc(2026, 3, 8, 7, 0));
        // Lord Howe Island springs forward half an hour (02:00 -> 02:30, +10:30 -> +11 on 2026-10-04): 02:10 is 02:30 = 15:30 UTC.
        let lhi: Tz = "Australia/Lord_Howe".parse().unwrap();
        let lh = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        assert_eq!(local_instant(lhi, lh, 130), utc(2026, 10, 3, 15, 30));
        // 10:00 that day is EDT (14:00 UTC), the day before EST (15:00 UTC).
        assert_eq!(local_instant(America::New_York, spring, 600), utc(2026, 3, 8, 14, 0));
        assert_eq!(local_instant(America::New_York, spring.pred_opt().unwrap(), 600), utc(2026, 3, 7, 15, 0));
        // Fall back 2026-11-01: 01:30 happens twice; the first one (EDT, 05:30 UTC) counts.
        let fall = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert_eq!(local_instant(America::New_York, fall, 90), utc(2026, 11, 1, 5, 30));
        // next_local across the change: after 10:00 EDT on Oct 31, the next 10:00 is EST.
        assert_eq!(next_local(America::New_York, utc(2026, 10, 31, 15, 0), None, 600), utc(2026, 11, 1, 15, 0));
    }

    #[test]
    fn next_local_respects_the_floor_date() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        // Before the day: that day's 10:00. After it: the next day.
        assert_eq!(next_local(America::Chicago, utc(2026, 11, 19, 0, 0), Some(d), 600), utc(2026, 11, 20, 16, 0));
        assert_eq!(next_local(America::Chicago, utc(2026, 11, 20, 17, 0), Some(d), 600), utc(2026, 11, 21, 16, 0));
    }

    #[test]
    fn windows() {
        // 9:00-20:00 in Honolulu (UTC-10). 03:00 UTC = 17:00 HST: open.
        let hi: Tz = "Pacific/Honolulu".parse().unwrap();
        assert_eq!(window_wait(hi, utc(2026, 11, 14, 3, 0), 540, 1200), None);
        // 13:00 UTC = 03:00 HST: wait until 09:00 HST = 19:00 UTC the same day.
        assert_eq!(window_wait(hi, utc(2026, 11, 14, 13, 0), 540, 1200), Some(utc(2026, 11, 14, 19, 0)));
        // 07:00 UTC = 21:00 HST: wait until tomorrow 09:00 HST.
        assert_eq!(window_wait(hi, utc(2026, 11, 14, 7, 0), 540, 1200), Some(utc(2026, 11, 14, 19, 0)));
        // Kathmandu 20:10 local (14:25 UTC) with 9:00-20:00: wait for 09:00 tomorrow = 03:15 UTC.
        assert_eq!(window_wait(Asia::Kathmandu, utc(2026, 11, 14, 14, 25), 540, 1200), Some(utc(2026, 11, 15, 3, 15)));
    }
}

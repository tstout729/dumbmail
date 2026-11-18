//! Delay steps that wait until a time of day, an allowed weekday, or a date from the triggering
//! event or the profile.
//!
//! A delay with only `amount` and `unit` is untouched by this module: [`begin`] and [`wake`] return
//! `None` for it and the flow engine stores and schedules it exactly as before. A delay with `at`,
//! `weekdays` or `until` is worked out in the person's time zone (the send-time module's lookup:
//! their profile's zone or one derived from where they are, else the account's), and the instant it
//! ends is kept on the run (`_wait_until`, with `_wait_from`, when they reached the step) so a run
//! woken early keeps waiting and a step marked `recompute_zone` can be worked out again when it fires.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::features::sendtime::{clock, zone};
use crate::flows::Node;
use crate::models::{Account, Profile};

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
/// Longest offset an `until` may take (a year).
const MAX_OFFSET_SECS: f64 = 366.0 * 86400.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfPast {
    Continue,
    Exit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Until {
    /// `event` (the triggering event's properties) or `profile` (the profile's properties).
    pub event: bool,
    /// Path inside the properties, dot-separated (`check_in`, `booking.check_in`).
    pub path: String,
    /// Seconds added to the date (negative: before it).
    pub offset_secs: i64,
    pub if_past: IfPast,
}

/// The extra options on a delay step.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Local time of day, minutes after midnight.
    pub at: Option<u32>,
    /// Allowed days, Monday = bit 0.
    pub weekdays: Option<u8>,
    pub until: Option<Until>,
    pub recompute_zone: bool,
}

/// What a delay decided when the person reached it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wait {
    Until(DateTime<Utc>),
    /// The time is already past: go on to the next step now.
    Continue,
    /// The time is already past and the step says to leave the flow.
    Exit,
}

fn present<'a>(n: &'a Node, k: &str) -> Option<&'a Value> {
    n.data.get(k).filter(|v| !v.is_null())
}

fn unit_secs(unit: Option<&str>) -> Option<f64> {
    match unit {
        Some("minutes") => Some(60.0),
        Some("hours") => Some(3600.0),
        Some("days") => Some(86400.0),
        _ => None,
    }
}

/// Read a delay's options. `Ok(None)`: a plain delay (amount and unit only).
pub fn parse(n: &Node) -> AppResult<Option<Plan>> {
    let (at, weekdays, until) = (present(n, "at"), present(n, "weekdays"), present(n, "until"));
    if at.is_none() && weekdays.is_none() && until.is_none() {
        return Ok(None);
    }
    let bad = |m: &str| AppError::bad(format!("delay {}: {m}", n.id));
    let at = match at {
        Some(v) => Some(v.as_str().and_then(clock::parse_hhmm).ok_or_else(|| bad("`at` must be a time of day like \"10:00\""))?),
        None => None,
    };
    let weekdays = match weekdays {
        Some(v) => {
            let list = v.as_array().ok_or_else(|| bad("`weekdays` must be a list like [\"tue\", \"wed\", \"thu\"]"))?;
            let mut mask = 0u8;
            for d in list {
                let i = d.as_str().and_then(|s| DAYS.iter().position(|x| x.eq_ignore_ascii_case(s.trim())));
                mask |= 1 << i.ok_or_else(|| bad("weekdays are mon, tue, wed, thu, fri, sat and sun"))?;
            }
            if mask == 0 {
                return Err(bad("choose at least one weekday"));
            }
            Some(mask)
        }
        None => None,
    };
    let until = match until {
        Some(u) => {
            let field = u.get("field").and_then(Value::as_str).map(str::trim).unwrap_or("");
            let (event, path) = if let Some(p) = field.strip_prefix("event.properties.").or_else(|| field.strip_prefix("event.")) {
                (true, p)
            } else if let Some(p) = field.strip_prefix("profile.properties.").or_else(|| field.strip_prefix("profile.")).or_else(|| field.strip_prefix("person.")) {
                (false, p)
            } else {
                return Err(bad("`until.field` must start with event. (the triggering event's properties) or profile. (a profile property), like event.check_in"));
            };
            if path.is_empty() || path.len() > 100 || path.split('.').any(|s| s.is_empty()) {
                return Err(bad("`until.field` needs a property name after event. or profile."));
            }
            let amount = match u.get("offset_amount") {
                None | Some(Value::Null) => 0.0,
                Some(v) => v.as_f64().filter(|a| *a >= 0.0).ok_or_else(|| bad("`until.offset_amount` must be zero or more"))?,
            };
            let unit = u.get("offset_unit").and_then(Value::as_str).unwrap_or("days");
            let secs = amount * unit_secs(Some(unit)).ok_or_else(|| bad("`until.offset_unit` must be minutes, hours or days"))?;
            if secs > MAX_OFFSET_SECS {
                return Err(bad("an `until` offset can be at most a year"));
            }
            let before = match u.get("offset_direction").and_then(Value::as_str).unwrap_or("after") {
                "before" => true,
                "after" => false,
                _ => return Err(bad("`until.offset_direction` must be before or after")),
            };
            let if_past = match u.get("if_past").and_then(Value::as_str) {
                Some("continue") => IfPast::Continue,
                Some("exit") => IfPast::Exit,
                None if before => IfPast::Exit,
                None => IfPast::Continue,
                Some(_) => return Err(bad("`until.if_past` must be continue or exit")),
            };
            Some(Until { event, path: path.to_string(), offset_secs: if before { -(secs as i64) } else { secs as i64 }, if_past })
        }
        None => None,
    };
    let recompute_zone = match present(n, "recompute_zone") {
        Some(v) => v.as_bool().ok_or_else(|| bad("`recompute_zone` must be true or false"))?,
        None => false,
    };
    Ok(Some(Plan { at, weekdays, until, recompute_zone }))
}

/// A date or date-time from a property, in `tz` when it has no zone of its own. A date alone is the
/// start of that day.
pub fn parse_when(v: &Value, tz: Tz) -> Option<DateTime<Utc>> {
    let s = match v {
        Value::String(s) => s.trim(),
        // Unix seconds (or milliseconds, for numbers that big).
        Value::Number(n) => {
            let x = n.as_f64()?;
            let secs = if x.abs() > 1e11 { x / 1000.0 } else { x };
            return DateTime::from_timestamp(secs as i64, 0);
        }
        _ => return None,
    };
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return Some(clock::local_instant(tz, n.date(), (n.time().num_seconds_from_midnight() / 60) as u32));
        }
    }
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().or_else(|| NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok())?;
    Some(clock::local_instant(tz, d, 0))
}

fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(root, |v, k| v.get(k))
}

/// The moment a delay with `plan` ends, for someone in `tz` who reached it at `from`. `None`: an
/// `until` whose date is missing or unreadable.
pub fn end(plan: &Plan, amount_secs: f64, from: DateTime<Utc>, tz: Tz, context: &Value, profile_props: &Value) -> Option<DateTime<Utc>> {
    let base = match &plan.until {
        Some(u) => {
            let root = if u.event { context.get("event").and_then(|e| e.get("properties")).unwrap_or(&Value::Null) } else { profile_props };
            parse_when(lookup(root, &u.path)?, tz)? + Duration::seconds(u.offset_secs)
        }
        None => from + Duration::milliseconds((amount_secs * 1000.0) as i64),
    };
    Some(align(plan, base, tz))
}

/// Move `base` forward to the time of day and allowed weekday a plan asks for.
pub fn align(plan: &Plan, base: DateTime<Utc>, tz: Tz) -> DateTime<Utc> {
    let mut t = base;
    if let Some(at) = plan.at {
        let day = t.with_timezone(&tz).date_naive();
        t = clock::local_instant(tz, day, at);
        if t < base {
            t = clock::local_instant(tz, day + Duration::days(1), at);
        }
    }
    if let Some(mask) = plan.weekdays {
        // Same local time of day on the next allowed day (`at` when set).
        let minute = plan.at.unwrap_or_else(|| clock::local_minute(tz, t));
        for _ in 0..7 {
            let local = t.with_timezone(&tz);
            if mask & (1 << local.weekday().num_days_from_monday()) != 0 {
                break;
            }
            t = clock::local_instant(tz, local.date_naive() + Duration::days(1), minute);
        }
    }
    t
}

fn amount_secs(n: &Node) -> f64 {
    n.data.get("amount").and_then(Value::as_f64).unwrap_or(0.0) * unit_secs(n.data.get("unit").and_then(Value::as_str)).unwrap_or(86400.0)
}

/// The person's time zone for flows (the send-time module's lookup).
pub fn person_zone(account: &Account, person: &Profile) -> Tz {
    zone::resolve(person, crate::sending::account_tz(account)).0
}

/// Decide a delay at the moment the person reaches it, from `context`. Plain delays: `None`
/// (nothing is touched). Otherwise the wait, with its end saved in `context`.
pub fn decide(plan: &Plan, node: &Node, from: DateTime<Utc>, now: DateTime<Utc>, tz: Tz, context: &mut Value, profile_props: &Value) -> Wait {
    match end(plan, amount_secs(node), from, tz, context, profile_props) {
        Some(t) if t > now => {
            context["_wait_until"] = json!(t);
            context["_wait_from"] = json!(from);
            Wait::Until(t)
        }
        // Past (or no usable date): an `until` follows its rule; other options can't be past.
        _ => match plan.until.as_ref().map(|u| u.if_past) {
            Some(IfPast::Exit) => Wait::Exit,
            _ => Wait::Continue,
        },
    }
}

/// The flow engine reached a delay step. `None`: a plain delay, handled as always.
pub fn begin(account: &Account, person: &Profile, node: &Node, context: &mut Value, now: DateTime<Utc>) -> Option<Wait> {
    // A definition that was saved passed validation; one that can't be read waits like a plain delay.
    let plan = parse(node).ok()??;
    Some(decide(&plan, node, now, now, person_zone(account, person), context, &person.properties))
}

fn saved_time(context: &Value, k: &str) -> Option<DateTime<Utc>> {
    context.get(k).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The run woke at a delay it was waiting on. `Some(t)`: keep waiting until `t` (saved in
/// `context`). `None`: the wait is over (its saved times are cleared); plain delays always end here.
pub fn wake(account: &Account, person: &Profile, node: &Node, context: &mut Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let until = saved_time(context, "_wait_until")?;
    let mut target = until;
    if let Ok(Some(plan)) = parse(node) {
        if plan.recompute_zone {
            let from = saved_time(context, "_wait_from").unwrap_or(now);
            if let Some(t) = end(&plan, amount_secs(node), from, person_zone(account, person), context, &person.properties) {
                target = t;
            }
        }
    }
    // Woken early (a second early is close enough).
    if target > now + Duration::seconds(1) {
        context["_wait_until"] = json!(target);
        return Some(target);
    }
    if let Some(o) = context.as_object_mut() {
        o.remove("_wait_until");
        o.remove("_wait_from");
    }
    None
}

/// Check a delay's options when a flow is saved.
pub fn validate(n: &Node) -> AppResult<()> {
    parse(n).map(|_| ())
}

/// Weekday names, for docs and errors.
pub fn weekday_names() -> &'static [&'static str] {
    &DAYS
}

/// A plan's `until` reads the triggering event.
pub fn reads_event(n: &Node) -> bool {
    matches!(parse(n), Ok(Some(Plan { until: Some(Until { event: true, .. }), .. })))
}

/// For back-populate: the time from reaching a delay to its end, if it can be known without a
/// person (a plain delay, or one with only `at`/`weekdays`, which only push it later). `None` for
/// `until` delays.
pub fn fixed_secs(n: &Node) -> Option<f64> {
    match parse(n) {
        Ok(None) => Some(amount_secs(n)),
        Ok(Some(p)) if p.until.is_none() => Some(amount_secs(n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::America::{Los_Angeles, New_York};

    fn node(v: Value) -> Node {
        let mut v = v;
        v["id"] = json!("d");
        v["type"] = json!("delay");
        serde_json::from_value(v).unwrap()
    }

    fn la(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Los_Angeles.with_ymd_and_hms(y, m, d, h, min, 0).earliest().unwrap().with_timezone(&Utc)
    }

    #[test]
    fn plain_delays_are_not_touched() {
        let n = node(json!({"amount": 2, "unit": "days"}));
        assert_eq!(parse(&n).unwrap(), None);
        let n = node(json!({"amount": 2, "unit": "days", "at": null, "weekdays": null, "until": null}));
        assert_eq!(parse(&n).unwrap(), None);
    }

    #[test]
    fn at_a_time_of_day() {
        let plan = parse(&node(json!({"amount": 2, "unit": "days", "at": "10:00"}))).unwrap().unwrap();
        // Reached Monday 08:00: two days later is Wednesday 08:00, before 10:00 that day.
        let from = la(2026, 11, 16, 8, 0);
        assert_eq!(end(&plan, 2.0 * 86400.0, from, Los_Angeles, &json!({}), &json!({})), Some(la(2026, 11, 18, 10, 0)));
        // Reached Monday 11:00: Wednesday 11:00 is past 10:00, so Thursday 10:00.
        let from = la(2026, 11, 16, 11, 0);
        assert_eq!(end(&plan, 2.0 * 86400.0, from, Los_Angeles, &json!({}), &json!({})), Some(la(2026, 11, 19, 10, 0)));
    }

    #[test]
    fn only_on_allowed_weekdays() {
        let plan = parse(&node(json!({"amount": 1, "unit": "days", "weekdays": ["tue", "wed", "thu"], "at": "09:30"}))).unwrap().unwrap();
        // Reached Thursday 2026-11-19 12:00: Friday 09:30 isn't allowed; next Tuesday 09:30.
        let from = la(2026, 11, 19, 12, 0);
        assert_eq!(end(&plan, 86400.0, from, Los_Angeles, &json!({}), &json!({})), Some(la(2026, 11, 24, 9, 30)));
        // Without `at` the time of day is kept.
        let plan = parse(&node(json!({"amount": 1, "unit": "days", "weekdays": ["mon"]}))).unwrap().unwrap();
        assert_eq!(end(&plan, 86400.0, la(2026, 11, 19, 15, 45), Los_Angeles, &json!({}), &json!({})), Some(la(2026, 11, 23, 15, 45)));
    }

    #[test]
    fn a_time_skipped_by_daylight_saving_moves_forward() {
        // 2027-03-14: Los Angeles skips 02:00-03:00. "at 02:30" that day is 03:00.
        let plan = parse(&node(json!({"amount": 1, "unit": "days", "at": "02:30"}))).unwrap().unwrap();
        let from = la(2027, 3, 13, 1, 0);
        let t = end(&plan, 86400.0, from, Los_Angeles, &json!({}), &json!({})).unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2027, 3, 14, 10, 0, 0).unwrap(), "03:00 PDT");
    }

    #[test]
    fn a_time_that_happens_twice_uses_the_first() {
        // 2026-11-01: Los Angeles repeats 01:00-02:00. "at 01:30" is the first one (PDT, 08:30 UTC).
        let plan = parse(&node(json!({"amount": 1, "unit": "days", "at": "01:30"}))).unwrap().unwrap();
        let from = Utc.with_ymd_and_hms(2026, 10, 31, 7, 0, 0).unwrap(); // Oct 31 00:00 PDT
        let t = end(&plan, 86400.0, from, Los_Angeles, &json!({}), &json!({})).unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 11, 1, 8, 30, 0).unwrap());
    }

    #[test]
    fn until_a_date_in_the_event() {
        let plan = parse(&node(json!({"amount": 0, "unit": "days", "until": {"field": "event.check_in", "offset_amount": 2, "offset_unit": "days", "offset_direction": "before"}, "at": "10:00"})))
            .unwrap()
            .unwrap();
        assert_eq!(plan.until.as_ref().unwrap().if_past, IfPast::Exit, "before offsets exit by default");
        let ctx = json!({"event": {"properties": {"check_in": "2026-12-20"}}});
        let t = end(&plan, 0.0, la(2026, 11, 17, 9, 0), Los_Angeles, &ctx, &json!({})).unwrap();
        assert_eq!(t, la(2026, 12, 18, 10, 0));
        // In New York the same date is 3 hours earlier.
        let t = end(&plan, 0.0, la(2026, 11, 17, 9, 0), New_York, &ctx, &json!({})).unwrap();
        assert_eq!(t, New_York.with_ymd_and_hms(2026, 12, 18, 10, 0, 0).unwrap().with_timezone(&Utc));
        // A profile property, after the date.
        let plan = parse(&node(json!({"amount": 0, "unit": "days", "until": {"field": "profile.renewal", "offset_amount": 3, "offset_unit": "hours"}}))).unwrap().unwrap();
        assert_eq!(plan.until.as_ref().unwrap().if_past, IfPast::Continue, "after offsets continue by default");
        let t = end(&plan, 0.0, la(2026, 11, 17, 9, 0), Los_Angeles, &json!({}), &json!({"renewal": "2026-12-01T12:00:00Z"})).unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 12, 1, 15, 0, 0).unwrap());
    }

    #[test]
    fn past_and_missing_dates_follow_the_step() {
        let exit = parse(&node(json!({"amount": 0, "unit": "days", "until": {"field": "event.check_in", "offset_amount": 1, "offset_direction": "before"}}))).unwrap().unwrap();
        let cont = parse(&node(json!({"amount": 0, "unit": "days", "until": {"field": "event.check_in", "if_past": "continue", "offset_direction": "before"}}))).unwrap().unwrap();
        let n = node(json!({"amount": 0, "unit": "days"}));
        let now = la(2026, 11, 17, 9, 0);
        for ctx in [json!({"event": {"properties": {"check_in": "2026-11-10"}}}), json!({"event": {"properties": {}}}), json!({})] {
            let mut c = ctx.clone();
            assert_eq!(decide(&exit, &n, now, now, Los_Angeles, &mut c, &json!({})), Wait::Exit, "{ctx}");
            assert_eq!(decide(&cont, &n, now, now, Los_Angeles, &mut c, &json!({})), Wait::Continue, "{ctx}");
            assert!(c.get("_wait_until").is_none());
        }
    }

    #[test]
    fn rejects_bad_options() {
        for bad in [
            json!({"at": "25:00"}),
            json!({"at": 10}),
            json!({"weekdays": []}),
            json!({"weekdays": ["funday"]}),
            json!({"until": {"field": "check_in"}}),
            json!({"until": {"field": "event."}}),
            json!({"until": {"field": "event.x", "offset_unit": "weeks"}}),
            json!({"until": {"field": "event.x", "offset_amount": -1}}),
            json!({"until": {"field": "event.x", "offset_amount": 400, "offset_unit": "days"}}),
            json!({"until": {"field": "event.x", "if_past": "maybe"}}),
            json!({"at": "10:00", "recompute_zone": "yes"}),
        ] {
            let mut v = bad.clone();
            v["amount"] = json!(1);
            v["unit"] = json!("days");
            assert!(parse(&node(v)).is_err(), "{bad}");
        }
    }
}

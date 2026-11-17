//! Planning a campaign send: each queued message gets the instant it's released (`send_after`) and
//! the time zone that was planned in (`send_tz`). Batches are then queued per release time, so a
//! zone's recipients go out when their clock reaches the hour. Planning only touches messages
//! without a release time, so a retried or restarted send never moves anyone or plans them twice.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use super::clock::{local_instant, next_local, window_wait};
use super::options::{self, Late, SendOptions, Timing};
use super::zone;
use crate::campaigns::Audience;
use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow};
use crate::state::AppState;

/// Messages planned per database round trip.
const PAGE: i64 = 1000;
/// Recipients sampled to estimate when a send goes out.
const SAMPLE: i64 = 2000;
/// Engagement events needed before a person's own history picks their hour.
const MIN_PERSONAL: usize = 3;
/// Engagement events needed before the account's history picks the hour for people without their own.
const MIN_ACCOUNT: f64 = 30.0;
/// Hour used when neither the person nor the account has enough history.
const DEFAULT_HOUR: u32 = 10;
/// Engagement older than this doesn't count; newer engagement counts more (half-life in days).
const HISTORY_DAYS: i32 = 180;
const HALF_LIFE_DAYS: f64 = 30.0;

/// Opens and clicks by hour of the week (Monday 00:00 = 0), weighted toward recent ones.
#[derive(Debug, Clone)]
pub struct Histogram {
    w: Vec<f64>,
    events: f64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self { w: vec![0.0; 168], events: 0.0 }
    }
}

impl Histogram {
    fn add(&mut self, weekday: u32, hour: u32, weight: f64) {
        if let Some(c) = self.w.get_mut((weekday * 24 + hour) as usize) {
            *c += weight;
            self.events += 1.0;
        }
    }

    /// A person's histogram in their zone. Clicks are the stronger signal (Apple Mail privacy
    /// protection opens many emails automatically), so with enough clicks only clicks count;
    /// otherwise opens count once and clicks twice.
    pub fn personal(tz: Tz, now: DateTime<Utc>, opens: &[DateTime<Utc>], clicks: &[DateTime<Utc>]) -> Self {
        let mut h = Histogram::default();
        let decay = |t: &DateTime<Utc>| 0.5f64.powf((now - *t).num_minutes().max(0) as f64 / 1440.0 / HALF_LIFE_DAYS);
        let mut put = |t: &DateTime<Utc>, w: f64| {
            let l = t.with_timezone(&tz);
            h.add(l.weekday().num_days_from_monday(), l.hour(), w * decay(t));
        };
        if clicks.len() >= MIN_PERSONAL {
            clicks.iter().for_each(|t| put(t, 1.0));
        } else {
            opens.iter().for_each(|t| put(t, 1.0));
            clicks.iter().for_each(|t| put(t, 2.0));
        }
        h
    }

    fn enough(&self, min: f64) -> bool {
        self.events >= min
    }

    /// Score of a local (weekday, hour): engagement at that hour of the week and, with less weight,
    /// at that hour on other days, each smoothed over nearby hours (halving per hour away), so a thin
    /// history still points somewhere sensible and the hour nearest the person's habit wins.
    fn score(&self, weekday: u32, hour: u32) -> f64 {
        let at = |d: u32, h: u32| self.w[((d % 7) * 24 + (h % 24)) as usize];
        (0..24u32)
            .map(|h| {
                let dist = (h as i32 - hour as i32).unsigned_abs().min(24 - (h as i32 - hour as i32).unsigned_abs());
                let day_avg: f64 = (0..7).map(|d| at(d, h)).sum::<f64>() / 7.0;
                0.5f64.powi(dist as i32) * (at(weekday, h) + 0.3 * day_avg)
            })
            .sum()
    }
}

/// Everything about one send that's the same for every recipient.
pub struct Plan {
    pub opts: SendOptions,
    pub start: DateTime<Utc>,
    pub account_tz: Tz,
    /// local_time: the day it goes out.
    pub local_date: NaiveDate,
    /// SMS: the hours a text may go out in each person's zone.
    pub sms_window: Option<(u32, u32)>,
    pub account_hist: Histogram,
}

impl Plan {
    /// When this recipient's message is released. `index`/`total` place them in a gradual send.
    pub fn release(&self, tz: Tz, index: i64, total: i64, personal: Option<&Histogram>) -> DateTime<Utc> {
        let at = match self.opts.timing {
            Timing::AllAtOnce => self.start,
            Timing::LocalTime => {
                let minute = self.opts.local_minute();
                match self.opts.late {
                    Late::NextDay => next_local(tz, self.start, Some(self.local_date), minute),
                    Late::Immediately => local_instant(tz, self.local_date, minute).max(self.start),
                }
            }
            Timing::Gradual => {
                let hours = self.opts.spread_hours().max(1);
                self.start + Duration::hours(index.max(0) * hours / total.max(1))
            }
            Timing::BestTime => self.best(tz, personal),
        };
        // Texts never go out during the recipient's quiet hours.
        match self.sms_window {
            Some((s, e)) => window_wait(tz, at, s, e).unwrap_or(at),
            None => at,
        }
    }

    fn best(&self, tz: Tz, personal: Option<&Histogram>) -> DateTime<Utc> {
        let (mut ws, mut we) = self.opts.window();
        if let Some((s, e)) = self.sms_window {
            if ws.max(s) < we.min(e) {
                (ws, we) = (ws.max(s), we.min(e));
            } else {
                (ws, we) = (s, e);
            }
        }
        let end = self.start + Duration::hours(self.opts.within_hours());
        let hist = personal.filter(|h| h.enough(MIN_PERSONAL as f64)).or(Some(&self.account_hist).filter(|h| h.enough(MIN_ACCOUNT)));
        let mut best: Option<(f64, DateTime<Utc>)> = None;
        let first = self.start.with_timezone(&tz).date_naive();
        for d in 0..=((self.opts.within_hours() / 24) + 1) {
            let day = first + Duration::days(d);
            for hour in 0..24u32 {
                // A whole hour inside the window, starting on the hour.
                if hour * 60 < ws || hour * 60 + 60 > we.max(ws + 60) {
                    continue;
                }
                let at = local_instant(tz, day, hour * 60);
                if at < self.start || at >= end || at.with_timezone(&tz).hour() != hour {
                    continue;
                }
                let score = match hist {
                    Some(h) => h.score(day.weekday().num_days_from_monday(), hour),
                    None => -((hour as f64) - DEFAULT_HOUR as f64).abs(),
                };
                if best.is_none_or(|(s, _)| score > s + 1e-9) {
                    best = Some((score, at));
                }
            }
        }
        match best {
            Some((_, at)) => at,
            // The window doesn't open before the look-ahead ends: its next opening.
            None => next_local(tz, self.start, None, ws),
        }
    }
}

/// The account's opens and clicks by hour of the week in its own zone, used as each person's local
/// hours for people without enough history of their own.
pub async fn account_histogram(db: &PgPool, account_id: Uuid, tz: Tz) -> sqlx::Result<Histogram> {
    let rows: Vec<(i32, i32, f64, i64)> = sqlx::query_as(
        "SELECT (extract(isodow FROM t AT TIME ZONE $2)::int - 1), extract(hour FROM t AT TIME ZONE $2)::int,
                sum(power(0.5, extract(epoch FROM now() - t) / 86400.0 / $4))::float8, count(*)
         FROM (SELECT COALESCE(clicked_at, opened_at) AS t FROM messages
               WHERE account_id = $1 AND NOT is_test AND NOT transactional AND COALESCE(clicked_at, opened_at) > now() - make_interval(days => $3)) x
         GROUP BY 1, 2",
    )
    .bind(account_id)
    .bind(tz.name())
    .bind(90)
    .bind(HALF_LIFE_DAYS)
    .fetch_all(db)
    .await?;
    let mut h = Histogram::default();
    for (d, hr, w, n) in rows {
        if let Some(c) = h.w.get_mut((d.clamp(0, 6) * 24 + hr.clamp(0, 23)) as usize) {
            *c += w;
            h.events += n as f64;
        }
    }
    Ok(h)
}

/// Each listed person's own histogram (only people with any engagement are returned).
async fn personal_histograms(db: &PgPool, people: &[(Uuid, Tz)], now: DateTime<Utc>) -> sqlx::Result<HashMap<Uuid, Histogram>> {
    let ids: Vec<Uuid> = people.iter().map(|(id, _)| *id).collect();
    let rows: Vec<(Uuid, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT profile_id, opened_at, clicked_at FROM messages
         WHERE profile_id = ANY($1) AND NOT is_test AND NOT transactional AND (opened_at IS NOT NULL OR clicked_at IS NOT NULL)
           AND created_at > now() - make_interval(days => $2)
         ORDER BY created_at DESC LIMIT 200000",
    )
    .bind(&ids)
    .bind(HISTORY_DAYS)
    .fetch_all(db)
    .await?;
    let mut by: HashMap<Uuid, (Vec<DateTime<Utc>>, Vec<DateTime<Utc>>)> = HashMap::new();
    for (pid, o, c) in rows {
        let e = by.entry(pid).or_default();
        // An email opened by its click counts once, as the click.
        match (o, c) {
            (_, Some(c)) => e.1.push(c),
            (Some(o), None) => e.0.push(o),
            _ => {}
        }
    }
    let tz: HashMap<Uuid, Tz> = people.iter().copied().collect();
    Ok(by.into_iter().map(|(pid, (o, c))| (pid, Histogram::personal(tz[&pid], now, &o, &c))).collect())
}

async fn build_plan(state: &AppState, account: &Account, channel: &str, opts: SendOptions, start: DateTime<Utc>) -> AppResult<Plan> {
    let account_tz = crate::sending::account_tz(account);
    let local_date = opts.local_date.unwrap_or_else(|| start.with_timezone(&account_tz).date_naive());
    let sms_window = if channel == "sms" { Some(super::settings::sms_window(&state.db, account.id).await?) } else { None };
    let account_hist = if opts.timing == Timing::BestTime { account_histogram(&state.db, account.id, account_tz).await? } else { Histogram::default() };
    Ok(Plan { opts, start, account_tz, local_date, sms_window, account_hist })
}

/// Job step: give every queued, unplanned message of the campaign its release time. Returns the
/// number planned. Safe to run again: planned messages are left alone.
pub async fn plan_campaign(state: &AppState, c: &CampaignRow) -> anyhow::Result<u64> {
    let Some(opts) = options::of(c.send_options.as_ref()).filter(SendOptions::plans) else { return Ok(0) };
    let account = crate::campaigns::load_account(&state.db, c.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let plan = build_plan(state, &account, &c.channel, opts, Utc::now()).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1 AND status = 'queued' AND send_after IS NULL AND NOT is_test")
        .bind(c.id)
        .fetch_one(&state.db)
        .await?;
    let mut index = 0i64;
    let mut last = Uuid::nil();
    let mut planned = 0u64;
    loop {
        let rows: Vec<(Uuid, Uuid, Value, Option<String>)> = sqlx::query_as(
            "SELECT m.id, m.profile_id, p.properties, p.phone_number FROM messages m JOIN profiles p ON p.id = m.profile_id
             WHERE m.campaign_id = $1 AND m.status = 'queued' AND m.send_after IS NULL AND NOT m.is_test AND m.id > $2
             ORDER BY m.id LIMIT $3",
        )
        .bind(c.id)
        .bind(last)
        .bind(PAGE)
        .fetch_all(&state.db)
        .await?;
        let Some(tail) = rows.last() else { break };
        last = tail.0;
        let zones: Vec<(Uuid, Tz)> = rows.iter().map(|(_, pid, props, phone)| (*pid, zone::resolve_parts(props, phone.as_deref(), plan.account_tz).0)).collect();
        let hists = if plan.opts.timing == Timing::BestTime { personal_histograms(&state.db, &zones, plan.start).await? } else { HashMap::new() };
        let (mut ids, mut ats, mut tzs) = (Vec::with_capacity(rows.len()), Vec::with_capacity(rows.len()), Vec::with_capacity(rows.len()));
        for ((mid, _, _, _), (pid, tz)) in rows.iter().zip(&zones) {
            ids.push(*mid);
            ats.push(plan.release(*tz, index, total, hists.get(pid)));
            tzs.push(tz.name().to_string());
            index += 1;
        }
        planned += sqlx::query(
            "UPDATE messages m SET send_after = x.at, send_tz = x.tz FROM unnest($1::uuid[], $2::timestamptz[], $3::text[]) AS x(id, at, tz)
             WHERE m.id = x.id AND m.send_after IS NULL AND m.status = 'queued'",
        )
        .bind(&ids)
        .bind(&ats)
        .bind(&tzs)
        .execute(&state.db)
        .await?
        .rows_affected();
    }
    Ok(planned)
}

/// When a campaign scheduled with these options should start. For local_time this is when the first
/// time zone reaches the hour on the chosen day (the day of `send_at` in the account's zone, or today),
/// and the day is recorded in the options. Returns the start (None: now) and the options to store.
pub async fn launch_start(state: &AppState, account: &Account, c: &CampaignRow, send_at: Option<DateTime<Utc>>) -> AppResult<Option<DateTime<Utc>>> {
    let Some(mut opts) = options::of(c.send_options.as_ref()) else { return Ok(send_at) };
    if c.channel == "sms" {
        let (s, e) = super::settings::sms_window(&state.db, account.id).await?;
        if opts.timing == Timing::LocalTime && !(s..e).contains(&opts.local_minute()) {
            return Err(AppError::bad(format!(
                "Texts only go out between {} and {} in each person's time zone; pick a local time in that window",
                super::clock::hhmm(s),
                super::clock::hhmm(e)
            )));
        }
    }
    if opts.timing != Timing::LocalTime {
        return Ok(send_at);
    }
    let tz = crate::sending::account_tz(account);
    let now = Utc::now();
    let today = now.with_timezone(&tz).date_naive();
    let day = send_at.map_or(today, |t| t.with_timezone(&tz).date_naive());
    if day < today {
        return Err(AppError::bad("pick today or a later day"));
    }
    opts.local_date = Some(day);
    sqlx::query("UPDATE campaigns SET send_options = $2 WHERE id = $1").bind(c.id).bind(serde_json::to_value(&opts)?).execute(&state.db).await?;
    // The first place on Earth to reach the hour is UTC+14.
    let earliest = local_instant(chrono_tz::Pacific::Kiritimati, day, opts.local_minute());
    Ok((earliest > now + Duration::seconds(30)).then_some(earliest))
}

/// Estimate when a send goes out: release times for a random sample of the audience, scaled to its
/// size, in 15-minute buckets. Also counts who smart sending would skip right now.
pub async fn preview(state: &AppState, account_id: Uuid, channel: &str, audience: &Audience, opts: Option<SendOptions>, send_at: Option<DateTime<Utc>>) -> AppResult<Value> {
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let tz = crate::sending::account_tz(&account);
    let total = crate::campaigns::estimate(&state.db, account_id, channel, audience).await?;
    let now = Utc::now();
    let start = send_at.filter(|t| *t > now).unwrap_or(now);
    let skipped = match opts.as_ref().and_then(SendOptions::smart_hours) {
        Some(h) => {
            let mut qb = QueryBuilder::new("SELECT count(*)");
            crate::campaigns::append_audience(&mut qb, &state.db, account_id, channel, audience).await?;
            qb.push(" AND ");
            push_recently_messaged(&mut qb, channel, h);
            Some(qb.build_query_scalar::<i64>().fetch_one(&state.db).await?)
        }
        None => None,
    };
    let description = options::describe(opts.as_ref());
    let Some(mut opts) = opts.filter(SendOptions::plans) else {
        let buckets = if total > 0 { vec![json!({"at": start, "count": total, "zones": []})] } else { vec![] };
        return Ok(json!({"total": total, "starts_at": start, "ends_at": start, "buckets": buckets, "smart_sending_skips": skipped, "description": description, "sampled": 0}));
    };
    // The day: the one recorded when it was scheduled, else the day of `send_at` (or today).
    if opts.timing == Timing::LocalTime && opts.local_date.is_none() {
        opts.local_date = Some(send_at.unwrap_or(now).with_timezone(&tz).date_naive());
    }
    let local_start = if opts.timing == Timing::LocalTime {
        local_instant(chrono_tz::Pacific::Kiritimati, opts.local_date.unwrap_or(now.date_naive()), opts.local_minute()).max(now)
    } else {
        start
    };
    let plan = build_plan(state, &account, channel, opts, local_start).await?;
    let mut qb = QueryBuilder::new("SELECT p.id, p.properties, p.phone_number");
    crate::campaigns::append_audience(&mut qb, &state.db, account_id, channel, audience).await?;
    qb.push(" ORDER BY random() LIMIT ").push_bind(SAMPLE);
    let rows: Vec<(Uuid, Value, Option<String>)> = qb.build_query_as().fetch_all(&state.db).await?;
    let zones: Vec<(Uuid, Tz)> = rows.iter().map(|(id, props, phone)| (*id, zone::resolve_parts(props, phone.as_deref(), tz).0)).collect();
    let hists = if plan.opts.timing == Timing::BestTime { personal_histograms(&state.db, &zones, plan.start).await? } else { HashMap::new() };
    let n = rows.len() as i64;
    let mut buckets: BTreeMap<DateTime<Utc>, (i64, BTreeMap<String, i64>)> = BTreeMap::new();
    for (i, (pid, ptz)) in zones.iter().enumerate() {
        // Spread the sample's positions over the whole audience for a gradual send.
        let at = plan.release(*ptz, i as i64 * total.max(1) / n.max(1), total, hists.get(pid));
        let key = at - Duration::minutes(i64::from(at.minute() % 15)) - Duration::seconds(i64::from(at.second()));
        let e = buckets.entry(key).or_default();
        e.0 += 1;
        *e.1.entry(ptz.name().to_string()).or_default() += 1;
    }
    let scale = |k: i64| if n == 0 { 0 } else { ((k as f64) * (total as f64) / (n as f64)).round() as i64 };
    let out: Vec<Value> = buckets
        .iter()
        .map(|(at, (k, zs))| {
            let mut zs: Vec<(&String, &i64)> = zs.iter().collect();
            zs.sort_by(|a, b| b.1.cmp(a.1));
            json!({"at": at, "count": scale(*k), "zones": zs.iter().take(3).map(|(z, _)| z).collect::<Vec<_>>()})
        })
        .collect();
    Ok(json!({
        "total": total,
        "starts_at": buckets.keys().next().copied().unwrap_or(start),
        "ends_at": buckets.keys().next_back().copied().unwrap_or(start),
        "buckets": out,
        "smart_sending_skips": skipped,
        "description": description,
        "sampled": n,
    }))
}

/// Appends a condition true when `p` got a marketing message on `channel` in the last `hours`.
pub fn push_recently_messaged(qb: &mut QueryBuilder<'_, sqlx::Postgres>, channel: &str, hours: i64) {
    qb.push("EXISTS (SELECT 1 FROM messages rm WHERE rm.profile_id = p.id AND rm.channel = ")
        .push_bind(channel.to_string())
        .push(" AND rm.status IN ('sent','delivered') AND NOT rm.is_test AND NOT rm.transactional AND rm.sent_at > now() - make_interval(hours => ")
        .push_bind(hours as i32)
        .push("))");
}

/// Smart sending: did this person get a marketing message on this channel in the last `hours`?
/// Transactional mail and tests don't count. Used by campaigns and flows.
pub async fn recently_messaged(db: &PgPool, profile_id: Uuid, channel: &str, hours: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE profile_id = $1 AND channel = $2 AND status IN ('sent','delivered') AND NOT is_test AND NOT transactional
                        AND sent_at > now() - make_interval(hours => $3))",
    )
    .bind(profile_id)
    .bind(channel)
    .bind(hours as i32)
    .fetch_one(db)
    .await
}

/// Where a campaign's send stands: messages per release time (with their zones) by status, and why
/// people were skipped.
pub async fn progress(db: &PgPool, account_id: Uuid, campaign_id: Uuid) -> AppResult<Value> {
    let c = crate::campaigns::load(db, account_id, campaign_id).await?;
    let rows: Vec<(Option<DateTime<Utc>>, Vec<String>, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT send_after, COALESCE((array_agg(DISTINCT send_tz) FILTER (WHERE send_tz IS NOT NULL))[1:4], '{}'),
                count(*) FILTER (WHERE status IN ('queued','sending')),
                count(*) FILTER (WHERE status IN ('sent','delivered','bounced','complained')),
                count(*) FILTER (WHERE status = 'skipped' AND error = 'smart sending'),
                count(*) FILTER (WHERE status = 'skipped' AND error IS DISTINCT FROM 'smart sending'),
                count(*) FILTER (WHERE status = 'failed')
         FROM messages WHERE campaign_id = $1 AND NOT is_test
         GROUP BY send_after ORDER BY send_after NULLS FIRST LIMIT 400",
    )
    .bind(c.id)
    .fetch_all(db)
    .await?;
    let reasons: Vec<(Option<String>, i64)> = sqlx::query_as("SELECT error, count(*) FROM messages WHERE campaign_id = $1 AND NOT is_test AND status = 'skipped' GROUP BY 1 ORDER BY 2 DESC LIMIT 20")
        .bind(c.id)
        .fetch_all(db)
        .await?;
    let mut totals = [0i64; 5];
    let buckets: Vec<Value> = rows
        .iter()
        .map(|(at, zones, queued, sent, smart, skipped, failed)| {
            for (t, v) in totals.iter_mut().zip([queued, sent, smart, skipped, failed]) {
                *t += *v;
            }
            json!({"at": at, "zones": zones, "waiting": queued, "sent": sent, "skipped_smart_sending": smart, "skipped_other": skipped, "failed": failed})
        })
        .collect();
    let opts = options::of(c.send_options.as_ref());
    Ok(json!({
        "campaign_id": c.id,
        "status": c.status,
        "send_options": opts,
        "description": options::describe(opts.as_ref()),
        "totals": {"waiting": totals[0], "sent": totals[1], "skipped_smart_sending": totals[2], "skipped_other": totals[3], "failed": totals[4]},
        "next_release": rows.iter().filter(|r| r.2 > 0).find_map(|r| r.0),
        "buckets": buckets,
        "skip_reasons": reasons.into_iter().map(|(why, n)| json!({"reason": why.unwrap_or_default(), "count": n})).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::{America, Asia};

    fn plan(opts: SendOptions, start: DateTime<Utc>) -> Plan {
        Plan { local_date: start.with_timezone(&America::Los_Angeles).date_naive(), opts, start, account_tz: America::Los_Angeles, sms_window: None, account_hist: Histogram::default() }
    }

    fn utc(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap()
    }

    #[test]
    fn local_time_releases_each_zone_at_its_hour() {
        let opts = SendOptions { timing: Timing::LocalTime, local_time: Some("10:00".into()), ..Default::default() };
        // Campaign day Nov 20; sending starts when UTC+14 reaches 10:00 (Nov 19 20:00 UTC).
        let mut p = plan(opts, utc(19, 20, 0));
        p.local_date = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        assert_eq!(p.release(Asia::Kolkata, 0, 1, None), utc(20, 4, 30));
        assert_eq!(p.release(Asia::Kathmandu, 0, 1, None), utc(20, 4, 15));
        assert_eq!(p.release(America::New_York, 0, 1, None), utc(20, 15, 0));
        assert_eq!(p.release("Pacific/Honolulu".parse().unwrap(), 0, 1, None), utc(20, 20, 0));
    }

    #[test]
    fn late_zones_go_next_day_or_at_once() {
        let mut opts = SendOptions { timing: Timing::LocalTime, local_time: Some("10:00".into()), ..Default::default() };
        // Sent at Nov 20 16:00 UTC: 10:00 already passed in New York (15:00 UTC), not in Los Angeles.
        let mut p = plan(opts.clone(), utc(20, 16, 0));
        p.local_date = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        assert_eq!(p.release(America::New_York, 0, 1, None), utc(21, 15, 0));
        assert_eq!(p.release(America::Los_Angeles, 0, 1, None), utc(20, 18, 0));
        opts.late = Late::Immediately;
        p.opts = opts;
        assert_eq!(p.release(America::New_York, 0, 1, None), utc(20, 16, 0));
    }

    #[test]
    fn gradual_spreads_in_hourly_batches() {
        let opts = SendOptions { timing: Timing::Gradual, spread_hours: Some(4), ..Default::default() };
        let p = plan(opts, utc(20, 12, 0));
        let at: Vec<i64> = (0..8).map(|i| (p.release(America::Chicago, i, 8, None) - utc(20, 12, 0)).num_hours()).collect();
        assert_eq!(at, vec![0, 0, 1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn best_time_follows_history_then_account_then_default() {
        let opts = SendOptions { timing: Timing::BestTime, window_start: Some("08:00".into()), window_end: Some("20:00".into()), within_hours: Some(24), ..Default::default() };
        // Friday Nov 20 2026, 06:00 in Chicago.
        let mut p = plan(opts, utc(20, 12, 0));
        let chi = America::Chicago;
        // Clicks at 19:00 Chicago on earlier days: their best hour is 19:00 today.
        let clicks: Vec<DateTime<Utc>> = (1..=4).map(|d| utc(20 - d, 1, 0)).collect();
        let h = Histogram::personal(chi, utc(20, 12, 0), &[], &clicks);
        assert_eq!(p.release(chi, 0, 1, Some(&h)), utc(21, 1, 0));
        // An hour outside the window never wins: opens at 23:00 local go at the window's last hour.
        let late: Vec<DateTime<Utc>> = (1..=4).map(|d| utc(20 - d, 5, 0)).collect();
        let h = Histogram::personal(chi, utc(20, 12, 0), &late, &[]);
        assert_eq!(p.release(chi, 0, 1, Some(&h)).with_timezone(&chi).hour(), 19, "the window hour nearest their habit");
        // No history: 10:00 local.
        assert_eq!(p.release(chi, 0, 1, None), utc(20, 16, 0));
        // Thin personal history: the account's best hour (14:00) instead.
        for d in 0..7 {
            for _ in 0..10 {
                p.account_hist.add(d, 14, 1.0);
            }
        }
        let thin = Histogram::personal(chi, utc(20, 12, 0), &[utc(19, 1, 0)], &[]);
        assert_eq!(p.release(chi, 0, 1, Some(&thin)).with_timezone(&chi).hour(), 14);
    }

    #[test]
    fn sms_never_lands_in_quiet_hours() {
        let opts = SendOptions { timing: Timing::Gradual, spread_hours: Some(10), ..Default::default() };
        // Starts 18:00 in New York; batches past 20:00 wait for 09:00 the next morning.
        let mut p = plan(opts, utc(20, 23, 0));
        p.sms_window = Some((540, 1200));
        assert_eq!(p.release(America::New_York, 0, 10, None), utc(20, 23, 0));
        assert_eq!(p.release(America::New_York, 3, 10, None), utc(21, 14, 0));
    }

    #[test]
    fn clicks_beat_opens_when_there_are_enough() {
        let tz = America::Chicago;
        let now = utc(20, 12, 0);
        let opens: Vec<DateTime<Utc>> = (1..=10).map(|d| utc(20 - d, 14, 0)).collect();
        let clicks: Vec<DateTime<Utc>> = (1..=3).map(|d| utc(20 - d, 22, 0)).collect();
        let h = Histogram::personal(tz, now, &opens, &clicks);
        let open_hour = utc(19, 14, 0).with_timezone(&tz).hour();
        let click_hour = utc(19, 22, 0).with_timezone(&tz).hour();
        assert!(h.score(3, click_hour) > h.score(3, open_hour));
    }
}

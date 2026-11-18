//! A calendar of campaigns by send time, in the account's time zone, and moving a scheduled
//! campaign to another day.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::features::sendtime::clock::{local_instant, local_minute};
use crate::features::sendtime::options::{self, Timing};
use crate::models::CampaignRow;
use crate::state::AppState;

/// The longest range one request may cover, in days.
pub const MAX_DAYS: i64 = 62;
/// A campaign this close to going out can't be moved.
pub const FREEZE_MINUTES: i64 = 15;
const MAX_ITEMS: i64 = 1000;

/// Campaigns are placed by when they go out: sent (and sending, stopped or failed) ones by when they
/// started, scheduled ones and drafts with a planned date by their send time.
const AT: &str = "CASE WHEN c.status IN ('sending', 'testing', 'sent', 'cancelled', 'failed') THEN COALESCE(c.sent_at, c.send_at) ELSE c.send_at END";

type Row = (Uuid, String, String, String, Option<DateTime<Utc>>, Option<Value>, bool, bool, bool);

fn note(opts: Option<&options::SendOptions>, tz: chrono_tz::Tz) -> Option<String> {
    let o = opts?;
    match o.timing {
        Timing::AllAtOnce => None,
        Timing::LocalTime => {
            let day = o.local_date.map(|d| d.format("%b %-d").to_string()).unwrap_or_else(|| "the send day".into());
            Some(format!("Goes out at {} in each person's time zone on {day}. Shown on the day it starts, when the first time zone reaches that hour.", o.local_time.as_deref().unwrap_or("10:00")))
        }
        Timing::BestTime => Some(format!("Goes out at each person's best hour within {} hours of this time.", o.within_hours.unwrap_or(24))),
        Timing::Gradual => Some(format!("Spread over {} hours from this time ({}).", o.spread_hours.unwrap_or(6), tz.name())),
    }
}

fn timing_name(o: Option<&options::SendOptions>) -> &'static str {
    match o.map(|o| o.timing) {
        None | Some(Timing::AllAtOnce) => "all_at_once",
        Some(Timing::LocalTime) => "local_time",
        Some(Timing::BestTime) => "best_time",
        Some(Timing::Gradual) => "gradual",
    }
}

/// Campaigns placed on the days `from` to `to` (both included) in the account's time zone.
pub async fn calendar(state: &AppState, account_id: Uuid, from: NaiveDate, to: NaiveDate, include_archived: bool) -> AppResult<Value> {
    if to < from {
        return Err(AppError::bad("to must be on or after from"));
    }
    if (to - from).num_days() + 1 > MAX_DAYS {
        return Err(AppError::bad(format!("show at most {MAX_DAYS} days at a time")));
    }
    let tz_name: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let start = local_instant(tz, from, 0);
    let end = local_instant(tz, to + Duration::days(1), 0);
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT c.id, c.name, c.channel, c.status, {AT}, c.send_options, c.archived_at IS NOT NULL,
                EXISTS (SELECT 1 FROM campaign_resends r WHERE r.campaign_id = c.id), c.ab_test IS NOT NULL
         FROM campaigns c WHERE c.account_id = $1 AND {AT} >= $2 AND {AT} < $3 AND ($4 OR c.archived_at IS NULL)
         ORDER BY {AT}, c.id LIMIT $5"
    ))
    .bind(account_id)
    .bind(start)
    .bind(end)
    .bind(include_archived)
    .bind(MAX_ITEMS)
    .fetch_all(&state.db)
    .await?;
    let now = Utc::now();
    let items: Vec<Value> = rows
        .into_iter()
        .filter_map(|(id, name, channel, status, at, send_options, archived, resend, ab)| {
            let at = at?;
            let o = options::of(send_options.as_ref());
            let local = at.with_timezone(&tz);
            Some(json!({
                "id": id, "name": name, "channel": channel, "status": status, "at": at,
                "date": local.format("%Y-%m-%d").to_string(), "time": local.format("%H:%M").to_string(),
                "timing": timing_name(o.as_ref()), "note": note(o.as_ref(), tz),
                "archived": archived, "resend": resend, "ab_test": ab,
                "can_move": status == "scheduled" && !archived && at > now + Duration::minutes(FREEZE_MINUTES),
            }))
        })
        .collect();
    Ok(json!({"from": from, "to": to, "timezone": tz.name(), "items": items, "truncated": items.len() as i64 >= MAX_ITEMS}))
}

/// A planned move: the send time to store, the send options to store with it (a local-time send
/// records its new day), and the day it was asked for.
pub struct Move {
    pub send_at: DateTime<Utc>,
    pub send_options: Option<serde_json::Value>,
}

/// Where a scheduled campaign would go when moved to `date` (its day in the account's time zone),
/// keeping its time of day. For a local-time send the day it goes out moves by as many days as its
/// start does. Refused when the start wouldn't fall on `date` in the account's zone (a local-time
/// send across a clock change can start a day off), or would be within 15 minutes.
pub async fn plan_move(state: &AppState, account_id: Uuid, c: &CampaignRow, date: NaiveDate) -> AppResult<Move> {
    if c.status != "scheduled" {
        return Err(AppError::Conflict(format!("only scheduled campaigns can be moved; this one is {}", c.status)));
    }
    let now = Utc::now();
    let old = c.send_at.ok_or_else(|| AppError::Conflict("this campaign has no send time".into()))?;
    if old <= now + Duration::minutes(FREEZE_MINUTES) {
        return Err(AppError::Conflict(format!("this campaign goes out in less than {FREEZE_MINUTES} minutes and can't be moved now")));
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let tz = crate::sending::account_tz(&account);
    let old_day = old.with_timezone(&tz).date_naive();
    let o = options::of(c.send_options.as_ref());
    let wanted = match o.as_ref() {
        Some(o) if o.timing == Timing::LocalTime => {
            let day = o.local_date.unwrap_or(old_day) + (date - old_day);
            local_instant(tz, day, 12 * 60)
        }
        _ => local_instant(tz, date, local_minute(tz, old)),
    };
    // When it would start, worked out the way scheduling does; None means right away.
    let (start, dated) = crate::features::sendtime::plan::launch_start(state, &account, c, Some(wanted)).await?;
    let start = match start {
        Some(s) if s > now + Duration::minutes(FREEZE_MINUTES) => s,
        _ => return Err(AppError::bad(format!("that would send it now or within {FREEZE_MINUTES} minutes; pick a later day"))),
    };
    let start_day = start.with_timezone(&tz).date_naive();
    if start_day != date {
        return Err(AppError::bad(format!(
            "moved to {}, it would start on {} in your time zone (each person's local time crosses a day there); pick another day",
            date.format("%b %-d"),
            start_day.format("%b %-d")
        )));
    }
    Ok(Move { send_at: start, send_options: dated })
}

/// Move a scheduled campaign to another day, in one conditional write: only while it's still
/// scheduled, unarchived and unchanged since it was read, and only while both its current and its
/// new send time are more than 15 minutes away by the database's clock. Never sends it now.
pub async fn reschedule(state: &AppState, account_id: Uuid, id: Uuid, date: NaiveDate) -> AppResult<(CampaignRow, DateTime<Utc>)> {
    let c = crate::campaigns::load(&state.db, account_id, id).await?;
    let old = c.send_at.unwrap_or_else(Utc::now);
    let m = plan_move(state, account_id, &c, date).await?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let problems = crate::campaigns::readiness(&account, &c, state.email.name() == "ses");
    if !problems.is_empty() {
        return Err(AppError::bad(format!("Not ready to send: {}", problems.join("; "))));
    }
    let row: Option<CampaignRow> = sqlx::query_as(
        "UPDATE campaigns SET send_at = $3, send_options = CASE WHEN $4 THEN $5 ELSE send_options END, updated_at = now()
         WHERE id = $1 AND account_id = $2 AND status = 'scheduled' AND updated_at = $6 AND archived_at IS NULL
           AND send_at > now() + make_interval(mins => $7) AND $3 > now() + make_interval(mins => $7)
         RETURNING *",
    )
    .bind(id)
    .bind(account_id)
    .bind(m.send_at)
    .bind(m.send_options.is_some())
    .bind(&m.send_options)
    .bind(c.updated_at)
    .bind(FREEZE_MINUTES as i32)
    .fetch_optional(&state.db)
    .await?;
    match row {
        Some(r) => Ok((r, old)),
        None => {
            let now = crate::campaigns::load(&state.db, account_id, id).await?;
            Err(AppError::Conflict(if now.status != "scheduled" {
                format!("this campaign is {} now and can't be moved", now.status)
            } else {
                format!("this campaign changed while you were moving it, or is less than {FREEZE_MINUTES} minutes from going out; open it again")
            }))
        }
    }
}

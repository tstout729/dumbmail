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

/// Where a scheduled campaign would go when moved to `date` (its day in the account's time zone),
/// keeping its time of day. For a local-time send the day it goes out moves by as many days as its
/// start does. Returns the send time to give `launch`, and when it would start.
pub async fn plan_move(state: &AppState, account_id: Uuid, c: &CampaignRow, date: NaiveDate) -> AppResult<(DateTime<Utc>, DateTime<Utc>)> {
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
    let send_at = match o.as_ref() {
        Some(o) if o.timing == Timing::LocalTime => {
            let day = o.local_date.unwrap_or(old_day) + (date - old_day);
            local_instant(tz, day, 12 * 60)
        }
        _ => local_instant(tz, date, local_minute(tz, old)),
    };
    // When it would start, worked out the way scheduling does; None means right away.
    let (start, _) = crate::features::sendtime::plan::launch_start(state, &account, c, Some(send_at)).await?;
    match start {
        Some(s) if s > now + Duration::minutes(FREEZE_MINUTES) => Ok((send_at, s)),
        _ => Err(AppError::bad(format!("that would send it now or within {FREEZE_MINUTES} minutes; pick a later day"))),
    }
}

/// Move a scheduled campaign to another day through the normal scheduling path (readiness, billing,
/// send options, and a write that only succeeds if the campaign hasn't changed meanwhile).
pub async fn reschedule(state: &AppState, account_id: Uuid, id: Uuid, date: NaiveDate) -> AppResult<(CampaignRow, DateTime<Utc>)> {
    let c = crate::campaigns::load(&state.db, account_id, id).await?;
    let old = c.send_at.unwrap_or_else(Utc::now);
    let (send_at, _) = plan_move(state, account_id, &c, date).await?;
    let row = crate::api::messaging::launch(state, account_id, id, Some(send_at), None).await?;
    Ok((row, old))
}

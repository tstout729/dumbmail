//! Shared plumbing for report queries: a bind-parameter builder, a time limit on every statement,
//! and the account's time zone and date ranges.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Longest any report statement may run in a request.
pub const REQUEST_TIMEOUT: &str = "20s";
/// Longest a statement may run in a background job (exports, scheduled reports).
pub const JOB_TIMEOUT: &str = "120s";

/// A transaction whose statements stop after [`REQUEST_TIMEOUT`], so one heavy report can't hold
/// the database.
pub async fn timed(state: &AppState) -> AppResult<Transaction<'static, Postgres>> {
    timed_for(state, REQUEST_TIMEOUT).await
}

pub async fn timed_for(state: &AppState, limit: &'static str) -> AppResult<Transaction<'static, Postgres>> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
    // `limit` is one of the constants above, never user input.
    sqlx::query(&format!("SET LOCAL statement_timeout = '{limit}'")).execute(&mut *tx).await?;
    Ok(tx)
}

/// Turn a timed-out statement into an answer people can act on.
pub fn db_err(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(d) = &e {
        if d.code().as_deref() == Some("57014") {
            return AppError::bad("This report took too long to run. Try a shorter date range or fewer groups.");
        }
    }
    e.into()
}

/// One bound value.
#[derive(Debug, Clone)]
pub enum Arg {
    Uuid(Uuid),
    Text(String),
    Ts(DateTime<Utc>),
    Int(i64),
    TextArr(Vec<String>),
    UuidArr(Vec<Uuid>),
}

/// SQL text plus its bound values. `$1` is always the account id, so query fragments written for
/// the account (like the attribution query) can be embedded as they are.
pub struct Sql {
    pub text: String,
    args: Vec<Arg>,
}

impl Sql {
    pub fn new(account_id: Uuid) -> Self {
        Sql { text: String::new(), args: vec![Arg::Uuid(account_id)] }
    }

    /// Add a value and return its placeholder.
    pub fn bind(&mut self, a: Arg) -> String {
        self.args.push(a);
        format!("${}", self.args.len())
    }

    pub fn push(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self
    }

    pub async fn fetch_all(&self, tx: &mut Transaction<'static, Postgres>) -> AppResult<Vec<PgRow>> {
        let mut q = sqlx::query(&self.text);
        for a in &self.args {
            q = match a {
                Arg::Uuid(v) => q.bind(*v),
                Arg::Text(v) => q.bind(v.clone()),
                Arg::Ts(v) => q.bind(*v),
                Arg::Int(v) => q.bind(*v),
                Arg::TextArr(v) => q.bind(v.clone()),
                Arg::UuidArr(v) => q.bind(v.clone()),
            };
        }
        q.fetch_all(&mut **tx).await.map_err(db_err)
    }
}

pub async fn account_tz(state: &AppState, account_id: Uuid) -> AppResult<(String, chrono_tz::Tz)> {
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let parsed = tz.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::America::Los_Angeles);
    Ok((parsed.name().to_string(), parsed))
}

/// Midnight at the start of `d` in `tz`, as UTC. A midnight skipped by daylight saving moves later.
pub fn day_start(tz: chrono_tz::Tz, d: NaiveDate) -> DateTime<Utc> {
    let mut t = d.and_hms_opt(0, 0, 0).expect("midnight");
    for _ in 0..4 {
        if let Some(x) = tz.from_local_datetime(&t).earliest() {
            return x.with_timezone(&Utc);
        }
        t += chrono::Duration::hours(1);
    }
    Utc.from_utc_datetime(&t)
}

/// A half-open time range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Window {
    /// The same length of time immediately before.
    pub fn previous(&self) -> Window {
        Window { start: self.start - (self.end - self.start), end: self.start }
    }
}

pub const MAX_RANGE_DAYS: i64 = 731;

/// `days` means the last `days` x 24 hours up to now, the same window as the analytics overview.
/// `start`/`end` are calendar dates in the account's time zone, both included.
pub fn resolve_range(tz: chrono_tz::Tz, days: Option<i64>, start: Option<NaiveDate>, end: Option<NaiveDate>) -> AppResult<Window> {
    let now = Utc::now();
    match (start, end) {
        (None, None) => {
            let days = days.unwrap_or(30);
            if !(1..=MAX_RANGE_DAYS).contains(&days) {
                return Err(AppError::bad(format!("range.days must be 1-{MAX_RANGE_DAYS}")));
            }
            Ok(Window { start: now - chrono::Duration::days(days), end: now })
        }
        (Some(s), e) => {
            let e = e.unwrap_or_else(|| now.with_timezone(&tz).date_naive());
            if e < s {
                return Err(AppError::bad("range.end must be on or after range.start"));
            }
            if (e - s).num_days() + 1 > MAX_RANGE_DAYS {
                return Err(AppError::bad(format!("a range can cover at most {MAX_RANGE_DAYS} days")));
            }
            Ok(Window { start: day_start(tz, s), end: day_start(tz, e + chrono::Duration::days(1)) })
        }
        (None, Some(_)) => Err(AppError::bad("range.end needs range.start")),
    }
}

/// Format a timestamp in the account's time zone, with its offset (for CSV files).
pub fn local_ts(tz: chrono_tz::Tz, t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&tz).format("%Y-%m-%dT%H:%M:%S%:z").to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_follow_the_account_time_zone() {
        let tz = chrono_tz::America::Los_Angeles;
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let w = resolve_range(tz, None, Some(d("2026-11-01")), Some(d("2026-11-30"))).unwrap();
        // Nov 1 starts at 07:00 UTC (daylight time); Dec 1 at 08:00 UTC (standard time).
        assert_eq!(w.start.to_rfc3339(), "2026-11-01T07:00:00+00:00");
        assert_eq!(w.end.to_rfc3339(), "2026-12-01T08:00:00+00:00");
        assert_eq!(w.previous().end, w.start);
        assert!(resolve_range(tz, None, Some(d("2026-11-02")), Some(d("2026-11-01"))).is_err());
        assert!(resolve_range(tz, Some(0), None, None).is_err());
        assert!(resolve_range(tz, None, Some(d("2020-01-01")), Some(d("2026-01-01"))).is_err());
        assert_eq!(local_ts(tz, Some(w.start)), "2026-11-01T00:00:00-07:00");
    }
}

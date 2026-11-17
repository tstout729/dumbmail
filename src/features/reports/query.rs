//! Shared plumbing for report queries: limits on how much reporting runs at once and for how long,
//! a bind-parameter builder, and the account's time zone and date ranges.
//!
//! Every report (custom, ready-made, cohorts, funnels, deliverability, benchmarks, attribution
//! previews, scheduled emails) runs inside a [`Run`]: one at a time per account, at most
//! [`max_concurrent`] across the server (so reporting can only ever use a few of the database
//! connections live sending needs), and one deadline for all of its statements together. Exports and
//! MX lookups take a slot of the same global limit.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Transaction};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// The whole of one report request, across all its statements.
pub const REQUEST_BUDGET: Duration = Duration::from_secs(20);
/// The whole of one background run (a scheduled report, one export batch, one MX lookup job).
pub const JOB_BUDGET: Duration = Duration::from_secs(120);
/// Default for `REPORTS_MAX_CONCURRENT`.
const DEFAULT_CONCURRENT: usize = 4;

/// Where a report runs: in a request (20 seconds, a second report for the same account is refused)
/// or in a background job (2 minutes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Request,
    Job,
}

impl Mode {
    pub fn budget(self) -> Duration {
        match self {
            Mode::Request => REQUEST_BUDGET,
            Mode::Job => JOB_BUDGET,
        }
    }
}

/// Most report, cohort, funnel, deliverability and export queries the server runs at once:
/// `REPORTS_MAX_CONCURRENT` (1-16), default 4 of the 20 database connections.
pub fn max_concurrent() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("REPORTS_MAX_CONCURRENT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_CONCURRENT).clamp(1, 16))
}

fn global() -> Arc<Semaphore> {
    static S: OnceLock<Arc<Semaphore>> = OnceLock::new();
    S.get_or_init(|| Arc::new(Semaphore::new(max_concurrent()))).clone()
}

fn running() -> &'static Mutex<HashSet<Uuid>> {
    static R: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Marks an account as running a report until dropped.
struct AccountSlot(Uuid);

impl Drop for AccountSlot {
    fn drop(&mut self) {
        running().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

pub const BUSY_ACCOUNT: &str = "A report is already running for this account. Wait for it to finish, then try again.";
const BUSY_SERVER: &str = "Reports are busy right now. Try again in a minute.";
const TOO_LONG: &str = "This report took too long to run. Try a shorter date range or fewer groups.";

/// One report's permission to run, and its deadline.
pub struct Run {
    deadline: Instant,
    _slot: Option<AccountSlot>,
    _permit: OwnedSemaphorePermit,
}

impl Run {
    /// Start a report for `account_id`. Refused (429) while another report runs for the account;
    /// waits for a global slot until the deadline.
    pub async fn start(account_id: Uuid, mode: Mode) -> AppResult<Run> {
        let deadline = Instant::now() + mode.budget();
        let slot = {
            let mut set = running().lock().unwrap_or_else(|e| e.into_inner());
            if !set.insert(account_id) {
                return Err(AppError::Busy(BUSY_ACCOUNT.into()));
            }
            AccountSlot(account_id)
        };
        let permit = Self::permit(deadline).await?;
        Ok(Run { deadline, _slot: Some(slot), _permit: permit })
    }

    /// A slot of the global limit only (exports and MX lookups, which have their own per-account rule).
    pub async fn background(budget: Duration) -> AppResult<Run> {
        let deadline = Instant::now() + budget;
        let permit = Self::permit(deadline).await?;
        Ok(Run { deadline, _slot: None, _permit: permit })
    }

    async fn permit(deadline: Instant) -> AppResult<OwnedSemaphorePermit> {
        match tokio::time::timeout_at(deadline.into(), global().acquire_owned()).await {
            Ok(Ok(p)) => Ok(p),
            _ => Err(AppError::Busy(BUSY_SERVER.into())),
        }
    }

    /// Time left before the deadline.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// A read-only transaction whose statements together stop at the deadline.
    pub async fn tx(&self, state: &AppState) -> AppResult<Tx> {
        let mut tx = state.db.begin().await?;
        sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
        Ok(Tx { tx, deadline: self.deadline })
    }
}

/// A report transaction. Before each statement the time limit is set to what's left of the run's
/// deadline, so several statements can't add up to more than one.
pub struct Tx {
    tx: Transaction<'static, Postgres>,
    deadline: Instant,
}

impl Tx {
    /// Set the time limit for the next statement and hand out the transaction to run it on.
    pub async fn arm(&mut self) -> AppResult<&mut Transaction<'static, Postgres>> {
        let left = self.deadline.saturating_duration_since(Instant::now()).as_millis();
        if left < 50 {
            return Err(AppError::bad(TOO_LONG));
        }
        sqlx::query("SELECT set_config('statement_timeout', $1, true)").bind(format!("{left}ms")).execute(&mut *self.tx).await?;
        Ok(&mut self.tx)
    }

    pub async fn fetch_all(&mut self, q: &Sql) -> AppResult<Vec<PgRow>> {
        let tx = self.arm().await?;
        q.query().fetch_all(&mut **tx).await.map_err(db_err)
    }

    pub async fn commit(self) -> AppResult<()> {
        self.tx.commit().await?;
        Ok(())
    }
}

/// Turn a timed-out statement into an answer people can act on.
pub fn db_err(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(d) = &e {
        if d.code().as_deref() == Some("57014") {
            return AppError::bad(TOO_LONG);
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

    /// The query with its values bound.
    pub fn query(&self) -> sqlx::query::Query<'_, Postgres, sqlx::postgres::PgArguments> {
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
        q
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

//! Watching changes without getting in their way.
//!
//! Webhooks and data exports observe the paths where messages are sent, consent is saved, provider
//! callbacks land, segments refresh and events are recorded. Two rules hold for every observer:
//!
//! - **An account that uses none of it pays nothing.** Which accounts have an enabled webhook
//!   endpoint or export destination, and what they want, is one snapshot per server ([`peek`]),
//!   reloaded in the background every [`ttl`] (one query for every account). Observers ask it
//!   before building a payload, collecting ids or running any SQL, and the shared send paths ask
//!   it before fetching affected rows just to report them. With nothing configured, nothing runs.
//!   A new endpoint or destination is seen by every server within one reload.
//! - **An observer never fails, blocks or slows what it observes.** Observer writes are
//!   append-only inserts (no unique key to fight over, no foreign key to lock). On a caller's
//!   connection they run under a savepoint with a 50 ms lock timeout and a 200 ms statement
//!   timeout ([`enter`] / [`leave`]); on their own they run in a short transaction with the same
//!   limits ([`begin`] / [`commit`]). A write that hits a limit is rolled back, logged and counted
//!   ([`misses`]); the caller carries on as if nothing was watching.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

/// One enabled webhook endpoint, as far as deciding whether to write an outbox row goes.
pub(crate) struct Sub {
    pub topics: Vec<String>,
    pub metrics: Vec<String>,
    pub trusted_only: bool,
}

/// What one account has set up.
#[derive(Default)]
pub(crate) struct Account {
    pub subs: Vec<Sub>,
    /// Datasets the account's enabled export destinations export.
    pub datasets: Vec<String>,
    /// Rows waiting in the webhook outbox at the sweep's last count (none counted: none waiting).
    pub outbox_rows: i64,
}

/// Every account that has anything set up. Accounts not in it have nothing.
#[derive(Default)]
pub(crate) struct Interest {
    accounts: HashMap<Uuid, Account>,
    /// When the sweep last counted every account's outbox.
    swept_at: Option<DateTime<Utc>>,
}

impl Interest {
    pub fn account(&self, id: Uuid) -> Option<&Account> {
        self.accounts.get(&id)
    }

    /// Whether any account at all has something set up (for sweeps that span accounts).
    pub fn any(&self) -> bool {
        !self.accounts.is_empty()
    }
}

struct Snap {
    loaded_at: Option<Instant>,
    loading: bool,
    interest: Arc<Interest>,
    /// Outbox rows this server added per account since the sweep it last saw (keyed by its time).
    added: HashMap<Uuid, (Option<DateTime<Utc>>, i64)>,
}

/// One snapshot per database (production has one; each test has its own).
static SNAPS: LazyLock<Mutex<HashMap<String, Snap>>> = LazyLock::new(Default::default);
static TTL_MS: AtomicU64 = AtomicU64::new(5_000);
static DISABLED: AtomicBool = AtomicBool::new(false);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// How long a snapshot is used before it's reloaded. A new endpoint or destination is seen within this.
pub fn ttl() -> Duration {
    Duration::from_millis(TTL_MS.load(Ordering::Relaxed))
}

/// Tests: keep snapshots longer (so a measurement isn't interrupted by a reload).
pub fn set_ttl_for_tests(d: Duration) {
    TTL_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

/// Tests: act as if no account had anything set up, without reading the snapshot. Lets a test
/// compare a path with observers present but idle against the same path with them switched off.
pub fn set_disabled_for_tests(off: bool) {
    DISABLED.store(off, Ordering::Relaxed);
}

/// Observer writes given up because they would have waited (a lock or statement timeout, or no
/// free connection). Each is also logged.
pub fn misses() -> u64 {
    MISSES.load(Ordering::Relaxed)
}

fn key(db: &PgPool) -> String {
    db.connect_options().get_database().unwrap_or("").to_string()
}

const LOAD_SQL: &str = "
    SELECT e.account_id, 'hook' AS kind, e.topics AS a, e.metrics AS b, e.trusted_only AS t, 0::bigint AS n, NULL::timestamptz AS at
    FROM webhook_endpoints e
    WHERE e.enabled AND (e.api_key_id IS NULL OR EXISTS (SELECT 1 FROM api_keys k WHERE k.id = e.api_key_id AND k.revoked_at IS NULL))
    UNION ALL
    SELECT d.account_id, 'export', d.datasets, '{}'::text[], false, 0, NULL FROM warehouse_destinations d WHERE d.enabled
    UNION ALL
    SELECT c.account_id, 'outbox', '{}'::text[], '{}'::text[], false, c.rows, c.counted_at FROM webhook_outbox_counts c";

/// The `webhook_outbox_counts` row that records when the sweep last counted (see `webhooks::fanout`).
pub(crate) const SWEPT: Uuid = Uuid::nil();

type LoadRow = (Uuid, String, Vec<String>, Vec<String>, bool, i64, Option<DateTime<Utc>>);

/// Load the snapshot now (the background reload does this; tests call it to start warm).
pub async fn refresh(db: &PgPool) -> sqlx::Result<()> {
    let rows: Vec<LoadRow> = sqlx::query_as(LOAD_SQL).fetch_all(db).await?;
    let mut interest = Interest::default();
    for (account_id, kind, a, b, t, n, at) in rows {
        if account_id == SWEPT {
            interest.swept_at = at;
            continue;
        }
        let acc = interest.accounts.entry(account_id).or_default();
        match kind.as_str() {
            "hook" => acc.subs.push(Sub { topics: a, metrics: b, trusted_only: t }),
            "export" => {
                for d in a {
                    if !acc.datasets.contains(&d) {
                        acc.datasets.push(d);
                    }
                }
            }
            _ => acc.outbox_rows = n,
        }
    }
    // An account that only has an old outbox count has nothing to observe.
    interest.accounts.retain(|_, a| !a.subs.is_empty() || !a.datasets.is_empty());
    if let Ok(mut m) = SNAPS.lock() {
        let s = m.entry(key(db)).or_insert_with(|| Snap { loaded_at: None, loading: false, interest: Arc::default(), added: HashMap::new() });
        s.interest = Arc::new(interest);
        s.loaded_at = Some(Instant::now());
        s.loading = false;
    }
    Ok(())
}

/// The current snapshot, without waiting and without SQL. When it's older than [`ttl`] a reload
/// starts in the background (and the old one is used meanwhile). `None` while the first load on
/// this server hasn't finished: callers then treat every account as maybe interested and let the
/// (guarded) insert decide.
pub(crate) fn peek(db: &PgPool) -> Option<Arc<Interest>> {
    if DISABLED.load(Ordering::Relaxed) {
        return Some(Arc::default());
    }
    let (current, reload) = {
        let Ok(mut m) = SNAPS.lock() else { return None };
        let s = m.entry(key(db)).or_insert_with(|| Snap { loaded_at: None, loading: false, interest: Arc::default(), added: HashMap::new() });
        let stale = s.loaded_at.is_none_or(|t| t.elapsed() >= ttl());
        let reload = stale && !s.loading;
        // (A failed reload leaves `loaded_at` set but old, so the old snapshot keeps being used.)
        if reload {
            s.loading = true;
        }
        (s.loaded_at.map(|_| s.interest.clone()), reload)
    };
    if reload {
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = refresh(&db).await {
                tracing::warn!(error = %e, "couldn't load webhook and export subscriptions; using the last ones");
                if let Ok(mut m) = SNAPS.lock() {
                    if let Some(s) = m.get_mut(&key(&db)) {
                        s.loading = false;
                    }
                }
            }
        });
    }
    current
}

/// After an endpoint or destination changed on this server: reload now (other servers reload
/// within [`ttl`]). If that fails, the next look reloads.
pub(crate) async fn changed(db: &PgPool) {
    if let Err(e) = refresh(db).await {
        tracing::warn!(error = %e, "couldn't reload webhook and export subscriptions");
        if let Ok(mut m) = SNAPS.lock() {
            if let Some(s) = m.get_mut(&key(db)) {
                s.loaded_at = Some(Instant::now() - ttl());
            }
        }
    }
}

/// How many of `want` new outbox rows the account may still add (and reserve them). Uses the
/// sweep's last count plus what this server added since, so the limit holds without counting rows
/// on the way in. Unknown (no snapshot yet): all of them.
pub(crate) fn outbox_room(db: &PgPool, account_id: Uuid, want: i64, max: i64) -> i64 {
    let Some(interest) = peek(db) else { return want };
    let rows = interest.account(account_id).map(|a| a.outbox_rows).unwrap_or(0);
    let swept_at = interest.swept_at;
    let Ok(mut m) = SNAPS.lock() else { return want };
    let Some(s) = m.get_mut(&key(db)) else { return want };
    let added = s.added.entry(account_id).or_insert((swept_at, 0));
    if added.0 != swept_at {
        *added = (swept_at, 0);
    }
    let room = (max - rows - added.1).clamp(0, want);
    added.1 += room;
    if s.added.len() > 50_000 {
        s.added.clear();
    }
    room
}

fn missed(what: &str, e: &dyn std::fmt::Display) {
    MISSES.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(observer = what, error = %e, "skipped an observer write so it couldn't hold up the change it watches");
}

const LIMITS: &str = "SELECT set_config('lock_timeout', '50ms', true), set_config('statement_timeout', '200ms', true)";

/// The caller's timeouts, restored by [`leave`].
pub(crate) struct Saved {
    lock_timeout: String,
    statement_timeout: String,
}

/// Start observer writes on a caller's connection (inside its transaction): a savepoint and short
/// timeouts. `None` means don't write (it's logged).
pub(crate) async fn enter(conn: &mut PgConnection, what: &str) -> Option<Saved> {
    if let Err(e) = sqlx::query("SAVEPOINT dm_observe").execute(&mut *conn).await {
        missed(what, &e);
        return None;
    }
    let res: sqlx::Result<Saved> = async {
        let (lock_timeout, statement_timeout): (String, String) =
            sqlx::query_as("SELECT current_setting('lock_timeout'), current_setting('statement_timeout')").fetch_one(&mut *conn).await?;
        sqlx::query(LIMITS).execute(&mut *conn).await?;
        Ok(Saved { lock_timeout, statement_timeout })
    }
    .await;
    match res {
        Ok(s) => Some(s),
        Err(e) => {
            missed(what, &e);
            let _ = sqlx::query("ROLLBACK TO SAVEPOINT dm_observe").execute(&mut *conn).await;
            let _ = sqlx::query("RELEASE SAVEPOINT dm_observe").execute(&mut *conn).await;
            None
        }
    }
}

/// Finish what [`enter`] started: keep the writes and restore the caller's timeouts, or roll the
/// writes back (which also restores them) and count a miss.
pub(crate) async fn leave(conn: &mut PgConnection, saved: Saved, res: sqlx::Result<()>, what: &str) {
    let end: sqlx::Result<()> = async {
        match &res {
            Ok(()) => {
                sqlx::query("RELEASE SAVEPOINT dm_observe").execute(&mut *conn).await?;
                sqlx::query("SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)")
                    .bind(&saved.lock_timeout)
                    .bind(&saved.statement_timeout)
                    .execute(&mut *conn)
                    .await?;
            }
            Err(_) => {
                sqlx::query("ROLLBACK TO SAVEPOINT dm_observe").execute(&mut *conn).await?;
                sqlx::query("RELEASE SAVEPOINT dm_observe").execute(&mut *conn).await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        missed(what, &e);
    }
    if let Err(e) = end {
        missed(what, &e);
    }
}

/// Start observer writes with no caller transaction: a short transaction of their own, with the
/// same timeouts, on a connection that must be free within 250 ms.
pub(crate) async fn begin(db: &PgPool, what: &str) -> Option<Transaction<'static, Postgres>> {
    let res: Result<Transaction<'static, Postgres>, String> = async {
        let mut tx = tokio::time::timeout(Duration::from_millis(250), db.begin()).await.map_err(|_| "no free database connection".to_string())?.map_err(|e| e.to_string())?;
        sqlx::query(LIMITS).execute(&mut *tx).await.map_err(|e| e.to_string())?;
        Ok(tx)
    }
    .await;
    match res {
        Ok(tx) => Some(tx),
        Err(e) => {
            missed(what, &e);
            None
        }
    }
}

/// Finish what [`begin`] started.
pub(crate) async fn commit(tx: Transaction<'static, Postgres>, res: sqlx::Result<()>, what: &str) {
    match res {
        Ok(()) => {
            if let Err(e) = tx.commit().await {
                missed(what, &e);
            }
        }
        Err(e) => {
            missed(what, &e);
            let _ = tx.rollback().await;
        }
    }
}

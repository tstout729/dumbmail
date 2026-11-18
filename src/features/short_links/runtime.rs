//! What this server knows about short links right now: whether its index on `messages` is in place,
//! and whether it has been serving long enough to issue short links.
//!
//! During a deploy the old server keeps receiving clicks for a while after the new one starts, and
//! the old one can't resolve `/s/` links. So a server issues short links only once it has been
//! accepting requests for [`ISSUE_DELAY`] (longer than a deploy's overlap) and its index is ready;
//! until then texts get the long form. Resolving links already sent needs only the index.
//!
//! Kept per database (tests run many servers, each with its own database, in one process).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::state::AppState;

/// How long a server serves before it issues short links.
pub const ISSUE_DELAY: Duration = Duration::from_secs(120);
/// How often a click re-checks a missing index (a catalog lookup, never a table scan).
const RECHECK_EVERY: Duration = Duration::from_secs(10);

pub struct Runtime {
    index_ready: AtomicBool,
    last_check: Mutex<Option<Instant>>,
    serving_since: Mutex<Option<Instant>>,
    issue_delay: Mutex<Duration>,
}

static ALL: Lazy<Mutex<HashMap<String, Arc<Runtime>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn of(state: &AppState) -> Arc<Runtime> {
    lock(&ALL)
        .entry(state.cfg.database_url.clone())
        .or_insert_with(|| {
            Arc::new(Runtime { index_ready: AtomicBool::new(false), last_check: Mutex::new(None), serving_since: Mutex::new(None), issue_delay: Mutex::new(ISSUE_DELAY) })
        })
        .clone()
}

impl Runtime {
    pub fn index_ready(&self) -> bool {
        self.index_ready.load(Ordering::Relaxed)
    }

    /// Whether this server may put short links in texts now.
    pub fn issuing(&self) -> bool {
        let delay = *lock(&self.issue_delay);
        self.index_ready() && lock(&self.serving_since).is_some_and(|t| t.elapsed() >= delay)
    }
}

/// Call once the server accepts requests (`main.rs`, right after binding).
pub fn mark_serving(state: &AppState) {
    lock(&of(state).serving_since).get_or_insert_with(Instant::now);
}

/// Tests only: how long this server waits before issuing short links.
#[doc(hidden)]
pub fn set_issue_delay(state: &AppState, delay: Duration) {
    *lock(&of(state).issue_delay) = delay;
}

pub fn index_ready(state: &AppState) -> bool {
    of(state).index_ready()
}

pub fn issuing(state: &AppState) -> bool {
    of(state).issuing()
}

/// Look the index up in the catalog and remember the answer (set, or cleared if it went missing).
pub async fn check_index(state: &AppState) -> bool {
    let rt = of(state);
    *lock(&rt.last_check) = Some(Instant::now());
    let valid: Option<bool> = match sqlx::query_scalar(crate::features::reports::indexes::STATE_SQL).bind(super::INDEXES[0].0).fetch_optional(&state.db).await {
        Ok(v) => v,
        // Can't tell: keep what we knew.
        Err(_) => return rt.index_ready(),
    };
    rt.index_ready.store(valid == Some(true), Ordering::Relaxed);
    rt.index_ready()
}

/// Whether the index is in place, checking again (at most every [`RECHECK_EVERY`]) when it wasn't.
/// Clicks call this, so a server that just started resolves links as soon as it has looked.
pub async fn index_known(state: &AppState) -> bool {
    let rt = of(state);
    if rt.index_ready() {
        return true;
    }
    let due = lock(&rt.last_check).is_none_or(|t| t.elapsed() >= RECHECK_EVERY);
    if due {
        check_index(state).await
    } else {
        false
    }
}

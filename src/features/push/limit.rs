//! One gate for every call to a push service, first attempts and retries alike: at most
//! [`OVERALL`] calls in flight in this process, and at most [`PER_ACCOUNT`] for one account, so one
//! large send can't take every connection and a slow push service can't pile up work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub const OVERALL: usize = 64;
pub const PER_ACCOUNT: usize = 8;

static ALL: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(OVERALL)));
static ACCOUNTS: Lazy<Mutex<HashMap<Uuid, Arc<Semaphore>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Held for the length of one provider call.
pub struct Slot {
    _account: OwnedSemaphorePermit,
    _all: OwnedSemaphorePermit,
}

fn account_gate(account_id: Uuid) -> Arc<Semaphore> {
    let mut m = ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    // Forget gates nobody is using, so the map stays as small as the set of busy accounts.
    if m.len() > 1000 {
        m.retain(|_, s| s.available_permits() < PER_ACCOUNT);
    }
    m.entry(account_id).or_insert_with(|| Arc::new(Semaphore::new(PER_ACCOUNT))).clone()
}

/// Wait for a slot: the account's first (so one busy account waits on itself), then the overall one.
pub async fn slot(account_id: Uuid) -> Slot {
    let account = account_gate(account_id).acquire_owned().await.expect("never closed");
    let all = ALL.clone().acquire_owned().await.expect("never closed");
    Slot { _account: account, _all: all }
}

/// Calls in flight for an account (tests).
#[doc(hidden)]
pub fn in_flight(account_id: Uuid) -> usize {
    PER_ACCOUNT - account_gate(account_id).available_permits()
}

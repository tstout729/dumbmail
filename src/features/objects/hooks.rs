//! Pause points for tests that put a competing step in a race's gap. Off unless a test installs a
//! gate for an account; then code that reaches a named point for that account reports it and waits
//! for the test to let it go. Costs a lock and a map lookup per point otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

type Gate = (mpsc::UnboundedSender<&'static str>, Arc<Notify>);

static GATES: Lazy<Mutex<HashMap<Uuid, Gate>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Install a gate for `account`: every point reached is sent on the receiver, and waits until the
/// returned `Notify` is notified once for it.
#[doc(hidden)]
pub fn install(account: Uuid) -> (mpsc::UnboundedReceiver<&'static str>, Arc<Notify>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let go = Arc::new(Notify::new());
    GATES.lock().unwrap_or_else(|e| e.into_inner()).insert(account, (tx, go.clone()));
    (rx, go)
}

#[doc(hidden)]
pub fn remove(account: Uuid) {
    GATES.lock().unwrap_or_else(|e| e.into_inner()).remove(&account);
}

/// A named point in a race window (does nothing unless a test installed a gate for the account).
pub async fn pause(account: Uuid, point: &'static str) {
    let gate = GATES.lock().unwrap_or_else(|e| e.into_inner()).get(&account).cloned();
    if let Some((tx, go)) = gate {
        if tx.send(point).is_ok() {
            go.notified().await;
        }
    }
}

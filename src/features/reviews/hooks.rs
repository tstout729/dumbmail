//! Test hooks: hold or fail a step at a named point, so a test can land a competing change (an
//! opt-out, a review, a second worker) exactly there, or simulate a crash. Nothing is set outside
//! tests; when nothing is set a point costs one map lookup and no SQL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tokio::sync::Semaphore;
use uuid::Uuid;

#[doc(hidden)]
pub struct Pause {
    arrived: Semaphore,
    go: Semaphore,
}

enum Hook {
    Pause(Arc<Pause>),
    /// Let this many passes through, then fail the next one once (a crash at that point).
    FailAfter(u32),
}

static HOOKS: Lazy<Mutex<HashMap<(&'static str, Uuid), Hook>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Hold every pass through `point` for `key` (an account id) until [`Pause::release`].
#[doc(hidden)]
pub fn hold(point: &'static str, key: Uuid) -> Arc<Pause> {
    let p = Arc::new(Pause { arrived: Semaphore::new(0), go: Semaphore::new(0) });
    if let Ok(mut m) = HOOKS.lock() {
        m.insert((point, key), Hook::Pause(p.clone()));
    }
    p
}

/// Let `after` passes through `point` for `key` go, then make the next one fail once, as if the
/// worker died there.
#[doc(hidden)]
pub fn fail(point: &'static str, key: Uuid, after: u32) {
    if let Ok(mut m) = HOOKS.lock() {
        m.insert((point, key), Hook::FailAfter(after));
    }
}

impl Pause {
    /// Wait until something is being held.
    pub async fn arrived(&self) {
        if let Ok(permit) = self.arrived.acquire().await {
            permit.forget();
        }
    }

    /// Let held passes go, and stop holding.
    pub fn release(&self, point: &'static str, key: Uuid) {
        if let Ok(mut m) = HOOKS.lock() {
            m.remove(&(point, key));
        }
        self.go.add_permits(1000);
    }
}

pub(crate) async fn point(point: &'static str, key: Uuid) -> anyhow::Result<()> {
    let pause = {
        let Ok(mut m) = HOOKS.lock() else { return Ok(()) };
        match m.get_mut(&(point, key)) {
            None => return Ok(()),
            Some(Hook::FailAfter(n)) => {
                if *n > 0 {
                    *n -= 1;
                    return Ok(());
                }
                m.remove(&(point, key));
                return Err(anyhow::anyhow!("test hook: the worker stopped at {point}"));
            }
            Some(Hook::Pause(p)) => p.clone(),
        }
    };
    pause.arrived.add_permits(1);
    if let Ok(permit) = pause.go.acquire().await {
        permit.forget();
    }
    Ok(())
}

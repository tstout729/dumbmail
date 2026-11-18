//! Limits in front of the click lookup, so guessing at `/s/` costs almost nothing: per address, at
//! most [`MISSES_PER_MINUTE`] lookups that end in "not found" per clock minute, after which that
//! address gets 429 for the rest of the minute without a query; and at most [`MAX_CONCURRENT`]
//! lookups at once across the server, beyond which it answers 503. Valid clicks never count against
//! an address. In-process: each server keeps its own counts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MISSES_PER_MINUTE: u32 = 30;
/// Requests whose address the proxy didn't tell us share one, larger, allowance.
pub const UNKNOWN_MISSES_PER_MINUTE: u32 = 600;
pub const MAX_CONCURRENT: usize = 32;
/// Addresses remembered before old minutes are dropped.
const MAX_ADDRESSES: usize = 50_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// This address missed too often this minute (429).
    TooMany,
    /// Too many lookups at once (503).
    Busy,
}

pub struct Limiter {
    /// Address → (minute, misses in it).
    misses: Mutex<HashMap<String, (u64, u32)>>,
    slots: Arc<Semaphore>,
    per_minute: u32,
    unknown_per_minute: u32,
}

pub static LIMITER: Lazy<Limiter> = Lazy::new(|| Limiter::new(MISSES_PER_MINUTE, UNKNOWN_MISSES_PER_MINUTE, MAX_CONCURRENT));

fn minute() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0)
}

impl Limiter {
    pub fn new(per_minute: u32, unknown_per_minute: u32, concurrent: usize) -> Self {
        Limiter { misses: Mutex::new(HashMap::new()), slots: Arc::new(Semaphore::new(concurrent)), per_minute, unknown_per_minute }
    }

    fn key(ip: Option<&str>) -> String {
        ip.unwrap_or("unknown").to_string()
    }

    fn allowance(&self, ip: Option<&str>) -> u32 {
        if ip.is_some() { self.per_minute } else { self.unknown_per_minute }
    }

    /// A slot for one lookup, unless this address is over its misses or the server is busy.
    pub fn admit(&self, ip: Option<&str>) -> Result<OwnedSemaphorePermit, Refused> {
        let now = minute();
        let over = self.misses.lock().unwrap_or_else(|e| e.into_inner()).get(&Self::key(ip)).is_some_and(|&(m, n)| m == now && n >= self.allowance(ip));
        if over {
            return Err(Refused::TooMany);
        }
        self.slots.clone().try_acquire_owned().map_err(|_| Refused::Busy)
    }

    /// Count a lookup from `ip` that ended in "not found".
    pub fn missed(&self, ip: Option<&str>) {
        let now = minute();
        let mut m = self.misses.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MAX_ADDRESSES {
            m.retain(|_, (minute, _)| *minute == now);
        }
        let e = m.entry(Self::key(ip)).or_insert((now, 0));
        if e.0 != now {
            *e = (now, 0);
        }
        e.1 = e.1.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misses_block_one_address_and_slots_cap_everyone() {
        let l = Limiter::new(2, 5, 1);
        for _ in 0..2 {
            drop(l.admit(Some("1.1.1.1")).unwrap());
            l.missed(Some("1.1.1.1"));
        }
        assert_eq!(l.admit(Some("1.1.1.1")).unwrap_err(), Refused::TooMany);
        // Another address is unaffected; a request with no address has its own allowance.
        let held = l.admit(Some("2.2.2.2")).unwrap();
        // The one slot is taken: everyone else is told the server is busy.
        assert_eq!(l.admit(Some("3.3.3.3")).unwrap_err(), Refused::Busy);
        assert_eq!(l.admit(None).unwrap_err(), Refused::Busy);
        drop(held);
        assert!(l.admit(None).is_ok());
    }
}

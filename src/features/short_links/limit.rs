//! Limits in front of the click lookup, so guessing at `/s/` costs almost nothing: per address, at
//! most [`MISSES_PER_MINUTE`] lookups that end in "not found" per clock minute, after which that
//! address gets 429 for the rest of the minute without a query; and at most [`MAX_CONCURRENT`]
//! lookups at once across the server, beyond which it answers 503. A lookup counts against its
//! address when it starts and gives the count back if it was a valid click, so valid clicks never
//! count and lookups at the same moment can't get past the allowance. In-process: each server keeps
//! its own counts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MISSES_PER_MINUTE: u32 = 30;
/// Requests whose address the proxy didn't tell us share one, larger, allowance.
pub const UNKNOWN_MISSES_PER_MINUTE: u32 = 600;
pub const MAX_CONCURRENT: usize = 32;
/// Addresses tracked in one minute; past that, new addresses share [`OVERFLOW_BUCKETS`] overflow
/// allowances, picked by a hash of the address (memory stays bounded, the limit still holds, and one
/// visitor can't use up everyone's).
const MAX_ADDRESSES: usize = 50_000;
const OVERFLOW_BUCKETS: u64 = 256;

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// This address missed too often this minute (429).
    TooMany,
    /// Too many lookups at once (503).
    Busy,
}

pub struct Limiter {
    counts: Mutex<Counts>,
    slots: Arc<Semaphore>,
    per_minute: u32,
    unknown_per_minute: u32,
}

/// Lookups per address in the current clock minute that haven't (yet) turned out to be valid.
struct Counts {
    minute: u64,
    by_address: HashMap<String, u32>,
}

/// A lookup let through: holds one of the server's slots, and one of its address's allowance until
/// [`Admitted::valid`] gives it back.
pub struct Admitted {
    _slot: OwnedSemaphorePermit,
    key: Option<(String, u64)>,
}

/// Keyed per process, so which addresses share an overflow allowance can't be chosen from outside.
static OVERFLOW_HASH: Lazy<std::collections::hash_map::RandomState> = Lazy::new(std::collections::hash_map::RandomState::new);

pub static LIMITER: Lazy<Limiter> = Lazy::new(|| Limiter::new(MISSES_PER_MINUTE, UNKNOWN_MISSES_PER_MINUTE, MAX_CONCURRENT));

fn minute() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0)
}

/// One key per visitor: an IPv6 address by its /64 (one household or server can hold a whole /64),
/// an IPv4 address written as IPv6 by its IPv4 address.
fn key(ip: Option<&str>) -> String {
    match ip.and_then(|i| i.parse::<std::net::IpAddr>().ok()) {
        Some(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_some() => v6.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_default(),
        Some(std::net::IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        Some(v4) => v4.to_string(),
        None => "unknown".to_string(),
    }
}

impl Limiter {
    pub fn new(per_minute: u32, unknown_per_minute: u32, concurrent: usize) -> Self {
        Limiter { counts: Mutex::new(Counts { minute: 0, by_address: HashMap::new() }), slots: Arc::new(Semaphore::new(concurrent)), per_minute, unknown_per_minute }
    }

    /// Let one lookup through, unless this address has used its allowance this minute or the
    /// server is busy. The lookup counts against the address from now on, so lookups running at the
    /// same time can't slip past the allowance; a valid click gives its count back.
    pub fn admit(&self, ip: Option<&str>) -> Result<Admitted, Refused> {
        let now = minute();
        let mut k = key(ip);
        let mut allowance = if ip.is_some() { self.per_minute } else { self.unknown_per_minute };
        {
            let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
            if c.minute != now {
                // Only the current minute matters.
                c.minute = now;
                c.by_address.clear();
            }
            if c.by_address.len() >= MAX_ADDRESSES && !c.by_address.contains_key(&k) {
                use std::hash::{BuildHasher, Hasher};
                let mut hasher = OVERFLOW_HASH.build_hasher();
                hasher.write(k.as_bytes());
                k = format!("overflow:{}", hasher.finish() % OVERFLOW_BUCKETS);
                // Each bucket gets one address's allowance, so the buckets together allow no more
                // than that many addresses would.
                allowance = self.per_minute;
            }
            let n = c.by_address.entry(k.clone()).or_insert(0);
            if *n >= allowance {
                return Err(Refused::TooMany);
            }
            *n += 1;
        }
        let key = Some((k, now));
        match self.slots.clone().try_acquire_owned() {
            Ok(slot) => Ok(Admitted { _slot: slot, key }),
            Err(_) => {
                self.give_back(key.as_ref());
                Err(Refused::Busy)
            }
        }
    }

    fn give_back(&self, key: Option<&(String, u64)>) {
        let Some((k, at)) = key else { return };
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if c.minute == *at {
            if let Some(n) = c.by_address.get_mut(k) {
                *n = n.saturating_sub(1);
                // Valid clicks leave no entry behind.
                if *n == 0 {
                    c.by_address.remove(k);
                }
            }
        }
    }
}

impl Admitted {
    /// The lookup found a real link: it doesn't count against the address.
    pub fn valid(self, limiter: &Limiter) {
        limiter.give_back(self.key.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misses_block_one_address_and_slots_cap_everyone() {
        let l = Limiter::new(2, 5, 2);
        // Lookups count when they start, so two at once use up an allowance of two.
        let a = l.admit(Some("1.1.1.1")).unwrap();
        let b = l.admit(Some("1.1.1.1")).unwrap();
        assert_eq!(l.admit(Some("1.1.1.1")).err(), Some(Refused::TooMany));
        // A valid click gives its count back; a miss keeps it.
        a.valid(&l);
        drop(b);
        let c = l.admit(Some("1.1.1.1")).unwrap();
        assert_eq!(l.admit(Some("1.1.1.1")).err(), Some(Refused::TooMany));
        // Another address is unaffected, until the server's slots are all taken.
        let d = l.admit(Some("2.2.2.2")).unwrap();
        assert_eq!(l.admit(Some("3.3.3.3")).err(), Some(Refused::Busy));
        assert_eq!(l.admit(None).err(), Some(Refused::Busy));
        drop((c, d));
        assert!(l.admit(None).is_ok());
        // A refusal for busy doesn't use up the address's allowance.
        let busy = Limiter::new(1, 1, 0);
        assert_eq!(busy.admit(Some("4.4.4.4")).err(), Some(Refused::Busy));
        assert_eq!(busy.admit(Some("4.4.4.4")).err(), Some(Refused::Busy), "still busy, not too many");
    }

    #[test]
    fn past_the_address_cap_new_addresses_share_one_allowance() {
        let l = Limiter::new(2, 50, 100);
        // Counts reset each clock minute: stay clear of a minute's last seconds.
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        if secs % 60 >= 57 {
            std::thread::sleep(std::time::Duration::from_secs(4));
        }
        {
            let mut c = l.counts.lock().unwrap();
            c.minute = minute();
            c.by_address = (0..MAX_ADDRESSES).map(|i| (format!("10.0.{}.{}", i / 256, i % 256), 0)).collect();
        }
        // One new address uses up its overflow bucket's allowance (one address's, 2 here), not everyone's.
        assert!(l.admit(Some("203.0.113.1")).is_ok());
        assert!(l.admit(Some("203.0.113.1")).is_ok());
        assert_eq!(l.admit(Some("203.0.113.1")).err(), Some(Refused::TooMany), "the overflow allowance is limited, not unlimited");
        let others = (2..40).filter(|i| l.admit(Some(&format!("203.0.113.{i}"))).is_ok()).count();
        assert!(others >= 30, "other new addresses mostly land in other buckets: {others}");
        assert!(l.counts.lock().unwrap().by_address.len() <= MAX_ADDRESSES + OVERFLOW_BUCKETS as usize);
        // A valid click frees its address's entry.
        let fresh = Limiter::new(5, 5, 5);
        fresh.admit(Some("192.0.2.1")).unwrap().valid(&fresh);
        assert!(fresh.counts.lock().unwrap().by_address.is_empty());
    }

    #[test]
    fn an_ipv6_visitor_is_one_key_per_64() {
        assert_eq!(key(Some("2001:db8:1:2:aaaa::1")), key(Some("2001:db8:1:2:bbbb::9")));
        assert_ne!(key(Some("2001:db8:1:2::1")), key(Some("2001:db8:1:3::1")));
        assert_eq!(key(Some("198.51.100.7")), "198.51.100.7");
        assert_eq!(key(None), "unknown");
        assert_eq!(key(Some("::ffff:198.51.100.7")), "198.51.100.7", "an IPv4 address written as IPv6 is that address");
    }
}

//! The short path after `/s/`: a target code, the message's reference and an authentication tag,
//! all letters and digits, always [`PATH_LEN`] characters.
//!
//! ```text
//! /s/ Ab12Cd  Ef34Gh  Jk56Lm78Np90
//!     target  message tag
//! ```
//!
//! The tag is HMAC-SHA256 under the server's secret key over the target code, a hash of the
//! destination and the message's full id, so it ties one destination to one message: changing any
//! character, putting one message's tag on another message's reference, or a code later given to
//! another destination (after cleanup freed it) all fail. See docs/SHORT_LINKS.md for why 12 characters is enough.

use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Letters and digits only: safe in a URL and in every phone's link detection, and one GSM-7
/// character each, so a short link never makes a text Unicode.
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub const TARGET_LEN: usize = 6;
pub const REF_LEN: usize = 6;
pub const TAG_LEN: usize = 12;
/// Everything after `/s/`.
pub const PATH_LEN: usize = TARGET_LEN + REF_LEN + TAG_LEN;

/// `n` random characters from [`ALPHABET`] (uniform; the thread RNG is a CSPRNG).
pub fn random(n: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..n).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

pub fn is_code(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The tag for `target`, going to `url`, in message `message_id`.
pub fn tag(secret: &str, target: &str, url: &str, message_id: Uuid) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(b"short-link:v2|");
    mac.update(target.as_bytes());
    mac.update(b"|");
    mac.update(&Sha256::digest(url.as_bytes()));
    mac.update(b"|");
    mac.update(message_id.simple().to_string().as_bytes());
    let digest = mac.finalize().into_bytes();
    // 128 bits of the digest, written in base 62. 62^12 is about 2^71.5, far below 2^128, so the
    // digits are as good as uniform.
    let mut n = u128::from_be_bytes(digest[..16].try_into().expect("16 bytes"));
    let mut out = [0u8; TAG_LEN];
    for c in out.iter_mut() {
        *c = ALPHABET[(n % 62) as usize];
        n /= 62;
    }
    String::from_utf8(out.to_vec()).expect("ascii")
}

/// The path after `/s/` for `target` (going to `url`) in the message with reference `message_ref`
/// and id `message_id`.
pub fn path(secret: &str, target: &str, url: &str, message_ref: &str, message_id: Uuid) -> String {
    format!("{target}{message_ref}{}", tag(secret, target, url, message_id))
}

/// A path split into target code, message reference and tag, if it has the right shape.
pub fn split(path: &str) -> Option<(&str, &str, &str)> {
    if !is_code(path, PATH_LEN) {
        return None;
    }
    Some((&path[..TARGET_LEN], &path[TARGET_LEN..TARGET_LEN + REF_LEN], &path[TARGET_LEN + REF_LEN..]))
}

/// Whether `tag` is the tag for `target`, going to `url`, in message `message_id` (compared in
/// constant time).
pub fn verify(secret: &str, target: &str, url: &str, message_id: Uuid, tag_given: &str) -> bool {
    let expected = tag(secret, target, url, message_id);
    expected.len() == tag_given.len() && expected.bytes().zip(tag_given.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_fixed_length_and_verify() {
        let id = Uuid::new_v4();
        let p = path("k", "Ab12Cd", "https://x.test/a", "Ef34Gh", id);
        assert_eq!(p.len(), PATH_LEN);
        assert!(p.bytes().all(|b| b.is_ascii_alphanumeric()));
        let (t, r, g) = split(&p).unwrap();
        assert_eq!((t, r), ("Ab12Cd", "Ef34Gh"));
        assert!(verify("k", t, "https://x.test/a", id, g));
        assert!(!verify("k", t, "https://x.test/b", id, g), "the destination is covered");
        assert!(!verify("other key", t, "https://x.test/a", id, g));
        assert!(!verify("k", "Ab12Ce", "https://x.test/a", id, g));
        assert!(!verify("k", t, "https://x.test/a", Uuid::new_v4(), g));
        assert!(split(&p[1..]).is_none());
        assert!(split(&format!("{}-", &p[1..])).is_none());
    }

    #[test]
    fn random_codes_use_the_alphabet() {
        let c = random(200);
        assert!(is_code(&c, 200));
        assert_ne!(random(REF_LEN), random(REF_LEN));
    }
}

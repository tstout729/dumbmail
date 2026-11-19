//! What may leave for Anthropic: free text with email addresses and phone numbers replaced, and
//! links reduced to their domain and path with anything that could identify someone replaced.
//! Every check builds its material only from these (see `account`, `imports`, `content`).

use once_cell::sync::Lazy;
use regex::Regex;

static EMAIL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)[a-z0-9._%+\-]+@[a-z0-9\-]+(\.[a-z0-9\-]+)+").unwrap());
/// A run that could be a phone number; kept only if it has 9 to 15 digits (so dates and prices stay).
static PHONE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\+?\d[\d\s().\-]{6,20}\d").unwrap());
static UUIDISH: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}$").unwrap());

/// Free text with email addresses as `[email]` and phone numbers as `[phone]`.
pub fn text(s: &str) -> String {
    let s = EMAIL.replace_all(s, "[email]");
    PHONE
        .replace_all(&s, |c: &regex::Captures<'_>| {
            let digits = c[0].chars().filter(char::is_ascii_digit).count();
            if (9..=15).contains(&digits) { "[phone]".to_string() } else { c[0].to_string() }
        })
        .into_owned()
}

/// A path segment that could identify a person or carry a secret.
fn identifying(seg: &str) -> Option<&'static str> {
    let digits = seg.chars().filter(char::is_ascii_digit).count();
    let letters = seg.chars().filter(char::is_ascii_alphabetic).count();
    if seg.contains('@') || seg.contains("%40") {
        Some(":email")
    } else if UUIDISH.is_match(seg) || (digits >= 4 && letters == 0) || digits >= 9 {
        Some(":id")
    } else if seg.len() >= 16 && digits > 0 && letters > 0 {
        Some(":token")
    } else {
        None
    }
}

/// A link as the model sees it: `host/path`, without the query or fragment, with any path segment
/// that looks like an id, a token, an email address or a phone number replaced.
pub fn link(href: &str) -> String {
    let Ok(u) = url::Url::parse(href.trim()) else { return String::new() };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let path: Vec<String> = u.path_segments().into_iter().flatten().filter(|s| !s.is_empty()).map(|s| identifying(s).map(str::to_string).unwrap_or_else(|| text(s))).take(12).collect();
    if path.is_empty() { host } else { format!("{host}/{}", path.join("/")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_details_are_replaced() {
        assert_eq!(text("Write to jane.doe+x@shop.test or call +1 (503) 555-0123."), "Write to [email] or call [phone].");
        assert_eq!(text("Sale ends 2026-11-30, $1,299.00 off"), "Sale ends 2026-11-30, $1,299.00 off", "dates and prices stay");
        assert_eq!(text("export jane@corp.test.csv"), "export [email]");
    }

    #[test]
    fn links_lose_queries_and_ids() {
        assert_eq!(link("https://Shop.test/orders/12345?token=abc#x"), "shop.test/orders/:id");
        assert_eq!(link("https://shop.test/u/jane@x.test/reset"), "shop.test/u/:email/reset");
        assert_eq!(link("https://shop.test/r/a8F3k2m9Zq7Lp0Xw"), "shop.test/r/:token");
        assert_eq!(link("https://shop.test/c/0b4f7a52-6d55-4b6a-9a1e-3f6f0c2d9e11"), "shop.test/c/:id");
        assert_eq!(link("https://shop.test/fall-collection"), "shop.test/fall-collection");
        assert_eq!(link("https://shop.test/"), "shop.test");
    }
}

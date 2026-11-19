//! What crosses to Claude and back.
//!
//! - **In**: card numbers (Luhn-valid digit runs), government-id-like numbers and passwords or
//!   one-time codes in a shopper's message are replaced with placeholders before the message is sent
//!   to Claude or saved ([`redact_input`]).
//! - **Out**: links survive only when their host is the store's website, its link domain or the
//!   hub, or the exact address appeared in a tool result in this conversation ([`Links`]); the
//!   filter also runs on the stream ([`StreamGuard`]). An answer that names a discount code, a
//!   percentage or a price that no tool result contains is refused ([`unsupported_claim`]).
//! - **History**: when the proof behind order access no longer holds, earlier order results (and
//!   the answers built on them) are dropped from what's replayed ([`drop_order_results`]).

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

// ---------- In ----------

static DIGIT_RUN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d(?:[ -]?\d){12,18}").unwrap());
static US_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap());
static ID_LABEL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(ssn|social security(?: number| no\.?)?|social insurance(?: number)?|sin|passport(?: number| no\.?)?|national id(?: number)?|tax id|tin|driver'?s licen[cs]e(?: number| no\.?)?|id number)(\s*(?:is|was|:|#|=)?\s*)([A-Za-z0-9][A-Za-z0-9 -]{4,18}[A-Za-z0-9])").unwrap()
});
static SECRET_LABEL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(password|passcode|passwd|pwd|pin(?: code| number)?|one[- ]time (?:code|password|passcode)|otp|2fa code|verification code|security code|sign[- ]in code|login code|auth(?:entication)? code)(\s*(?:is|was|:|=)?\s*)(\S+)").unwrap()
});

fn luhn(digits: &[u32]) -> bool {
    let mut sum = 0;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = *d;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum % 10 == 0
}

/// The shopper's message with card numbers, id numbers and passwords or codes replaced.
pub fn redact_input(text: &str) -> String {
    let s = DIGIT_RUN.replace_all(text, |c: &regex::Captures<'_>| {
        let digits: Vec<u32> = c[0].chars().filter_map(|ch| ch.to_digit(10)).collect();
        if (13..=19).contains(&digits.len()) && luhn(&digits) {
            "[card number removed]".to_string()
        } else {
            c[0].to_string()
        }
    });
    let s = US_ID.replace_all(&s, "[ID number removed]");
    let s = ID_LABEL.replace_all(&s, |c: &regex::Captures<'_>| {
        if c[3].chars().any(|ch| ch.is_ascii_digit()) {
            format!("{}{}[ID number removed]", &c[1], &c[2])
        } else {
            c[0].to_string()
        }
    });
    let s = SECRET_LABEL.replace_all(&s, |c: &regex::Captures<'_>| {
        let what = if c[1].to_lowercase().contains("pass") || c[1].eq_ignore_ascii_case("pwd") { "[password removed]" } else { "[code removed]" };
        format!("{}{}{what}", &c[1], &c[2])
    });
    s.into_owned()
}

// ---------- Out: links ----------

static SCHEMED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>"'`]+"#).unwrap());
static BARE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(?:[a-z0-9-]+\.)+[a-z][a-z0-9-]{1,23}\b(?:/[^\s<>\x22'`]*)?").unwrap());
static EMAIL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[a-z0-9._%+-]+@(?:[a-z0-9-]+\.)+[a-z]{2,24}$").unwrap());

/// Where links in an answer may point.
#[derive(Debug, Clone, Default)]
pub struct Links {
    /// Hosts (without `www.`) whose pages and subdomains are allowed.
    pub hosts: Vec<String>,
    /// Exact addresses seen in tool results.
    pub urls: HashSet<String>,
}

fn trim_url(u: &str) -> &str {
    u.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '*', '_'])
}

fn host_of(u: &str) -> Option<String> {
    let with = if u.to_ascii_lowercase().starts_with("http") { u.to_string() } else { format!("https://{u}") };
    url::Url::parse(&with).ok().and_then(|x| x.host_str().map(|h| h.trim_start_matches("www.").trim_end_matches('.').to_ascii_lowercase()))
}

impl Links {
    /// Add every address that appears in `text` (a tool result) to the allowed list.
    pub fn learn(&mut self, text: &str) {
        for m in SCHEMED.find_iter(text) {
            self.urls.insert(trim_url(m.as_str()).to_string());
        }
    }

    fn allowed(&self, raw: &str) -> bool {
        let u = trim_url(raw);
        if self.urls.contains(u) {
            return true;
        }
        let Some(h) = host_of(u) else { return false };
        self.hosts.iter().any(|a| h == *a || h.ends_with(&format!(".{a}")))
    }

    /// `text` with every link that isn't allowed removed.
    pub fn filter(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for (i, token) in split_keep(text) {
            let _ = i;
            out.push_str(&self.filter_token(token));
        }
        out
    }

    fn filter_token(&self, token: &str) -> String {
        if token.trim().is_empty() {
            return token.to_string();
        }
        // Links with a scheme (or www.) are judged whole; the bare-domain pass is for the rest.
        if SCHEMED.is_match(token) {
            return SCHEMED.replace_all(token, |c: &regex::Captures<'_>| self.keep(&c[0])).into_owned();
        }
        // A token that is wholly an email address is the address, not a link; anything else with a
        // host in it (before or after an `@`) is judged as a link.
        let core = token.trim_start_matches(['(', '[', '<', '"', '\'']).trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\'']);
        if EMAIL.is_match(core) {
            return token.to_string();
        }
        BARE.replace_all(token, |c: &regex::Captures<'_>| self.keep(&c[0])).into_owned()
    }

    fn keep(&self, m: &str) -> String {
        if m.contains("(link removed)") || self.allowed(m) {
            return m.to_string();
        }
        let tail = &m[trim_url(m).len()..];
        format!("(link removed){tail}")
    }
}

/// The text split into whitespace and non-whitespace runs, in order.
fn split_keep(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut ws: Option<bool> = None;
    for (i, ch) in text.char_indices() {
        let w = ch.is_whitespace();
        match ws {
            Some(prev) if prev != w => {
                out.push((start, &text[start..i]));
                start = i;
            }
            _ => {}
        }
        ws = Some(w);
    }
    if start < text.len() {
        out.push((start, &text[start..]));
    }
    out
}

/// Filters streamed text: whole words only, so a link split across pieces is checked whole. It
/// stops showing anything more once the text names a price, a percentage or a code, or once the
/// turn has read the shopper's orders ([`StreamGuard::hold`]): the rest goes out only after the
/// answer's checks pass ([`StreamGuard::finish`]).
#[derive(Debug, Default)]
pub struct StreamGuard {
    full: String,
    emitted: usize,
    held: bool,
}

impl StreamGuard {
    /// Add a piece; returns what may be shown now.
    pub fn push(&mut self, links: &Links, piece: &str) -> Option<String> {
        self.full.push_str(piece);
        let cut = self.full.char_indices().filter(|(_, c)| c.is_whitespace()).map(|(i, c)| i + c.len_utf8()).next_back()?;
        self.take(links, cut, false)
    }

    /// Show nothing more until [`StreamGuard::finish`].
    pub fn hold(&mut self) {
        self.held = true;
    }

    /// Everything not shown yet (at the end of the answer, once its checks passed).
    pub fn finish(&mut self, links: &Links) -> Option<String> {
        self.held = false;
        let n = self.full.len();
        self.take(links, n, true)
    }

    fn take(&mut self, links: &Links, cut: usize, last: bool) -> Option<String> {
        if self.held {
            return None;
        }
        let shown = links.filter(&self.full[..cut]);
        if shown.len() <= self.emitted {
            return None;
        }
        let out = shown[self.emitted..].to_string();
        if !last && (PRICE.is_match(&out) || PERCENT.is_match(&out) || CODE.is_match(&out)) {
            self.held = true;
            return None;
        }
        self.emitted = shown.len();
        Some(out)
    }

    /// Forget what was shown (the answer was withdrawn).
    pub fn reset(&mut self) {
        self.full.clear();
        self.emitted = 0;
    }
}

// ---------- Out: claims ----------

static PRICE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:[$€£¥]\s?(\d[\d,]*(?:\.\d{1,2})?)|(\d[\d,]*(?:\.\d{1,2})?)\s?(?:usd|eur|gbp|cad|aud|dollars?|euros?|pounds?)\b)").unwrap()
});
static PERCENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s?(?:%|percent\b|per cent\b)").unwrap());
static CODE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)\b(?:discount|coupon|promo(?:tion(?:al)?)?|voucher|offer|gift)?\s*(?:code|coupon|voucher)\s*(?:is|of|:|=)?\s*["'“‘`*]*([A-Za-z0-9][A-Za-z0-9_-]{3,})"#).unwrap()
});

fn amounts(re: &Regex, text: &str) -> Vec<f64> {
    re.captures_iter(text)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)))
        .filter_map(|m| m.as_str().replace(',', "").parse::<f64>().ok())
        .collect()
}

/// Why an answer can't go out as written: a price, a percentage or a discount code that none of
/// the tool results (`evidence`) contains. None when it's fine.
pub fn unsupported_claim(answer: &str, evidence: &str) -> Option<&'static str> {
    let known_prices = amounts(&PRICE, evidence);
    for p in amounts(&PRICE, answer) {
        if !known_prices.iter().any(|k| (k - p).abs() < 0.005) {
            return Some("a price no tool result gave");
        }
    }
    let known_pct = amounts(&PERCENT, evidence);
    for p in amounts(&PERCENT, answer) {
        if !known_pct.iter().any(|k| (k - p).abs() < 0.005) {
            return Some("a percentage no tool result gave");
        }
    }
    let ev = evidence.to_lowercase();
    for c in CODE.captures_iter(answer) {
        let token = &c[1];
        // A code is written in capitals or with digits; ordinary words after "code" aren't one.
        let looks_like_code = token.chars().any(|ch| ch.is_ascii_digit()) || (token.chars().any(|ch| ch.is_ascii_alphabetic()) && token == token.to_uppercase());
        if looks_like_code && !ev.contains(&token.to_lowercase()) {
            return Some("a discount code no tool result gave");
        }
    }
    None
}

/// Every tool result's text in these messages (what an answer's facts must come from).
pub fn evidence(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages.iter().filter(|m| m["role"] == "user") {
        for b in m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
            if let Some(t) = b["content"].as_str() {
                out.push_str(t);
                out.push('\n');
            }
        }
    }
    out
}

// ---------- History ----------

pub const DROPPED_ORDERS: &str = "Order details from earlier in this chat are no longer available.";
const DROPPED_ANSWER: &str = "[An earlier answer about this shopper's orders was removed.]";

/// Drop earlier `my_orders` results, and the answers that followed them in the same shopper turn,
/// from replayed history. Thinking blocks are removed too (all of them, which the API accepts),
/// since an edited history can't carry them. Returns whether anything changed.
pub fn drop_order_results(messages: &mut [Value]) -> bool {
    let ids: HashSet<String> = messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_use" && b["name"] == "my_orders")
        .filter_map(|b| b["id"].as_str().map(String::from))
        .collect();
    if ids.is_empty() {
        return false;
    }
    let mut tainted = false;
    for m in messages.iter_mut() {
        let role = m["role"].as_str().unwrap_or("").to_string();
        let Some(content) = m["content"].as_array_mut() else { continue };
        if role == "user" {
            if content.iter().any(|b| b["type"] == "text") {
                tainted = false;
            }
            for b in content.iter_mut().filter(|b| b["type"] == "tool_result") {
                if b["tool_use_id"].as_str().is_some_and(|id| ids.contains(id)) {
                    b["content"] = json!(json!({"note": DROPPED_ORDERS}).to_string());
                    tainted = true;
                }
            }
        } else {
            content.retain(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")));
            if tainted {
                for b in content.iter_mut().filter(|b| b["type"] == "text") {
                    b["text"] = json!(DROPPED_ANSWER);
                }
            }
            if content.is_empty() {
                content.push(json!({"type": "text", "text": DROPPED_ANSWER}));
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_numbers_ids_and_secrets_are_replaced() {
        let s = redact_input("My card is 4242 4242 4242 4242, SSN 123-45-6789, password: hunter2 and my verification code is 834201. Order #10421042.");
        assert!(!s.contains("4242") && !s.contains("6789") && !s.contains("hunter2") && !s.contains("834201"), "{s}");
        assert!(s.contains("[card number removed]") && s.contains("[ID number removed]") && s.contains("password: [password removed]") && s.contains("[code removed]"));
        assert!(s.contains("#10421042"), "order numbers stay");
        // Not a valid card number (fails Luhn): kept.
        assert!(redact_input("Tracking 1234567890123").contains("1234567890123"));
        assert!(redact_input("passport number X1234567").contains("[ID number removed]"));
        assert_eq!(redact_input("Do you ship to Canada?"), "Do you ship to Canada?");
    }

    #[test]
    fn only_the_stores_links_and_tool_links_survive() {
        let mut l = Links { hosts: vec!["acme.test".into(), "links.acme.test".into()], urls: HashSet::new() };
        l.learn(r#"{"tracking_url":"https://www.ups.com/track?tracknum=1Z9"}"#);
        let out = l.filter("See https://acme.test/returns, https://shop.acme.test/x and https://www.ups.com/track?tracknum=1Z9. Not https://evil.example/p or www.evil.com or evil.com/free or [click](https://phish.io/a). Mail help@gmail.com.");
        assert!(out.contains("https://acme.test/returns,") && out.contains("https://shop.acme.test/x") && out.contains("https://www.ups.com/track?tracknum=1Z9."));
        assert!(!out.contains("evil") && !out.contains("phish"), "{out}");
        assert!(out.contains("help@gmail.com"));
        assert!(!l.filter("https://www.ups.com/track?tracknum=OTHER").contains("ups.com"), "only the exact address from the tool result");
    }

    #[test]
    fn the_stream_never_shows_a_disallowed_link_even_split_in_pieces() {
        let l = Links { hosts: vec!["acme.test".into()], urls: HashSet::new() };
        let mut g = StreamGuard::default();
        let mut shown = String::new();
        for piece in ["Go to htt", "ps://ev", "il.example/x now or ", "https://acme.", "test/a", " ok"] {
            if let Some(s) = g.push(&l, piece) {
                shown.push_str(&s);
            }
        }
        if let Some(s) = g.finish(&l) {
            shown.push_str(&s);
        }
        assert_eq!(shown, "Go to (link removed) now or https://acme.test/a ok");
    }

    #[test]
    fn the_stream_holds_back_prices_codes_and_anything_after_orders() {
        let l = Links::default();
        let mut g = StreamGuard::default();
        assert_eq!(g.push(&l, "It costs ").as_deref(), Some("It costs "));
        assert_eq!(g.push(&l, "$59 today "), None, "a price waits for the checks");
        assert_eq!(g.push(&l, "only "), None, "and so does everything after it");
        assert_eq!(g.finish(&l).as_deref(), Some("$59 today only "));
        let mut g = StreamGuard::default();
        g.hold();
        assert_eq!(g.push(&l, "Order #A100 shipped "), None);
    }

    #[test]
    fn a_host_hidden_next_to_an_at_sign_is_still_a_link() {
        let l = Links { hosts: vec!["acme.test".into()], urls: HashSet::new() };
        assert!(!l.filter("go evil.com/x@a now").contains("evil.com"));
        assert!(!l.filter("go me@evil.co/path").contains("evil.co/path"));
        assert!(!l.filter("visit evil.shoes today").contains("evil.shoes"), "any top-level domain");
        assert!(l.filter("mail help@gmail.com.").contains("help@gmail.com"));
        assert!(l.filter("shop at acme.test/new").contains("acme.test/new"));
    }

    #[test]
    fn prices_percentages_and_codes_must_come_from_tool_results() {
        let ev = r#"{"products":[{"title":"Wool jacket","price":"$84.00"}],"store_answers":[{"answer":"Free shipping over $75. Use code WELCOME10 for 10% off your first order."}]}"#;
        assert_eq!(unsupported_claim("The Wool jacket is $84 and shipping is free over $75.", ev), None);
        assert_eq!(unsupported_claim("Use code WELCOME10 for 10% off.", ev), None);
        assert!(unsupported_claim("It's $59 today.", ev).is_some());
        assert!(unsupported_claim("I can give you 50% off.", ev).is_some());
        assert!(unsupported_claim("Use discount code FREE50 at checkout.", ev).is_some());
        assert!(unsupported_claim("Use promo code: save20", ev).is_some());
        assert_eq!(unsupported_claim("Your code below is in the email.", ev), None, "an ordinary word after 'code'");
    }

    #[test]
    fn order_results_and_their_answers_are_dropped_with_all_thinking() {
        let mut m = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "Where is my order?"}]}),
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "s"}, {"type": "tool_use", "id": "t1", "name": "my_orders", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "{\"orders\":[{\"number\":\"#A100\"}]}"}]}),
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "s2"}, {"type": "text", "text": "Order #A100 is in transit."}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "Do you ship to Canada?"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Yes."}]}),
        ];
        assert!(drop_order_results(&mut m));
        let s = Value::Array(m.clone()).to_string();
        assert!(!s.contains("A100") && !s.contains("signature"), "{s}");
        assert_eq!(m[5]["content"][0]["text"], "Yes.");
        let mut none = vec![json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})];
        assert!(!drop_order_results(&mut none));
    }
}

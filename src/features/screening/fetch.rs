//! What the checks look up outside: the account's website (through `features::content::net`:
//! public addresses only, a short deadline, bytes capped), a domain's registration date (RDAP, the
//! same way) and address domains' mail servers (`domains::lookup`).
//!
//! Tests install canned answers with [`testing`]; while any are installed, nothing goes to the
//! network from here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Method;

use crate::features::content::net::{self, NetError};
use crate::state::AppState;

const PAGE_BYTES: usize = 512 * 1024;
const PAGE_TIMEOUT: Duration = Duration::from_secs(8);
/// The most website text that goes into a check.
pub const PAGE_TEXT_CHARS: usize = 4000;

#[derive(Debug, Clone, Default)]
struct Canned {
    pages: HashMap<String, Option<(String, String)>>,
    registered: HashMap<String, DateTime<Utc>>,
    mx: HashMap<String, bool>,
}

static CANNED: Lazy<Mutex<Option<Canned>>> = Lazy::new(|| Mutex::new(None));

/// Canned answers for tests (never used by the server).
#[doc(hidden)]
pub mod testing {
    use super::*;

    fn with(f: impl FnOnce(&mut Canned)) {
        if let Ok(mut c) = CANNED.lock() {
            f(c.get_or_insert_with(Canned::default));
        }
    }

    /// Stop looking anything up on the network (unknown sites don't answer, unknown domains have
    /// no known age, unknown mail domains have a mail server).
    pub fn offline() {
        with(|_| {});
    }

    /// The home page of `host`: Some((title, text)), or None for a site that doesn't answer.
    pub fn page(host: &str, page: Option<(&str, &str)>) {
        with(|c| {
            c.pages.insert(host.to_ascii_lowercase(), page.map(|(t, x)| (t.to_string(), x.to_string())));
        });
    }

    pub fn registered(domain: &str, at: DateTime<Utc>) {
        with(|c| {
            c.registered.insert(domain.to_ascii_lowercase(), at);
        });
    }

    pub fn mail_server(domain: &str, has: bool) {
        with(|c| {
            c.mx.insert(domain.to_ascii_lowercase(), has);
        });
    }
}

fn canned() -> Option<Canned> {
    CANNED.lock().ok().and_then(|c| c.clone())
}

/// A fetched home page, reduced to its title, description and visible text.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Page {
    pub title: String,
    pub description: String,
    pub text: String,
}

/// Why a website couldn't be read.
#[derive(Debug, Clone, PartialEq)]
pub enum SiteError {
    NotAnswering(&'static str),
}

static TITLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
static DESC: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<meta\s[^>]*name\s*=\s*["']description["'][^>]*content\s*=\s*["']([^"']*)["']"#).unwrap());
static DROP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<(script|style|noscript|svg|template)[^>]*>.*?</(script|style|noscript|svg|template)>|<!--.*?-->").unwrap());
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());

fn entities(s: &str) -> String {
    s.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

fn cap(s: &str, n: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    t.chars().take(n).collect()
}

/// HTML reduced to its visible text, whitespace collapsed, at most `n` characters.
pub fn visible_text(html: &str, n: usize) -> String {
    let no_code = DROP.replace_all(html, " ");
    cap(&entities(&TAG.replace_all(&no_code, " ")), n)
}

pub fn read_page(html: &str) -> Page {
    let title = TITLE.captures(html).map(|c| cap(&entities(&TAG.replace_all(&c[1], " ")), 200)).unwrap_or_default();
    let description = DESC.captures(html).map(|c| cap(&entities(&c[1]), 400)).unwrap_or_default();
    let body = html.find("<body").map(|i| &html[i..]).unwrap_or(html);
    Page { title, description, text: visible_text(body, PAGE_TEXT_CHARS) }
}

/// The host of a website the customer typed (`acme.com`, `https://www.acme.com/shop`).
pub fn website_host(website: &str) -> Option<String> {
    let w = website.trim();
    if w.is_empty() {
        return None;
    }
    let with_scheme = if w.contains("://") { w.to_string() } else { format!("https://{w}") };
    url::Url::parse(&with_scheme).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())).filter(|h| h.contains('.'))
}

/// The account's home page. A redirect is followed once, only within the same site.
pub async fn website(host: &str) -> Result<Page, SiteError> {
    if let Some(c) = canned() {
        return match c.pages.get(host) {
            Some(Some((title, text))) => Ok(Page { title: title.clone(), description: String::new(), text: text.chars().take(PAGE_TEXT_CHARS).collect() }),
            _ => Err(SiteError::NotAnswering("didn't answer")),
        };
    }
    let mut url = format!("https://{host}/");
    for hop in 0..2 {
        let r = match net::request(Method::GET, &url, PAGE_BYTES, PAGE_TIMEOUT).await {
            Ok(r) => r,
            Err(NetError::NoSuchDomain) => return Err(SiteError::NotAnswering("doesn't exist")),
            Err(e) => return Err(SiteError::NotAnswering(e.describe())),
        };
        if (300..400).contains(&r.status) && hop == 0 {
            let next = r.location.as_deref().and_then(|l| url::Url::parse(&url).ok()?.join(l).ok());
            match next {
                Some(n) if n.scheme() == "https" && n.host_str().is_some_and(|h| super::rules::registrable(h) == super::rules::registrable(host)) => {
                    url = n.to_string();
                    continue;
                }
                _ => return Err(SiteError::NotAnswering("sends visitors to another site")),
            }
        }
        if !(200..300).contains(&r.status) {
            return Err(SiteError::NotAnswering("answered with an error"));
        }
        return Ok(read_page(&String::from_utf8_lossy(&r.body)));
    }
    Err(SiteError::NotAnswering("redirects too many times"))
}

/// When a domain was registered, from RDAP (rdap.org, one redirect to the registry's server).
/// None when the lookup fails (never a strike against anyone).
pub async fn registered_at(domain: &str) -> Option<DateTime<Utc>> {
    let domain = super::rules::registrable(domain);
    if let Some(c) = canned() {
        return c.registered.get(&domain).copied();
    }
    let mut url = format!("https://rdap.org/domain/{domain}");
    for _ in 0..2 {
        let r = net::request(Method::GET, &url, 256 * 1024, Duration::from_secs(6)).await.ok()?;
        if (300..400).contains(&r.status) {
            let l = r.location?;
            if !l.starts_with("https://") {
                return None;
            }
            url = l;
            continue;
        }
        if r.status != 200 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&r.body).ok()?;
        return v["events"].as_array()?.iter().find(|e| e["eventAction"] == "registration").and_then(|e| e["eventDate"].as_str()).and_then(|d| DateTime::parse_from_rfc3339(d).ok()).map(|d| d.with_timezone(&Utc));
    }
    None
}

/// Whether a mail domain has a mail server (an MX record, or an address to fall back on).
/// None when the lookup didn't answer.
pub async fn has_mail_server(state: &AppState, domain: &str) -> Option<bool> {
    if let Some(c) = canned() {
        return Some(c.mx.get(domain).copied().unwrap_or(true));
    }
    let mx = crate::domains::lookup(state, domain, "MX").await;
    if !mx.is_empty() {
        // A "null MX" (`0 .`) says the domain takes no mail.
        return Some(mx.iter().any(|r| r.split_whitespace().nth(1).is_some_and(|h| !h.is_empty() && h != ".")));
    }
    let a = crate::domains::lookup(state, domain, "A").await;
    Some(!a.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_reduced_to_text() {
        let p = read_page("<html><head><title>Acme &amp; Co</title><meta name=\"description\" content=\"Soap\"><style>b{}</style></head><body><script>x()</script><h1>Hand made</h1><p>soap</p></body></html>");
        assert_eq!(p.title, "Acme & Co");
        assert_eq!(p.description, "Soap");
        assert_eq!(p.text, "Hand made soap");
        assert_eq!(website_host("Acme.com"), Some("acme.com".into()));
        assert_eq!(website_host("https://www.acme.com/shop"), Some("www.acme.com".into()));
        assert_eq!(website_host("nope"), None);
    }
}

//! The fixed rules: pure functions over names and numbers, each giving reasons a person can read.
//! The thresholds are the constants below (docs/SCREENING.md lists them).

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;

use super::Verdict;

// ---------- Imports ----------

/// Block when more than this share of addresses are shared inboxes (info@, sales@).
pub const IMPORT_ROLE_BLOCK: f64 = 0.15;
/// Block when more than this share of addresses are on domains with no mail server.
pub const IMPORT_NO_MX_BLOCK: f64 = 0.10;
/// Review above these shares.
pub const IMPORT_ROLE_REVIEW: f64 = 0.05;
pub const IMPORT_NO_MX_REVIEW: f64 = 0.03;
pub const IMPORT_TYPO_REVIEW: f64 = 0.03;
/// A block on mail servers needs at least this share of the file's addresses checked.
pub const IMPORT_MX_COVERAGE_FOR_BLOCK: f64 = 0.80;
/// Shares are judged only once a file has this many addresses (one bad row in a file of ten means nothing).
pub const IMPORT_MIN_ROWS: u64 = 20;

/// Words in a file or column name that say where a bought or scraped list came from.
pub const LIST_SOURCE_WORDS: &[&str] = &[
    "purchased", "bought", "scraped", "leads", "lead list", "prospects", "apollo", "zoominfo", "seamless", "lusha", "rocketreach", "hunter", "snov",
    "linkedin", "sales navigator", "data axle", "infousa",
];

/// Lower-case words of a name: letters and digits, split at everything else and at camelCase.
pub fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            cur.extend(c.to_lowercase());
        } else {
            prev_lower = false;
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The list-source words found in a name, as whole words (two-word terms also run together).
pub fn list_source_terms(name: &str) -> Vec<&'static str> {
    let w = words(name);
    LIST_SOURCE_WORDS
        .iter()
        .copied()
        .filter(|term| {
            let parts: Vec<&str> = term.split(' ').collect();
            let joined = parts.concat();
            // Whole words, or two or three words run together (ZoomInfo, LeadList, Sales Navigator).
            w.contains(&joined) || (2..=3).any(|n| w.windows(n).any(|win| win.concat() == joined))
        })
        .collect()
}

/// Column sets typical of prospect lists: a job title, a company size and a LinkedIn or company address.
pub fn prospect_columns(headers: &[String]) -> bool {
    let has = |keys: &[&str]| {
        headers.iter().any(|h| {
            let w = words(h).join(" ");
            keys.iter().any(|k| w == *k || w.contains(k))
        })
    };
    has(&["job title", "title", "position", "role", "seniority"])
        && has(&["company size", "employees", "employee count", "headcount", "number of employees", "staff count"])
        && has(&["linkedin", "company url", "company website", "company domain", "website", "domain"])
}

/// Address domains that are obvious typos of the big mailbox providers.
pub const TYPO_DOMAINS: &[&str] = &[
    "gmial.com", "gmai.com", "gamil.com", "gnail.com", "gmail.co", "gmail.con", "gmaill.com", "gmal.com", "hotmial.com", "hotmai.com", "hotmal.com", "hotmail.co",
    "hotmail.con", "hotmil.com", "yaho.com", "yahooo.com", "yahoo.co", "yahoo.con", "yhoo.com", "outlok.com", "outloo.com", "outlook.co", "outlook.con", "iclod.com",
    "icloud.co", "icoud.com", "aol.co", "aoll.com",
];

/// Consumer mailbox domains: they have mail servers (no lookup needed) and aren't a company's own.
pub const CONSUMER_DOMAINS: &[&str] = &[
    "gmail.com", "googlemail.com", "yahoo.com", "ymail.com", "hotmail.com", "outlook.com", "live.com", "msn.com", "icloud.com", "me.com", "mac.com", "aol.com",
    "proton.me", "protonmail.com", "gmx.com", "gmx.de", "gmx.net", "web.de", "mail.com", "zoho.com", "yandex.com", "yandex.ru", "mail.ru", "comcast.net",
    "verizon.net", "att.net", "sbcglobal.net", "bellsouth.net", "cox.net", "charter.net", "hotmail.co.uk", "yahoo.co.uk", "btinternet.com", "orange.fr",
    "free.fr", "laposte.net", "libero.it", "t-online.de", "qq.com", "163.com", "126.com", "naver.com", "fastmail.com", "hey.com", "rocketmail.com",
];

pub fn is_consumer(domain: &str) -> bool {
    CONSUMER_DOMAINS.contains(&domain)
}

/// What an import's rows add up to (no addresses, names or rows).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ImportStats {
    pub rows: u64,
    /// Rows with an address that reads as one.
    pub with_address: u64,
    pub no_address: u64,
    pub role: u64,
    pub disposable: u64,
    pub typo: u64,
    pub consumer: u64,
    pub company: u64,
    /// Rows whose domain was looked up and has no mail server, of `mx_checked` rows looked up.
    pub no_mx: u64,
    pub mx_checked: u64,
    /// The most common address domains, with counts.
    pub top_domains: Vec<(String, u64)>,
    pub distinct_domains: u64,
    /// Columns that look like consent or sign-up dates.
    pub consent_columns: Vec<String>,
}

fn share(n: u64, of: u64) -> f64 {
    if of == 0 { 0.0 } else { n as f64 / of as f64 }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

/// The rules' verdict on an import, with reasons.
pub fn judge_import(file_name: Option<&str>, headers: &[String], s: &ImportStats) -> (Verdict, Vec<String>) {
    let mut block = Vec::new();
    let mut review = Vec::new();
    if let Some(f) = file_name {
        let t = list_source_terms(f);
        if !t.is_empty() {
            block.push(format!("The file name suggests a bought or scraped list (\"{}\"). Only import people who signed up with you.", t.join("\", \"")));
        }
    }
    let mut header_terms: Vec<&str> = headers.iter().flat_map(|h| list_source_terms(h)).collect();
    header_terms.sort();
    header_terms.dedup();
    if !header_terms.is_empty() {
        block.push(format!("A column name suggests a bought or scraped list (\"{}\"). Only import people who signed up with you.", header_terms.join("\", \"")));
    }
    if s.with_address >= IMPORT_MIN_ROWS {
        let role = share(s.role, s.with_address);
        if role > IMPORT_ROLE_BLOCK {
            block.push(format!("{} of the addresses are shared inboxes like info@ or sales@ (more than {} is typical of bought lists).", pct(role), pct(IMPORT_ROLE_BLOCK)));
        } else if role > IMPORT_ROLE_REVIEW {
            review.push(format!("{} of the addresses are shared inboxes like info@ or sales@.", pct(role)));
        }
        // Over the addresses whose domain was actually checked (a lookup that failed or timed out is
        // unknown, never "no mail server"); a block needs most of the file checked.
        if s.mx_checked >= IMPORT_MIN_ROWS {
            let no_mx = share(s.no_mx, s.mx_checked);
            let coverage = share(s.mx_checked, s.with_address);
            let said = format!("{} of the addresses we could check ({} of the file) are on domains that can't receive email.", pct(no_mx), pct(coverage));
            if no_mx > IMPORT_NO_MX_BLOCK && coverage >= IMPORT_MX_COVERAGE_FOR_BLOCK {
                block.push(said);
            } else if no_mx > IMPORT_NO_MX_REVIEW {
                review.push(said);
            }
        }
        let typo = share(s.typo, s.with_address);
        if typo > IMPORT_TYPO_REVIEW {
            review.push(format!("{} of the addresses have a misspelled domain (like gmial.com).", pct(typo)));
        }
    }
    if prospect_columns(headers) {
        review.push("The columns (job title, company size, company links) look like a sales prospect list.".into());
    }
    if !block.is_empty() {
        (Verdict::Block, block)
    } else if !review.is_empty() {
        (Verdict::Review, review)
    } else {
        (Verdict::Pass, Vec::new())
    }
}

// ---------- Domains ----------

/// Brands phishers use most, with the domains each really owns. `common`: the name is also an
/// ordinary word (apple, chase), so it counts only with a phishing word or a lookalike spelling.
pub struct Brand {
    pub name: &'static str,
    pub domains: &'static [&'static str],
    pub common: bool,
}

const fn b(name: &'static str, domains: &'static [&'static str], common: bool) -> Brand {
    Brand { name, domains, common }
}

pub static BRANDS: &[Brand] = &[
    b("paypal", &["paypal.com", "paypal.me"], false),
    b("venmo", &["venmo.com"], false),
    b("zelle", &["zellepay.com", "zelle.com"], false),
    b("stripe", &["stripe.com"], true),
    b("square", &["squareup.com", "square.com"], true),
    b("coinbase", &["coinbase.com"], false),
    b("binance", &["binance.com"], false),
    b("visa", &["visa.com"], true),
    b("mastercard", &["mastercard.com"], false),
    b("amex", &["americanexpress.com", "amex.com"], true),
    b("americanexpress", &["americanexpress.com"], false),
    b("chase", &["chase.com", "jpmorganchase.com"], true),
    b("wellsfargo", &["wellsfargo.com"], false),
    b("bankofamerica", &["bankofamerica.com", "bofa.com"], false),
    b("citibank", &["citi.com", "citibank.com"], false),
    b("citi", &["citi.com"], true),
    b("capitalone", &["capitalone.com"], false),
    b("hsbc", &["hsbc.com", "hsbc.co.uk"], false),
    b("barclays", &["barclays.com", "barclays.co.uk"], false),
    b("santander", &["santander.com", "santander.co.uk"], false),
    b("apple", &["apple.com", "icloud.com"], true),
    b("icloud", &["icloud.com", "apple.com"], false),
    b("itunes", &["apple.com", "itunes.com"], false),
    b("microsoft", &["microsoft.com", "live.com", "outlook.com", "office.com"], false),
    b("outlook", &["outlook.com", "microsoft.com"], false),
    b("office365", &["office.com", "microsoft.com"], false),
    b("hotmail", &["hotmail.com", "microsoft.com"], false),
    b("google", &["google.com", "gmail.com"], false),
    b("gmail", &["gmail.com", "google.com"], false),
    b("youtube", &["youtube.com", "google.com"], false),
    b("amazon", &["amazon.com", "amazon.co.uk", "amazon.de", "aws.amazon.com"], true),
    b("meta", &["meta.com", "facebook.com"], true),
    b("facebook", &["facebook.com", "meta.com"], false),
    b("instagram", &["instagram.com"], false),
    b("whatsapp", &["whatsapp.com"], false),
    b("netflix", &["netflix.com"], false),
    b("linkedin", &["linkedin.com"], false),
    b("docusign", &["docusign.com", "docusign.net"], false),
    b("dropbox", &["dropbox.com"], false),
    b("adobe", &["adobe.com"], false),
    b("ebay", &["ebay.com"], false),
    b("usps", &["usps.com"], false),
    b("ups", &["ups.com"], true),
    b("fedex", &["fedex.com"], false),
    b("dhl", &["dhl.com"], false),
    b("att", &["att.com"], true),
    b("verizon", &["verizon.com"], false),
    b("tmobile", &["t-mobile.com"], false),
    b("xfinity", &["xfinity.com", "comcast.com"], false),
    b("comcast", &["comcast.com", "xfinity.com"], false),
    b("irs", &["irs.gov"], true),
];

/// Words phishers put next to a brand.
const PHISH_WORDS: &[&str] = &["secure", "security", "login", "signin", "verify", "verification", "account", "accounts", "support", "update", "billing", "alert", "service", "auth", "wallet", "unlock", "recovery", "confirm", "help"];

/// A name with the usual lookalike swaps undone (`rn`→`m`, `0`→`o`, `1`→`l` ...).
fn unconfuse(s: &str) -> String {
    s.replace("rn", "m").replace("vv", "w").chars().map(|c| match c {
        '0' => 'o',
        '1' | '!' | '|' => 'l',
        '3' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' => 't',
        _ => c,
    }).collect()
}

/// The registrable part of a host (`shop.acme.co.uk` → `acme.co.uk`), by the public suffix list.
pub fn registrable(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    match psl::domain_str(&host) {
        Some(d) => d.to_string(),
        None => host,
    }
}

/// The main name of a domain, without its suffix (`shop.acme.co.uk` → `acme`).
pub fn main_label(host: &str) -> String {
    registrable(host).split('.').next().unwrap_or("").to_string()
}

fn on_domain(host: &str, owned: &str) -> bool {
    host == owned || host.ends_with(&format!(".{owned}"))
}

/// What proves an account belongs to a brand: a verified sending domain on the brand's own
/// domain, or an operator's confirmation. A website the customer typed proves nothing.
#[derive(Debug, Default, Clone)]
pub struct Affiliation<'a> {
    pub verified_domains: &'a [String],
    /// Brand names an operator confirmed (`accounts.sending_review_brands`).
    pub brands: &'a [String],
}

impl Affiliation<'_> {
    fn covers(&self, brand: &Brand) -> bool {
        self.brands.iter().any(|b| b.eq_ignore_ascii_case(brand.name))
            || self.verified_domains.iter().any(|v| brand.domains.iter().any(|d| registrable(v) == registrable(d)))
    }
}

/// The brand a domain name impersonates, if it does, unless the account proved it belongs to it.
pub fn impersonated_brand(domain: &str, proof: &Affiliation<'_>) -> Option<&'static str> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let suffix_len = registrable(&domain).split('.').skip(1).map(|l| l.len() + 1).sum::<usize>();
    let name_part = &domain[..domain.len().saturating_sub(suffix_len)];
    let raw_tokens: Vec<&str> = name_part.split(['.', '-', '_']).filter(|t| !t.is_empty()).collect();
    let tokens: Vec<String> = raw_tokens.iter().map(|t| unconfuse(t)).collect();
    let phishy = tokens.iter().any(|t| PHISH_WORDS.contains(&t.as_str()));
    for brand in BRANDS {
        if brand.domains.iter().any(|d| on_domain(&domain, d)) || proof.covers(brand) {
            continue;
        }
        let hit = raw_tokens.iter().zip(&tokens).any(|(raw, t)| {
            let lookalike = *raw != t.as_str();
            if t == brand.name {
                return !brand.common || phishy || lookalike;
            }
            // Run together with a phishing word: paypalsecure, securepaypal, apple-idlogin.
            t.strip_prefix(brand.name).or_else(|| t.strip_suffix(brand.name)).is_some_and(|rest| PHISH_WORDS.contains(&rest) || (rest.len() <= 2 && lookalike && !brand.common))
        });
        if hit {
            return Some(brand.name);
        }
    }
    None
}

/// A domain name that reads as random characters (`xkqzvbtr`, `a8f3k2m9`).
pub fn looks_random(domain: &str) -> bool {
    let label = main_label(domain);
    let letters: Vec<char> = label.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    let digits = label.chars().filter(|c| c.is_ascii_digit()).count();
    if label.len() < 6 {
        return false;
    }
    let vowel = |c: &char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
    let mut run = 0;
    let mut longest = 0;
    for c in label.chars() {
        if c.is_ascii_alphabetic() && !vowel(&c) {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let vowels = letters.iter().filter(|c| vowel(c)).count();
    longest >= 6 || (letters.len() >= 6 && vowels == 0) || (digits >= 3 && letters.len() >= 3 && digits * 3 >= label.len())
}

/// Two names that share nothing: different registrable domains, and no common word of 4+ letters.
pub fn unrelated(a: &str, b: &str) -> bool {
    if registrable(a) == registrable(b) {
        return false;
    }
    let (x, y) = (main_label(a), main_label(b));
    let (cx, cy) = (x.replace('-', ""), y.replace('-', ""));
    if cx.len() >= 4 && cy.len() >= 4 && (cx.contains(&cy) || cy.contains(&cx)) {
        return false;
    }
    let wa: HashSet<String> = x.split('-').filter(|w| w.len() >= 4).map(str::to_string).collect();
    !y.split('-').any(|w| w.len() >= 4 && wa.contains(w))
}

// ---------- Websites ----------

/// Free hosting addresses that aren't a business's own site.
const FREE_HOSTS: &[&str] = &[
    "wixsite.com", "weebly.com", "carrd.co", "github.io", "netlify.app", "vercel.app", "blogspot.com", "wordpress.com", "godaddysites.com", "square.site",
    "webflow.io", "000webhostapp.com", "glitch.me", "pages.dev", "web.app", "firebaseapp.com", "herokuapp.com", "framer.website", "notion.site", "linktr.ee",
];

pub fn free_host(host: &str) -> bool {
    FREE_HOSTS.iter().any(|f| on_domain(&host.to_ascii_lowercase(), f))
}

static PARKED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)domain (is|may be) for sale|buy this domain|this domain is parked|parked free|parkingcrew|sedoparking|bodis\.com|hugedomains|dan\.com|afternic|domain parking|this domain has been registered|make an offer on this domain").unwrap()
});
static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^\s*(coming soon|under construction|website coming soon|site coming soon)\b|welcome to nginx|it works!|apache2 (ubuntu|debian) default page|index of /|default web site page|future home of").unwrap()
});

/// Why a fetched home page isn't a real website (parked, for sale, a placeholder), if it isn't.
pub fn website_problem(title: &str, text: &str) -> Option<&'static str> {
    let both = format!("{title}\n{text}");
    if PARKED.is_match(&both) {
        return Some("The website looks parked or for sale.");
    }
    if PLACEHOLDER.is_match(title) || PLACEHOLDER.is_match(text.get(..text.len().min(400)).unwrap_or(text)) || text.split_whitespace().count() < 15 && title.trim().is_empty() {
        return Some("The website is a placeholder page, not a business's site yet.");
    }
    None
}

// ---------- Content ----------

static CREDENTIALS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)verify your (account|identity|email address|payment)|confirm your (password|account|identity|login|billing)|unusual sign[- ]?in|suspicious (sign[- ]?in|login|activity)|your account (has been|will be) (suspended|locked|disabled|limited|closed)|re-?enter your password|update your (payment|billing) (details|information|method)|reset your password|log ?in to (restore|unlock|verify)").unwrap()
});

pub fn asks_for_credentials(text: &str) -> bool {
    CREDENTIALS.is_match(text)
}

/// Subject lines that pretend to be a reply or a forward.
pub fn fake_reply(subject: &str) -> bool {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*(re|fwd?|aw|tr)\s*:").unwrap());
    RE.is_match(subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_source_words_are_whole_words() {
        assert_eq!(list_source_terms("apollo_export.csv"), vec!["apollo"]);
        assert_eq!(list_source_terms("ZoomInfo Leads 2026.csv"), vec!["leads", "zoominfo"]);
        assert_eq!(list_source_terms("my-lead-list.csv"), vec!["lead list"]);
        assert_eq!(list_source_terms("LeadList.csv"), vec!["lead list"]);
        assert_eq!(list_source_terms("SalesNavigator.csv"), vec!["sales navigator"]);
        assert_eq!(list_source_terms("LinkedIn URL"), vec!["linkedin"]);
        assert!(list_source_terms("newsletter-signups.csv").is_empty());
        assert!(list_source_terms("apollonia_customers.csv").is_empty(), "not a whole word");
        assert!(list_source_terms("mislead.csv").is_empty());
        assert!(list_source_terms("customers.csv").is_empty());
    }

    #[test]
    fn prospect_lists_are_recognized_by_columns() {
        let h = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(prospect_columns(&h(&["Email", "First Name", "Job Title", "Company Size", "Company Website"])));
        assert!(!prospect_columns(&h(&["Email", "First Name", "Job Title"])));
        assert!(!prospect_columns(&h(&["email", "first_name", "signup_date"])));
    }

    #[test]
    fn import_thresholds() {
        let s = |with: u64, role: u64, no_mx: u64, typo: u64| ImportStats { rows: with, with_address: with, role, no_mx, mx_checked: with, typo, ..Default::default() };
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 0, 0, 0)).0, Verdict::Pass);
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 15, 0, 0)).0, Verdict::Review, "exactly 15% role is a review");
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 16, 0, 0)).0, Verdict::Block);
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 5, 0, 0)).0, Verdict::Pass, "5% role is allowed");
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 6, 0, 0)).0, Verdict::Review);
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 0, 11, 0)).0, Verdict::Block);
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 0, 4, 0)).0, Verdict::Review);
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(100, 0, 0, 4)).0, Verdict::Review);
        assert_eq!(judge_import(Some("customers.csv"), &[], &s(10, 9, 9, 9)).0, Verdict::Pass, "too few addresses to judge shares");
        assert_eq!(judge_import(Some("apollo_export.csv"), &[], &s(10, 0, 0, 0)).0, Verdict::Block);
        assert_eq!(judge_import(None, &["email".into(), "Purchased".into()], &s(10, 0, 0, 0)).0, Verdict::Block);
    }

    #[test]
    fn brand_lookalikes() {
        assert_eq!(impersonated_brand("paypal-secure-login.com", &Affiliation::default()), Some("paypal"));
        assert_eq!(impersonated_brand("paypa1.com", &Affiliation::default()), Some("paypal"));
        assert_eq!(impersonated_brand("rnicrosoft.net", &Affiliation::default()), Some("microsoft"));
        assert_eq!(impersonated_brand("mail.netflix-billing.co", &Affiliation::default()), Some("netflix"));
        assert_eq!(impersonated_brand("applesupport.help", &Affiliation::default()), Some("apple"));
        assert_eq!(impersonated_brand("apple-verify.com", &Affiliation::default()), Some("apple"));
        assert_eq!(impersonated_brand("app1e.com", &Affiliation::default()), Some("apple"), "a lookalike spelling of a common word");
        assert_eq!(impersonated_brand("wellsfargo.info", &Affiliation::default()), Some("wellsfargo"));
        assert_eq!(impersonated_brand("secure-chase.com", &Affiliation::default()), Some("chase"));
        // Ordinary words and real owners are fine.
        assert_eq!(impersonated_brand("apple-orchard-farm.com", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("bigappletours.com", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("amazon-river-tours.com", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("pop-ups.com", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("chasefarms.com", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("news.paypal.com", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("paypal-news.com", &Affiliation { verified_domains: &["mail.paypal.com".to_string()], brands: &[] }), None, "a verified domain on the brand proves it");
        assert_eq!(impersonated_brand("paypal-news.com", &Affiliation { verified_domains: &[], brands: &["paypal".to_string()] }), None, "an operator confirmed it");
        assert_eq!(impersonated_brand("acme.test", &Affiliation::default()), None);
        assert_eq!(impersonated_brand("mail.acmegoods.com", &Affiliation::default()), None);
    }

    #[test]
    fn a_website_proves_nothing_about_a_brand() {
        // The account typed paypal.com as its website: still a lookalike.
        assert_eq!(impersonated_brand("paypal-secure-login.com", &Affiliation::default()), Some("paypal"));
        // A verified sending domain on another brand doesn't help either.
        assert_eq!(impersonated_brand("paypal-secure-login.com", &Affiliation { verified_domains: &["mail.netflix.com".to_string()], brands: &[] }), Some("paypal"));
    }

    #[test]
    fn mail_server_shares_count_only_what_was_checked() {
        let s = |with: u64, checked: u64, no_mx: u64| ImportStats { rows: with, with_address: with, no_mx, mx_checked: checked, ..Default::default() };
        // 20 of the 50 addresses checked have no mail server (40%), but only half the file was checked.
        let (v, why) = judge_import(None, &[], &s(100, 50, 20));
        assert_eq!(v, Verdict::Review, "no block without 80% coverage");
        assert!(why[0].contains("50.0% of the file"), "{why:?}");
        assert_eq!(judge_import(None, &[], &s(100, 80, 20)).0, Verdict::Block);
        assert_eq!(judge_import(None, &[], &s(100, 10, 10)).0, Verdict::Pass, "too few checked to judge");
    }

    #[test]
    fn registrable_names_follow_the_public_suffix_list() {
        assert_eq!(registrable("shop.acme.co.uk"), "acme.co.uk");
        assert_eq!(registrable("evil.github.io"), "evil.github.io", "github.io is a public suffix: each site is its own");
        assert_ne!(registrable("www.acme.github.io"), registrable("evil.github.io"));
        assert_eq!(registrable("a.b.example.com.au"), "example.com.au");
        assert_eq!(registrable("mail.acme.test"), "acme.test");
    }

    #[test]
    fn random_names() {
        assert!(looks_random("xkqzvbtr.com"));
        assert!(looks_random("a8f3k2m9.net"));
        assert!(looks_random("qwrtpsdf.io"));
        assert!(!looks_random("acmegoods.com"));
        assert!(!looks_random("strengths.co"));
        assert!(!looks_random("rhythm.com"));
        assert!(!looks_random("shop.example.co.uk"));
        assert!(!looks_random("store24.com"));
    }

    #[test]
    fn related_names() {
        assert!(!unrelated("acme.test", "mail.acme.test"));
        assert!(!unrelated("acmegoods.com", "acme-goods.com"));
        assert!(!unrelated("acmegoods.com", "shop-acmegoods.net"));
        assert!(unrelated("acme.test", "zebra-deals.com"));
        assert_eq!(registrable("shop.acme.co.uk"), "acme.co.uk");
        assert_eq!(main_label("news.acme.com"), "acme");
    }

    #[test]
    fn websites() {
        assert!(website_problem("acme.test is for sale", "Buy this domain today").is_some());
        assert!(website_problem("Coming soon", "").is_some());
        assert!(website_problem("Welcome to nginx!", "If you see this page, the nginx web server is successfully installed").is_some());
        assert!(website_problem("Acme Goods - handmade soap", "We make soap by hand in Portland from local ingredients. Shop bars, gift sets and refills, shipped free over $40.").is_none());
        assert!(free_host("acme.wixsite.com"));
        assert!(!free_host("acme.com"));
    }

    #[test]
    fn content_language() {
        assert!(asks_for_credentials("We noticed an unusual sign-in. Verify your account now."));
        assert!(asks_for_credentials("Your account has been suspended"));
        assert!(!asks_for_credentials("Our fall collection is here. Shop now."));
        assert!(fake_reply("Re: your order"));
        assert!(fake_reply("FWD: quick question"));
        assert!(!fake_reply("Recipes for fall"));
    }
}

//! Normalizing and hashing identifiers exactly as each ad platform documents, so matches work.
//! Everything that leaves the server for a match is SHA-256 hashed here first (Google's region and
//! postal codes are the documented exceptions: Google wants them in plain text).
//!
//! - Meta: https://developers.facebook.com/docs/marketing-api/audiences/guides/custom-audiences (hashing and normalization table)
//! - Google: https://developers.google.com/data-manager/api/devguides/concepts/formatting
//! - TikTok: https://ads.tiktok.com/help/article/how-to-create-a-custom-audience-with-a-customer-file

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::platforms::Platform;

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// What we know about a person, from their profile.
#[derive(Debug, Default, Clone)]
pub struct Person {
    pub email: Option<String>,
    /// E.164 as stored on profiles (`+15035550100`).
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    /// ISO 3166-1 alpha-2, uppercase.
    pub country: Option<String>,
    /// The profile's `ads_consent` property: Some(true) when they agreed to their data being used
    /// for ads, Some(false) when they refused, None when unknown.
    pub ads_consent: Option<bool>,
}

fn clean(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

// ---------- Meta ----------

/// Trim and lowercase.
pub fn meta_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_lowercase();
    (e.contains('@') && e.len() > 3).then_some(e)
}

/// Digits only, country code included, no leading zeros.
pub fn meta_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_start_matches('0').to_string();
    (digits.len() >= 7).then_some(digits)
}

/// Lowercase letters only: no punctuation, digits or spaces. Accented letters stay (UTF-8).
pub fn meta_letters(raw: &str) -> Option<String> {
    let s: String = raw.trim().to_lowercase().chars().filter(|c| c.is_alphabetic()).collect();
    (!s.is_empty()).then_some(s)
}

/// US: the lowercase 2-letter state code. Elsewhere: lowercase with no punctuation or spaces.
pub fn meta_state(raw: &str, country: Option<&str>) -> Option<String> {
    let s = raw.trim();
    if country.is_none_or(|c| c == "US") {
        if let Some(code) = us_state_code(s) {
            return Some(code.to_lowercase());
        }
        if country == Some("US") {
            return None;
        }
    }
    let s: String = s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    (!s.is_empty()).then_some(s)
}

/// Lowercase, no spaces. US: the first 5 digits only.
pub fn meta_zip(raw: &str, country: Option<&str>) -> Option<String> {
    let s: String = raw.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if country == Some("US") || (country.is_none() && s.chars().take(5).all(|c| c.is_ascii_digit()) && s.len() >= 5) {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        return (digits.len() >= 5).then(|| digits[..5].to_string());
    }
    (!s.is_empty()).then_some(s)
}

// ---------- Google ----------

/// Lowercase with all whitespace removed. For gmail.com and googlemail.com, dots and anything after
/// a `+` in the part before the @ are removed too.
pub fn google_email(raw: &str) -> Option<String> {
    let e: String = raw.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let (local, domain) = e.split_once('@')?;
    if local.is_empty() || !domain.contains('.') {
        return None;
    }
    if domain == "gmail.com" || domain == "googlemail.com" {
        let local = local.split('+').next().unwrap_or_default().replace('.', "");
        if local.is_empty() {
            return None;
        }
        return Some(format!("{local}@{domain}"));
    }
    Some(e)
}

/// E.164: `+` and digits only.
pub fn e164(raw: &str) -> Option<String> {
    let t = raw.trim();
    let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    if !t.starts_with('+') || !(7..=15).contains(&digits.len()) || digits.starts_with('0') {
        return None;
    }
    Some(format!("+{digits}"))
}

const NAME_PREFIXES: &[&str] = &["mr", "mrs", "ms", "miss", "mx", "dr", "prof", "sir", "madam"];
const NAME_SUFFIXES: &[&str] = &["jr", "sr", "ii", "iii", "iv", "v", "phd", "md", "esq", "dds"];

/// Lowercase, punctuation removed, surrounding whitespace trimmed; prefixes (Mrs.) dropped from given
/// names and suffixes (Jr.) from family names.
pub fn google_name(raw: &str, given: bool) -> Option<String> {
    let lowered = raw.trim().to_lowercase();
    let mut words: Vec<String> =
        lowered.split_whitespace().map(|w| w.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '\'').collect::<String>()).filter(|w| !w.is_empty()).collect();
    let strip = |w: &str| w.trim_matches(|c: char| c == '-' || c == '\'').to_string();
    if given {
        while words.len() > 1 && NAME_PREFIXES.contains(&strip(&words[0]).as_str()) {
            words.remove(0);
        }
    } else {
        while words.len() > 1 && NAME_SUFFIXES.contains(&strip(words.last().map(String::as_str).unwrap_or_default()).as_str()) {
            words.pop();
        }
    }
    // Punctuation inside words (hyphens, apostrophes) goes too.
    let s = words.iter().map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>()).collect::<Vec<_>>().join(" ");
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// US: 5 digits. Elsewhere: trimmed, no extension.
pub fn google_postal(raw: &str, country: &str) -> Option<String> {
    let s = raw.trim();
    if country == "US" {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        return (digits.len() >= 5).then(|| digits[..5].to_string());
    }
    (!s.is_empty()).then(|| s.to_uppercase())
}

// ---------- TikTok ----------

/// Spaces removed, lowercase.
pub fn tiktok_email(raw: &str) -> Option<String> {
    let e: String = raw.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    (e.contains('@') && e.len() > 3).then_some(e)
}

// ---------- Countries ----------

/// The European Economic Area, the UK and Switzerland: Google needs consent for ad use of data from
/// people here (EU user consent policy, Digital Markets Act).
pub const CONSENT_REGIONS: &[&str] = &[
    "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT", "LV", "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE", "IS",
    "LI", "NO", "GB", "CH",
];

/// An ISO 3166-1 alpha-2 code (uppercase) from a profile's `country` property: a 2-letter code or a
/// few common names.
pub fn country_code(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
        let up = s.to_uppercase();
        return Some(if up == "UK" { "GB".into() } else { up });
    }
    let named = match s.to_lowercase().replace('.', "").as_str() {
        "usa" | "united states" | "united states of america" | "us of a" => "US",
        "canada" => "CA",
        "united kingdom" | "great britain" | "england" | "scotland" | "wales" | "northern ireland" => "GB",
        "australia" => "AU",
        "new zealand" => "NZ",
        "ireland" => "IE",
        "germany" | "deutschland" => "DE",
        "france" => "FR",
        "spain" | "españa" => "ES",
        "italy" | "italia" => "IT",
        "netherlands" | "the netherlands" => "NL",
        "mexico" | "méxico" => "MX",
        _ => return None,
    };
    Some(named.into())
}

fn us_state_code(s: &str) -> Option<&'static str> {
    let key = s.trim().trim_end_matches('.').to_lowercase();
    US_STATES.iter().find(|(code, name)| code.eq_ignore_ascii_case(&key) || *name == key).map(|(c, _)| *c)
}

const US_STATES: &[(&str, &str)] = &[
    ("AL", "alabama"), ("AK", "alaska"), ("AZ", "arizona"), ("AR", "arkansas"), ("CA", "california"), ("CO", "colorado"), ("CT", "connecticut"),
    ("DE", "delaware"), ("DC", "district of columbia"), ("FL", "florida"), ("GA", "georgia"), ("HI", "hawaii"), ("ID", "idaho"), ("IL", "illinois"),
    ("IN", "indiana"), ("IA", "iowa"), ("KS", "kansas"), ("KY", "kentucky"), ("LA", "louisiana"), ("ME", "maine"), ("MD", "maryland"),
    ("MA", "massachusetts"), ("MI", "michigan"), ("MN", "minnesota"), ("MS", "mississippi"), ("MO", "missouri"), ("MT", "montana"), ("NE", "nebraska"),
    ("NV", "nevada"), ("NH", "new hampshire"), ("NJ", "new jersey"), ("NM", "new mexico"), ("NY", "new york"), ("NC", "north carolina"),
    ("ND", "north dakota"), ("OH", "ohio"), ("OK", "oklahoma"), ("OR", "oregon"), ("PA", "pennsylvania"), ("PR", "puerto rico"), ("RI", "rhode island"),
    ("SC", "south carolina"), ("SD", "south dakota"), ("TN", "tennessee"), ("TX", "texas"), ("UT", "utah"), ("VT", "vermont"), ("VA", "virginia"),
    ("WA", "washington"), ("WV", "west virginia"), ("WI", "wisconsin"), ("WY", "wyoming"),
];

// ---------- Per-platform identifier sets ----------

/// Why a person in the source isn't sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Nothing this platform can match on (e.g. no email for TikTok).
    NoIdentifiers,
    /// In a region where Google needs ad consent, and we don't have it.
    NoConsent,
}

/// Meta's schema keys for a sync, in the order rows are sent.
pub fn meta_schema(with_location: bool) -> Vec<&'static str> {
    if with_location { vec!["EMAIL", "PHONE", "FN", "LN", "CT", "ST", "ZIP", "COUNTRY"] } else { vec!["EMAIL", "PHONE"] }
}

/// The hashed identifiers to send for one person, as a small JSON object. This exact object is
/// stored in the sync's snapshot, so removals resend the same values.
pub fn identifiers(platform: Platform, p: &Person, with_location: bool) -> Result<Map<String, Value>, Skip> {
    let mut m = Map::new();
    let country = clean(&p.country).and_then(country_code);
    let cc = country.as_deref();
    match platform {
        Platform::Meta => {
            let put = |m: &mut Map<String, Value>, k: &str, v: Option<String>| {
                if let Some(v) = v {
                    m.insert(k.into(), json!(sha256_hex(&v)));
                }
            };
            put(&mut m, "EMAIL", clean(&p.email).and_then(meta_email));
            put(&mut m, "PHONE", clean(&p.phone).and_then(meta_phone));
            if !m.is_empty() && with_location {
                put(&mut m, "FN", clean(&p.first_name).and_then(meta_letters));
                put(&mut m, "LN", clean(&p.last_name).and_then(meta_letters));
                put(&mut m, "CT", clean(&p.city).and_then(meta_letters));
                put(&mut m, "ST", clean(&p.state).and_then(|s| meta_state(s, cc)));
                put(&mut m, "ZIP", clean(&p.zip).and_then(|z| meta_zip(z, cc)));
                put(&mut m, "COUNTRY", cc.map(str::to_lowercase));
            }
        }
        Platform::Google => {
            // A country we can't read might be one that needs consent: fail closed.
            let unreadable = clean(&p.country).is_some() && cc.is_none();
            if unreadable || cc.is_some_and(|c| CONSENT_REGIONS.contains(&c)) {
                if p.ads_consent != Some(true) {
                    return Err(Skip::NoConsent);
                }
                m.insert("consent".into(), json!("granted"));
            }
            if let Some(e) = clean(&p.email).and_then(google_email) {
                m.insert("email".into(), json!(sha256_hex(&e)));
            }
            if let Some(ph) = clean(&p.phone).and_then(e164) {
                m.insert("phone".into(), json!(sha256_hex(&ph)));
            }
            if with_location {
                let given = clean(&p.first_name).and_then(|n| google_name(n, true));
                let family = clean(&p.last_name).and_then(|n| google_name(n, false));
                let postal = cc.and_then(|c| clean(&p.zip).and_then(|z| google_postal(z, c)));
                if let (Some(g), Some(f), Some(c), Some(z)) = (given, family, cc, postal) {
                    m.insert("address".into(), json!({"givenName": sha256_hex(&g), "familyName": sha256_hex(&f), "regionCode": c, "postalCode": z}));
                }
            }
            if !m.keys().any(|k| k != "consent") {
                return Err(Skip::NoIdentifiers);
            }
        }
        Platform::Tiktok => {
            if let Some(e) = clean(&p.email).and_then(tiktok_email) {
                m.insert("email".into(), json!(sha256_hex(&e)));
            }
        }
    }
    if m.is_empty() {
        return Err(Skip::NoIdentifiers);
    }
    Ok(m)
}

/// A short, stable fingerprint of an identifier set (keys are kept sorted by serde_json's map).
pub fn digest(ids: &Map<String, Value>) -> String {
    sha256_hex(&Value::Object(ids.clone()).to_string())[..32].to_string()
}

/// One Meta payload row in schema order ("" where a value is missing).
pub fn meta_row(ids: &Value, schema: &[&str]) -> Vec<String> {
    schema.iter().map(|k| ids[*k].as_str().unwrap_or_default().to_string()).collect()
}

/// One Google Data Manager AudienceMember.
pub fn google_member(ids: &Value) -> Value {
    let mut idents = Vec::new();
    if let Some(e) = ids["email"].as_str() {
        idents.push(json!({"emailAddress": e}));
    }
    if let Some(p) = ids["phone"].as_str() {
        idents.push(json!({"phoneNumber": p}));
    }
    if ids["address"].is_object() {
        idents.push(json!({"address": ids["address"]}));
    }
    let mut m = json!({"userData": {"userIdentifiers": idents}});
    if ids["consent"] == "granted" {
        m["consent"] = json!({"adUserData": "CONSENT_GRANTED", "adPersonalization": "CONSENT_GRANTED"});
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_doc_example() {
        // Meta's Custom Audiences guide: mary@example.com hashes to this.
        assert_eq!(sha256_hex(&meta_email("  Mary@Example.com ").unwrap()), "f1904cf1a9d73a55fa5de0ac823c4403ded71afd4c3248d00bdcd0866552bb79");
    }

    #[test]
    fn meta_fields() {
        assert_eq!(meta_phone("+1 (650) 555-1212").as_deref(), Some("16505551212"));
        assert_eq!(meta_phone("+0044 20 7946 0958").as_deref(), Some("442079460958"));
        assert_eq!(meta_letters("Mary-Jane O'Brien").as_deref(), Some("maryjaneobrien"));
        assert_eq!(meta_letters("José").as_deref(), Some("josé"));
        assert_eq!(meta_letters("San Francisco").as_deref(), Some("sanfrancisco"));
        assert_eq!(meta_state("California", Some("US")).as_deref(), Some("ca"));
        assert_eq!(meta_state("or", Some("US")).as_deref(), Some("or"));
        assert_eq!(meta_state("Nueva York", Some("US")), None);
        assert_eq!(meta_state("Île-de-France", Some("FR")).as_deref(), Some("îledefrance"));
        assert_eq!(meta_zip("97201-1234", Some("US")).as_deref(), Some("97201"));
        assert_eq!(meta_zip("SW1A 1AA", Some("GB")).as_deref(), Some("sw1a1aa"));
    }

    #[test]
    fn google_doc_examples() {
        // Google's formatting guide.
        assert_eq!(google_email("cloudy.sanfrancisco+shopping@gmail.com").as_deref(), Some("cloudysanfrancisco@gmail.com"));
        assert_eq!(google_email("user.name+NYC@Example.com").as_deref(), Some("user.name+nyc@example.com"));
        assert_eq!(google_email(" Ann @ Googlemail.com ").as_deref(), Some("ann@googlemail.com"));
        assert_eq!(e164("+1 (800) 555-0100").as_deref(), Some("+18005550100"));
        assert_eq!(e164("8005550100"), None);
        assert_eq!(google_name("Mrs. Jane", true).as_deref(), Some("jane"));
        assert_eq!(google_name("Smith Jr.", false).as_deref(), Some("smith"));
        assert_eq!(google_name("  O'Neil  ", false).as_deref(), Some("oneil"));
        assert_eq!(google_postal("94043-1351", "US").as_deref(), Some("94043"));
        assert_eq!(google_postal(" 75001 ", "FR").as_deref(), Some("75001"));
    }

    #[test]
    fn tiktok_fields() {
        // TikTok's customer file guide: E.164 with the + ("+16577770000"), email trimmed and lowercased.
        assert_eq!(e164("+1 657 777 0000").as_deref(), Some("+16577770000"));
        assert_eq!(sha256_hex("+16577770000"), "a6e82c2f4091baff57a4f33d01efb65bfbeacbbfbcfaa9d0ff9f57d28d78fedb");
        assert_eq!(tiktok_email(" Ann@Shop.COM").as_deref(), Some("ann@shop.com"));
    }

    #[test]
    fn identifier_sets() {
        let p = Person {
            email: Some("Mary@Example.com".into()),
            phone: Some("+15035550100".into()),
            first_name: Some("Mary".into()),
            last_name: Some("Jones".into()),
            city: Some("Portland".into()),
            state: Some("Oregon".into()),
            zip: Some("97201".into()),
            country: Some("United States".into()),
            ads_consent: None,
        };
        let meta = identifiers(Platform::Meta, &p, false).unwrap();
        assert_eq!(meta.keys().collect::<Vec<_>>(), vec!["EMAIL", "PHONE"]);
        assert_eq!(meta["PHONE"], json!(sha256_hex("15035550100")));
        let meta = identifiers(Platform::Meta, &p, true).unwrap();
        assert_eq!(meta["ST"], json!(sha256_hex("or")));
        assert_eq!(meta["COUNTRY"], json!(sha256_hex("us")));
        let row = meta_row(&Value::Object(meta), &meta_schema(true));
        assert_eq!(row.len(), 8);
        assert!(row.iter().all(|v| v.len() == 64));

        let g = identifiers(Platform::Google, &p, true).unwrap();
        assert_eq!(g["phone"], json!(sha256_hex("+15035550100")));
        assert_eq!(g["address"]["regionCode"], "US");
        assert_eq!(g["address"]["postalCode"], "97201");
        assert!(g.get("consent").is_none());
        let member = google_member(&Value::Object(g));
        assert_eq!(member["userData"]["userIdentifiers"].as_array().unwrap().len(), 3);
        assert!(member.get("consent").is_none());

        let t = identifiers(Platform::Tiktok, &p, true).unwrap();
        assert_eq!(t.keys().collect::<Vec<_>>(), vec!["email"]);
        let no_email = Person { email: None, ..p.clone() };
        assert_eq!(identifiers(Platform::Tiktok, &no_email, false), Err(Skip::NoIdentifiers));
        assert!(identifiers(Platform::Meta, &no_email, false).is_ok());
    }

    #[test]
    fn google_consent_regions() {
        let de = Person { email: Some("a@b.de".into()), country: Some("de".into()), ..Default::default() };
        assert_eq!(identifiers(Platform::Google, &de, false), Err(Skip::NoConsent));
        let refused = Person { ads_consent: Some(false), ..de.clone() };
        assert_eq!(identifiers(Platform::Google, &refused, false), Err(Skip::NoConsent));
        let granted = Person { ads_consent: Some(true), ..de.clone() };
        let ids = identifiers(Platform::Google, &granted, false).unwrap();
        assert_eq!(google_member(&Value::Object(ids))["consent"], json!({"adUserData": "CONSENT_GRANTED", "adPersonalization": "CONSENT_GRANTED"}));
        // Meta and TikTok don't take these signals; their opt-outs are handled before this point.
        assert!(identifiers(Platform::Meta, &de, false).is_ok());
        let uk = Person { country: Some("UK".into()), ..de.clone() };
        assert_eq!(identifiers(Platform::Google, &uk, false), Err(Skip::NoConsent));
        // A country we can't read is treated as one that needs consent.
        let unknown = Person { country: Some("Österreich".into()), ..de.clone() };
        assert_eq!(identifiers(Platform::Google, &unknown, false), Err(Skip::NoConsent));
        let none = Person { country: None, ..de.clone() };
        assert!(identifiers(Platform::Google, &none, false).is_ok());
    }

    #[test]
    fn digest_tracks_changes() {
        let a = identifiers(Platform::Meta, &Person { email: Some("a@x.com".into()), ..Default::default() }, false).unwrap();
        let b = identifiers(Platform::Meta, &Person { email: Some("A@X.com ".into()), ..Default::default() }, false).unwrap();
        let c = identifiers(Platform::Meta, &Person { email: Some("c@x.com".into()), ..Default::default() }, false).unwrap();
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&c));
    }
}

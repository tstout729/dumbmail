//! Normalizing and hashing identifiers exactly as each ad platform documents, so matches work.
//! Everything that leaves the server for a match is SHA-256 hashed here first.
//!
//! - Meta: https://developers.facebook.com/docs/marketing-api/audiences/guides/custom-audiences (hashing and normalization table)
//! - Google: https://developers.google.com/data-manager/api/devguides/concepts/formatting
//! - TikTok: https://ads.tiktok.com/help/article/how-to-create-a-custom-audience-with-a-customer-file
//!
//! A sync works in identifiers ("keys"): one hashed email or one hashed phone number. Two profiles
//! whose addresses normalize to the same hash are the same key on the platform.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

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
    /// The phone number was opted in (text consent `subscribed`). A browser can fill a missing
    /// phone number on a profile but can't set consent, so only an opted-in number is sent.
    pub phone_trusted: bool,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub country: Option<String>,
}

fn clean(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Accented Latin letters to their base letter (é → e), then lowercase ASCII letters and digits only.
fn ascii_fold(raw: &str) -> String {
    raw.nfd().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

// ---------- Meta ----------

/// Trim and lowercase.
pub fn meta_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_lowercase();
    (e.contains('@') && e.len() > 3).then_some(e)
}

/// Digits only, country code included, no leading zeros. Profiles store E.164, so the country code
/// is always there.
pub fn meta_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_start_matches('0').to_string();
    (digits.len() >= 7).then_some(digits)
}

/// First and last names: lowercase, no punctuation or spaces; letters outside a–z stay (UTF-8),
/// which Meta allows for names only.
pub fn meta_name(raw: &str) -> Option<String> {
    let s: String = raw.trim().to_lowercase().chars().filter(|c| c.is_alphabetic()).collect();
    (!s.is_empty()).then_some(s)
}

/// City: a–z only.
pub fn meta_city(raw: &str) -> Option<String> {
    let s: String = ascii_fold(raw).chars().filter(|c| c.is_ascii_lowercase()).collect();
    (!s.is_empty()).then_some(s)
}

/// US: the lowercase 2-letter state code. Elsewhere: lowercase with no punctuation, special
/// characters or spaces.
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
    let s = ascii_fold(s);
    (!s.is_empty()).then_some(s)
}

/// Lowercase, no spaces. US: the first 5 digits. UK: area, district and sector ("SW1A 1AA" → "sw1a1").
pub fn meta_zip(raw: &str, country: Option<&str>) -> Option<String> {
    let s: String = raw.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if country == Some("US") || (country.is_none() && s.len() >= 5 && s.chars().take(5).all(|c| c.is_ascii_digit())) {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        return (digits.len() >= 5).then(|| digits[..5].to_string());
    }
    if country == Some("GB") {
        let s: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        // The inward code is always the last three characters: a digit and two letters.
        if (5..=7).contains(&s.len()) && s.is_char_boundary(s.len() - 3) {
            let (outward, inward) = s.split_at(s.len() - 3);
            if let Some(d) = inward.chars().next().filter(char::is_ascii_digit) {
                return Some(format!("{outward}{d}"));
            }
        }
        return (!s.is_empty()).then_some(s);
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

// ---------- TikTok ----------

/// Spaces removed, lowercase.
pub fn tiktok_email(raw: &str) -> Option<String> {
    let e: String = raw.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    (e.contains('@') && e.len() > 3).then_some(e)
}

// ---------- Countries ----------

/// An ISO 3166-1 alpha-2 code (uppercase) from a profile's `country` property: a 2-letter code or a
/// few common names. Used only for Meta's optional location fields.
pub fn country_code(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
        let up = s.to_uppercase();
        return Some(if up == "UK" { "GB".into() } else { up });
    }
    let named = match s.to_lowercase().replace('.', "").as_str() {
        "usa" | "united states" | "united states of america" => "US",
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

#[rustfmt::skip]
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

// ---------- Keys ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Email,
    Phone,
}

impl KeyType {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyType::Email => "email",
            KeyType::Phone => "phone",
        }
    }
}

/// One hashed identifier, with the hash of the stored value it came from (to notice at dispatch
/// that the profile's address changed since the run started).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub kind: KeyType,
    pub hash: String,
    pub src_check: String,
}

/// The platform's hash of an email or phone, as the platform normalizes it.
fn hash_for(platform: Platform, kind: KeyType, raw: &str) -> Option<String> {
    let normalized = match (platform, kind) {
        (Platform::Meta, KeyType::Email) => meta_email(raw),
        (Platform::Meta, KeyType::Phone) => meta_phone(raw),
        (Platform::Google, KeyType::Email) => google_email(raw),
        (Platform::Google, KeyType::Phone) => e164(raw),
        (Platform::Tiktok, KeyType::Email) => tiktok_email(raw),
        // TikTok audiences take one identifier type each; DumbMail's TikTok audiences are email.
        (Platform::Tiktok, KeyType::Phone) => None,
    }?;
    Some(sha256_hex(&normalized))
}

/// The identifiers to send for someone who qualifies: their email, and their phone number when it
/// was opted in. `veto` lists every identifier of someone who opted out, opted in or not, so none
/// of them is ever added and each one is removed.
pub fn keys(platform: Platform, p: &Person, veto: bool) -> Vec<Key> {
    let mut out = Vec::new();
    if let Some(e) = clean(&p.email) {
        if let Some(hash) = hash_for(platform, KeyType::Email, e) {
            out.push(Key { kind: KeyType::Email, hash, src_check: sha256_hex(p.email.as_deref().unwrap_or_default()) });
        }
    }
    if let Some(ph) = clean(&p.phone).filter(|_| veto || p.phone_trusted) {
        if let Some(hash) = hash_for(platform, KeyType::Phone, ph) {
            out.push(Key { kind: KeyType::Phone, hash, src_check: sha256_hex(p.phone.as_deref().unwrap_or_default()) });
        }
    }
    out
}

/// Meta's optional match fields (hashed), sent alongside an identifier when adding.
pub fn meta_hints(p: &Person) -> Option<Value> {
    let cc = clean(&p.country).and_then(country_code);
    let cc = cc.as_deref();
    let mut m = Map::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            m.insert(k.into(), json!(sha256_hex(&v)));
        }
    };
    put("FN", clean(&p.first_name).and_then(meta_name));
    put("LN", clean(&p.last_name).and_then(meta_name));
    put("CT", clean(&p.city).and_then(meta_city));
    put("ST", clean(&p.state).and_then(|s| meta_state(s, cc)));
    put("ZIP", clean(&p.zip).and_then(|z| meta_zip(z, cc)));
    put("COUNTRY", cc.map(str::to_lowercase));
    (!m.is_empty()).then_some(Value::Object(m))
}

pub const META_HINT_KEYS: [&str; 6] = ["FN", "LN", "CT", "ST", "ZIP", "COUNTRY"];

/// Meta's schema for a batch, in the order row values are sent.
pub fn meta_schema(with_hints: bool) -> Vec<&'static str> {
    let mut s = vec!["EMAIL", "PHONE"];
    if with_hints {
        s.extend(META_HINT_KEYS);
    }
    s
}

/// One batch row as stored: `{"t": "email", "h": "<hash>", "x": {hints}}`.
pub fn meta_row(row: &Value, schema: &[&str]) -> Vec<String> {
    let t = row["t"].as_str().unwrap_or_default();
    let h = row["h"].as_str().unwrap_or_default();
    schema
        .iter()
        .map(|k| match *k {
            "EMAIL" if t == "email" => h.to_string(),
            "PHONE" if t == "phone" => h.to_string(),
            "EMAIL" | "PHONE" => String::new(),
            other => row["x"][other].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// One Google Data Manager AudienceMember: one identifier per member, so Google's per-record
/// status maps back to exactly one key.
pub fn google_member(row: &Value) -> Value {
    let h = row["h"].as_str().unwrap_or_default();
    let ident = if row["t"] == "phone" { json!({"phoneNumber": h}) } else { json!({"emailAddress": h}) };
    json!({"userData": {"userIdentifiers": [ident]}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_doc_vectors() {
        // Meta's Custom Audiences guide.
        assert_eq!(sha256_hex(&meta_email("  Mary@Example.com ").unwrap()), "f1904cf1a9d73a55fa5de0ac823c4403ded71afd4c3248d00bdcd0866552bb79");
        assert_eq!(sha256_hex(&meta_phone("+1 (555) 987-6543").unwrap()), "1ef970831d7963307784fa8688e8fce101a15685d62aa765fed23f3a2c576a4e");
    }

    #[test]
    fn meta_fields() {
        assert_eq!(meta_phone("+1 (650) 555-1212").as_deref(), Some("16505551212"));
        assert_eq!(meta_phone("+0044 20 7946 0958").as_deref(), Some("442079460958"));
        assert_eq!(meta_name("Mary-Jane O'Brien").as_deref(), Some("maryjaneobrien"));
        assert_eq!(meta_name("José").as_deref(), Some("josé"), "names keep UTF-8 letters");
        assert_eq!(meta_city("San Francisco").as_deref(), Some("sanfrancisco"));
        assert_eq!(meta_city("Zürich").as_deref(), Some("zurich"), "cities are a–z only");
        assert_eq!(meta_city("Saint-Étienne").as_deref(), Some("saintetienne"));
        assert_eq!(meta_state("California", Some("US")).as_deref(), Some("ca"));
        assert_eq!(meta_state("or", Some("US")).as_deref(), Some("or"));
        assert_eq!(meta_state("Nueva York", Some("US")), None);
        assert_eq!(meta_state("Île-de-France", Some("FR")).as_deref(), Some("iledefrance"));
        assert_eq!(meta_zip("97201-1234", Some("US")).as_deref(), Some("97201"));
        assert_eq!(meta_zip("SW1A 1AA", Some("GB")).as_deref(), Some("sw1a1"), "UK: area, district and sector");
        assert_eq!(meta_zip("M1 1AE", Some("GB")).as_deref(), Some("m11"));
        assert_eq!(meta_zip("75001", Some("FR")).as_deref(), Some("75001"));
    }

    #[test]
    fn google_doc_examples() {
        // Google's formatting guide.
        assert_eq!(google_email("cloudy.sanfrancisco+shopping@gmail.com").as_deref(), Some("cloudysanfrancisco@gmail.com"));
        assert_eq!(google_email("user.name+NYC@Example.com").as_deref(), Some("user.name+nyc@example.com"));
        assert_eq!(google_email(" Ann @ Googlemail.com ").as_deref(), Some("ann@googlemail.com"));
        assert_eq!(e164("+1 (800) 555-0100").as_deref(), Some("+18005550100"));
        assert_eq!(e164("8005550100"), None);
    }

    #[test]
    fn tiktok_fields() {
        assert_eq!(tiktok_email(" Ann@Shop.COM").as_deref(), Some("ann@shop.com"));
        assert_eq!(tiktok_email("a b@shop.com").as_deref(), Some("ab@shop.com"));
    }

    #[test]
    fn key_sets() {
        let p = Person {
            email: Some("Mary@Example.com".into()),
            phone: Some("+15035550100".into()),
            phone_trusted: true,
            first_name: Some("Mary".into()),
            last_name: Some("Jones".into()),
            city: Some("Portland".into()),
            state: Some("Oregon".into()),
            zip: Some("97201".into()),
            country: Some("United States".into()),
        };
        let meta = keys(Platform::Meta, &p, false);
        assert_eq!(meta.iter().map(|k| (k.kind, k.hash.clone())).collect::<Vec<_>>(), vec![(KeyType::Email, sha256_hex("mary@example.com")), (KeyType::Phone, sha256_hex("15035550100"))]);
        assert_eq!(meta[0].src_check, sha256_hex("Mary@Example.com"));
        let hints = meta_hints(&p).unwrap();
        assert_eq!(hints["ST"], json!(sha256_hex("or")));
        assert_eq!(hints["COUNTRY"], json!(sha256_hex("us")));
        let row = json!({"t": "email", "h": meta[0].hash, "x": hints});
        let cells = meta_row(&row, &meta_schema(true));
        assert_eq!(cells.len(), 8);
        assert_eq!(cells[1], "", "one identifier per row");
        assert!(cells[2..].iter().all(|v| v.len() == 64));
        assert_eq!(meta_row(&json!({"t": "phone", "h": "abc"}), &meta_schema(false)), vec!["".to_string(), "abc".to_string()]);

        let g = keys(Platform::Google, &p, false);
        assert_eq!(g[1].hash, sha256_hex("+15035550100"));
        assert_eq!(google_member(&json!({"t": "phone", "h": "abc"})), json!({"userData": {"userIdentifiers": [{"phoneNumber": "abc"}]}}));

        // TikTok is email only.
        assert_eq!(keys(Platform::Tiktok, &p, false).len(), 1);
        // A phone number that wasn't opted in isn't sent, but it still vetoes.
        let untrusted = Person { phone_trusted: false, ..p.clone() };
        assert_eq!(keys(Platform::Meta, &untrusted, false).len(), 1);
        assert_eq!(keys(Platform::Meta, &untrusted, true).len(), 2);
        let none = Person { email: None, ..untrusted };
        assert!(keys(Platform::Google, &none, false).is_empty());
    }

    #[test]
    fn aliases_share_a_key_on_google_only() {
        let a = Person { email: Some("a.b+x@gmail.com".into()), ..Default::default() };
        let b = Person { email: Some("ab@gmail.com".into()), ..Default::default() };
        assert_eq!(keys(Platform::Google, &a, false)[0].hash, keys(Platform::Google, &b, false)[0].hash);
        assert_ne!(keys(Platform::Meta, &a, false)[0].hash, keys(Platform::Meta, &b, false)[0].hash);
        assert_ne!(keys(Platform::Google, &a, false)[0].src_check, keys(Platform::Google, &b, false)[0].src_check);
    }
}

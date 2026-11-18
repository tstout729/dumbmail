//! Checking that an incoming webhook really came from the app that's supposed to send it.

use std::collections::HashMap;

use axum::http::HeaderMap;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Signature {
    /// Anyone with the URL can post. The URL itself is the secret.
    #[default]
    None,
    /// An HMAC of the raw body (or `timestamp.body`) in a header.
    Hmac {
        #[serde(default)]
        algorithm: Algorithm,
        /// The header holding the signature, e.g. `X-Hub-Signature-256` or `Stripe-Signature`.
        header: String,
        #[serde(default)]
        format: Format,
        /// `plain` format only: how the signature is written.
        #[serde(default)]
        encoding: Encoding,
        /// `plain` format only: text before the signature, e.g. `sha256=`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// `plain` format only: sign `timestamp.body`, taking the timestamp (Unix seconds) from this header.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_header: Option<String>,
        /// Reject timestamps further than this from now. Default 300.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tolerance_seconds: Option<i64>,
        /// `plain` format only: `utf8` (default) uses the secret's text as the key; `base64` decodes it first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_encoding: Option<Encoding>,
        /// `plain` format with `timestamp_header` only: what joins the timestamp and the body. Default
        /// `.`; Webflow uses `:` and Zendesk nothing (`""`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_separator: Option<String>,
        /// `plain` format with `timestamp_header` only: how the timestamp is written. Default `seconds`
        /// (Unix); Webflow sends `milliseconds`; `iso8601` for a date-time like `2026-11-17T18:00:00Z`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_unit: Option<TimestampUnit>,
        /// `plain` format only: sign the webhook URL (as given to the app) followed by the body, as
        /// Square does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sign_url: Option<bool>,
    },
    /// The secret itself, sent in a header or a query parameter.
    SharedSecret {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[default]
    Sha256,
    Sha1,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// The whole header is the signature (after an optional prefix).
    #[default]
    Plain,
    /// `t=<unix>,v1=<hex>` over `t.body`, as Stripe and Calendly send.
    Stripe,
    /// Svix / Standard Webhooks: `v1,<base64>` over `id.timestamp.body` with a `whsec_` secret, as Guesty sends.
    Svix,
    /// `ts=<unix>;h1=<hex>` over `ts:body`, as Paddle sends.
    Paddle,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TimestampUnit {
    #[default]
    Seconds,
    Milliseconds,
    Iso8601,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Hex,
    Base64,
    Utf8,
}

impl Signature {
    pub fn parse(v: &serde_json::Value) -> AppResult<Self> {
        let s: Signature = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid signature settings: {e}")))?;
        match &s {
            Signature::Hmac { header, format, encoding, prefix, timestamp_header, tolerance_seconds, secret_encoding, algorithm, timestamp_separator, timestamp_unit, sign_url } => {
                check_header_name(header)?;
                if let Some(h) = timestamp_header {
                    check_header_name(h)?;
                }
                if *encoding == Encoding::Utf8 {
                    return Err(AppError::bad("signature encoding must be hex or base64"));
                }
                let plain_only = prefix.is_some() || timestamp_header.is_some() || secret_encoding.is_some() || sign_url.is_some();
                if *format != Format::Plain && plain_only {
                    return Err(AppError::bad("prefix, timestamp_header, secret_encoding and sign_url only apply to the plain format"));
                }
                if (timestamp_separator.is_some() || timestamp_unit.is_some()) && timestamp_header.is_none() {
                    return Err(AppError::bad("timestamp_separator and timestamp_unit go with timestamp_header"));
                }
                if timestamp_separator.as_ref().is_some_and(|s| s.len() > 4) {
                    return Err(AppError::bad("timestamp_separator is at most 4 characters"));
                }
                if *sign_url == Some(true) && timestamp_header.is_some() {
                    return Err(AppError::bad("sign_url and timestamp_header can't be combined"));
                }
                if matches!(format, Format::Svix | Format::Paddle) && *algorithm != Algorithm::Sha256 {
                    return Err(AppError::bad("the svix and paddle formats always use sha256"));
                }
                if prefix.as_ref().is_some_and(|p| p.len() > 40) {
                    return Err(AppError::bad("signature prefix is too long"));
                }
                if tolerance_seconds.is_some_and(|t| !(10..=172_800).contains(&t)) {
                    return Err(AppError::bad("tolerance_seconds must be 10 to 172800 (48 hours)"));
                }
            }
            Signature::SharedSecret { header, query } => match (header, query) {
                (Some(h), None) => check_header_name(h)?,
                (None, Some(q)) if !q.is_empty() && q.len() <= 64 => {}
                _ => return Err(AppError::bad("shared_secret needs exactly one of `header` or `query`")),
            },
            Signature::None => {}
        }
        Ok(s)
    }

    pub fn needs_secret(&self) -> bool {
        !matches!(self, Signature::None)
    }

    /// Headers that hold the secret itself and must not be stored with the delivery.
    pub fn secret_header(&self) -> Option<String> {
        match self {
            Signature::SharedSecret { header: Some(h), .. } => Some(h.to_ascii_lowercase()),
            _ => None,
        }
    }

    pub fn secret_query(&self) -> Option<&str> {
        match self {
            Signature::SharedSecret { query: Some(q), .. } => Some(q),
            _ => None,
        }
    }
}

fn check_header_name(h: &str) -> AppResult<()> {
    if h.is_empty() || h.len() > 64 || axum::http::HeaderName::from_bytes(h.as_bytes()).is_err() {
        return Err(AppError::bad(format!("{h:?} is not a valid header name")));
    }
    Ok(())
}

/// The key a check signs with, from the saved secret: the same step [`verify`] takes first, so
/// "ready" (and saving) can say whether a request could ever verify.
fn signing_key(sig: &Signature, secret: Option<&str>) -> Result<Vec<u8>, String> {
    let secret = match (sig, secret) {
        (Signature::None, _) => return Ok(Vec::new()),
        (_, Some(s)) if !s.is_empty() => s,
        _ => return Err("this integration checks signatures but has no signing secret saved".into()),
    };
    match sig {
        Signature::Hmac { format: Format::Svix, .. } => {
            b64(secret.strip_prefix("whsec_").unwrap_or(secret)).filter(|k| !k.is_empty()).ok_or_else(|| "the signing secret should look like whsec_ followed by base64".into())
        }
        Signature::Hmac { format: Format::Plain, secret_encoding: Some(Encoding::Base64), .. } => b64(secret).filter(|k| !k.is_empty()).ok_or_else(|| "the signing secret isn't valid base64".into()),
        _ => Ok(secret.as_bytes().to_vec()),
    }
}

/// Whether this secret works with these settings (see [`signing_key`]).
pub fn check_secret(sig: &Signature, secret: Option<&str>) -> Result<(), String> {
    signing_key(sig, secret).map(|_| ())
}

/// Check a request. `url` is the webhook URL as the app was given it (for `sign_url`); `now` is
/// Unix seconds.
pub fn verify(sig: &Signature, secret: Option<&str>, headers: &HeaderMap, query: &HashMap<String, String>, url: &str, body: &[u8], now: i64) -> Result<(), String> {
    if matches!(sig, Signature::None) {
        return Ok(());
    }
    let key = signing_key(sig, secret)?;
    let secret = secret.unwrap_or_default();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    match sig {
        Signature::None => Ok(()),
        Signature::SharedSecret { header: h, query: q } => {
            let got = match (h, q) {
                (Some(h), _) => header(h),
                (None, Some(q)) => query.get(q).map(String::as_str),
                _ => None,
            }
            .ok_or("the shared secret is missing")?;
            // Compare digests so the comparison takes the same time whatever the lengths.
            if constant_eq(&Sha256::digest(got.as_bytes()), &Sha256::digest(secret.as_bytes())) {
                Ok(())
            } else {
                Err("the shared secret doesn't match".into())
            }
        }
        Signature::Hmac { algorithm, header: name, format, encoding, prefix, timestamp_header, tolerance_seconds, secret_encoding: _, timestamp_separator, timestamp_unit, sign_url } => {
            let raw = header(name).ok_or_else(|| format!("the {name} header is missing"))?;
            let tolerance = tolerance_seconds.unwrap_or(300);
            let unit = timestamp_unit.unwrap_or_default();
            let fresh = |ts: &str| -> Result<(), String> {
                let t: i64 = match unit {
                    TimestampUnit::Seconds => ts.trim().parse().map_err(|_| "the signature timestamp isn't a number".to_string())?,
                    TimestampUnit::Milliseconds => ts.trim().parse::<i64>().map_err(|_| "the signature timestamp isn't a number".to_string())?.div_euclid(1000),
                    TimestampUnit::Iso8601 => chrono::DateTime::parse_from_rfc3339(ts.trim()).map_err(|_| "the signature timestamp isn't an ISO 8601 date-time".to_string())?.timestamp(),
                };
                // Overflow-safe: an extreme timestamp is simply too old (or too new).
                if now.checked_sub(t).and_then(i64::checked_abs).is_none_or(|d| d > tolerance) {
                    return Err(format!("the signature timestamp is more than {tolerance} seconds old (or in the future)"));
                }
                Ok(())
            };
            match format {
                Format::Plain => {
                    let mut text = raw;
                    if let Some(p) = prefix {
                        text = strip_prefix_ci(text, p).ok_or_else(|| format!("the signature doesn't start with {p:?}"))?;
                    }
                    let given = match encoding {
                        Encoding::Base64 => b64(text),
                        _ => hex::decode(text.to_ascii_lowercase()).ok(),
                    }
                    .ok_or("the signature isn't encoded the way the settings say")?;
                    let signed = match timestamp_header {
                        Some(th) => {
                            let ts = header(th).ok_or_else(|| format!("the {th} header is missing"))?;
                            fresh(ts)?;
                            [ts.as_bytes(), timestamp_separator.as_deref().unwrap_or(".").as_bytes(), body].concat()
                        }
                        None if *sign_url == Some(true) => [url.as_bytes(), body].concat(),
                        None => body.to_vec(),
                    };
                    if mac_matches(*algorithm, &key, &signed, &given) { Ok(()) } else { Err("the signature doesn't match".into()) }
                }
                Format::Stripe => {
                    let mut ts = None;
                    let mut sigs = Vec::new();
                    for part in raw.split(',') {
                        match part.trim().split_once('=') {
                            Some(("t", v)) => ts = Some(v),
                            Some(("v1", v)) => sigs.push(v),
                            _ => {}
                        }
                    }
                    let ts = ts.ok_or("the signature header has no t= timestamp")?;
                    fresh(ts)?;
                    let signed = [ts.as_bytes(), b".", body].concat();
                    let ok = sigs.iter().filter_map(|s| hex::decode(s).ok()).any(|given| mac_matches(*algorithm, secret.as_bytes(), &signed, &given));
                    if ok { Ok(()) } else { Err("the signature doesn't match".into()) }
                }
                Format::Paddle => {
                    let mut ts = None;
                    let mut sigs = Vec::new();
                    for part in raw.split(';') {
                        match part.trim().split_once('=') {
                            Some(("ts", v)) => ts = Some(v),
                            Some(("h1", v)) => sigs.push(v),
                            _ => {}
                        }
                    }
                    let ts = ts.ok_or("the signature header has no ts= timestamp")?;
                    fresh(ts)?;
                    let signed = [ts.as_bytes(), b":", body].concat();
                    let ok = sigs.iter().filter_map(|s| hex::decode(s).ok()).any(|given| mac_matches(Algorithm::Sha256, secret.as_bytes(), &signed, &given));
                    if ok { Ok(()) } else { Err("the signature doesn't match".into()) }
                }
                Format::Svix => {
                    let base = if name.to_ascii_lowercase().starts_with("svix-") { "svix" } else { "webhook" };
                    let id = header(&format!("{base}-id")).ok_or_else(|| format!("the {base}-id header is missing"))?;
                    let ts = header(&format!("{base}-timestamp")).ok_or_else(|| format!("the {base}-timestamp header is missing"))?;
                    fresh(ts)?;
                    let signed = [id.as_bytes(), b".", ts.as_bytes(), b".", body].concat();
                    let ok = raw
                        .split_whitespace()
                        .filter_map(|s| s.strip_prefix("v1,"))
                        .filter_map(b64)
                        .any(|given| mac_matches(Algorithm::Sha256, &key, &signed, &given));
                    if ok { Ok(()) } else { Err("the signature doesn't match".into()) }
                }
            }
        }
    }
}

fn b64(s: &str) -> Option<Vec<u8>> {
    let e = base64::engine::general_purpose::STANDARD;
    e.decode(s.trim()).ok().or_else(|| base64::engine::general_purpose::URL_SAFE.decode(s.trim()).ok())
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    (s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p)).then(|| &s[p.len()..])
}

/// Constant-time HMAC check.
fn mac_matches(algorithm: Algorithm, key: &[u8], signed: &[u8], given: &[u8]) -> bool {
    match algorithm {
        Algorithm::Sha256 => {
            let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key");
            m.update(signed);
            m.verify_slice(given).is_ok()
        }
        Algorithm::Sha1 => {
            let mut m = Hmac::<sha1::Sha1>::new_from_slice(key).expect("hmac takes any key");
            m.update(signed);
            m.verify_slice(given).is_ok()
        }
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A short hint for a saved secret, never the secret itself.
pub fn hint(secret: &str) -> String {
    let n = secret.chars().count();
    if n >= 12 {
        let tail: String = secret.chars().skip(n - 4).collect();
        format!("…{tail}")
    } else {
        "saved".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
        m.update(data);
        m.finalize().into_bytes().to_vec()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[test]
    fn plain_hex_with_prefix() {
        let sig = Signature::parse(&json!({"type": "hmac", "header": "X-Hub-Signature-256", "prefix": "sha256="})).unwrap();
        let body = br#"{"a":1}"#;
        let good = format!("sha256={}", hex::encode(hmac256(b"s3cret", body)));
        let q = HashMap::new();
        assert!(verify(&sig, Some("s3cret"), &headers(&[("x-hub-signature-256", &good)]), &q, "", body, 0).is_ok());
        assert!(verify(&sig, Some("other"), &headers(&[("x-hub-signature-256", &good)]), &q, "", body, 0).is_err());
        assert!(verify(&sig, Some("s3cret"), &headers(&[("x-hub-signature-256", &good)]), &q, "", b"{\"a\":2}", 0).is_err());
        assert!(verify(&sig, Some("s3cret"), &headers(&[]), &q, "", body, 0).unwrap_err().contains("missing"));
        assert!(verify(&sig, None, &headers(&[("x-hub-signature-256", &good)]), &q, "", body, 0).unwrap_err().contains("no signing secret"));
    }

    #[test]
    fn plain_base64_sha1_and_timestamp() {
        let sig = Signature::parse(&json!({"type": "hmac", "algorithm": "sha1", "header": "X-Sig", "encoding": "base64"})).unwrap();
        let mut m = Hmac::<sha1::Sha1>::new_from_slice(b"k").unwrap();
        m.update(b"body");
        let s = base64::engine::general_purpose::STANDARD.encode(m.finalize().into_bytes());
        assert!(verify(&sig, Some("k"), &headers(&[("x-sig", &s)]), &HashMap::new(), "", b"body", 0).is_ok());

        let sig = Signature::parse(&json!({"type": "hmac", "header": "X-Sig", "timestamp_header": "X-Ts", "tolerance_seconds": 60})).unwrap();
        let s = hex::encode(hmac256(b"k", b"1000.body"));
        let h = headers(&[("x-sig", &s), ("x-ts", "1000")]);
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", 1030).is_ok());
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", 1100).unwrap_err().contains("seconds"));
        // Extreme timestamps fail cleanly instead of overflowing.
        for ts in [i64::MIN.to_string(), i64::MAX.to_string()] {
            let s = hex::encode(hmac256(b"k", format!("{ts}.body").as_bytes()));
            let h = headers(&[("x-sig", &s), ("x-ts", &ts)]);
            assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", 1030).unwrap_err().contains("seconds"));
        }
        let ts = i64::MIN.to_string();
        let s = hex::encode(hmac256(b"k", format!("{ts}.body").as_bytes()));
        assert!(verify(&sig, Some("k"), &headers(&[("x-sig", &s), ("x-ts", &ts)]), &HashMap::new(), "", b"body", i64::MAX).is_err());
    }

    #[test]
    fn stripe_and_calendly_format() {
        let sig = Signature::parse(&json!({"type": "hmac", "header": "Stripe-Signature", "format": "stripe"})).unwrap();
        let body = b"{\"id\":\"evt_1\"}";
        let v1 = hex::encode(hmac256(b"whsec_abc", &[b"1700000000.".as_slice(), body].concat()));
        let h = headers(&[("stripe-signature", &format!("t=1700000000,v1=deadbeef,v1={v1}"))]);
        assert!(verify(&sig, Some("whsec_abc"), &h, &HashMap::new(), "", body, 1700000100).is_ok());
        assert!(verify(&sig, Some("whsec_abc"), &h, &HashMap::new(), "", body, 1700001000).is_err(), "too old");
        assert!(verify(&sig, Some("whsec_xyz"), &h, &HashMap::new(), "", body, 1700000100).is_err());
    }

    #[test]
    fn svix_format() {
        let sig = Signature::parse(&json!({"type": "hmac", "header": "svix-signature", "format": "svix"})).unwrap();
        let key = b"0123456789abcdef0123456789abcdef";
        let secret = format!("whsec_{}", base64::engine::general_purpose::STANDARD.encode(key));
        let body = b"{\"event\":\"reservation.new\"}";
        let mac = base64::engine::general_purpose::STANDARD.encode(hmac256(key, &[b"msg_1.1700000000.".as_slice(), body].concat()));
        let h = headers(&[("svix-id", "msg_1"), ("svix-timestamp", "1700000000"), ("svix-signature", &format!("v1,bm9wZQ== v1,{mac}"))]);
        assert!(verify(&sig, Some(&secret), &h, &HashMap::new(), "", body, 1700000010).is_ok());
        let h2 = headers(&[("svix-id", "msg_2"), ("svix-timestamp", "1700000000"), ("svix-signature", &format!("v1,{mac}"))]);
        assert!(verify(&sig, Some(&secret), &h2, &HashMap::new(), "", body, 1700000010).is_err(), "id is signed");
    }

    #[test]
    fn paddle_format() {
        let sig = Signature::parse(&json!({"type": "hmac", "header": "Paddle-Signature", "format": "paddle"})).unwrap();
        let body = b"{\"event_type\":\"transaction.completed\"}";
        let h1 = hex::encode(hmac256(b"pdl_ntfset_abc", &[b"1671552777:".as_slice(), body].concat()));
        let h = headers(&[("paddle-signature", &format!("ts=1671552777;h1=00ff;h1={h1}"))]);
        assert!(verify(&sig, Some("pdl_ntfset_abc"), &h, &HashMap::new(), "", body, 1671552800).is_ok());
        assert!(verify(&sig, Some("pdl_ntfset_abc"), &h, &HashMap::new(), "", body, 1671553800).is_err(), "too old");
        assert!(verify(&sig, Some("pdl_ntfset_xyz"), &h, &HashMap::new(), "", body, 1671552800).is_err());
        // Stripe's separators don't pass for Paddle's.
        let h = headers(&[("paddle-signature", &format!("ts=1671552777,h1={h1}"))]);
        assert!(verify(&sig, Some("pdl_ntfset_abc"), &h, &HashMap::new(), "", body, 1671552800).is_err());
    }

    #[test]
    fn square_signs_the_url() {
        // Square's own example: key asdf1234, URL https://example.com/webhook, body {"hello":"world"}.
        let sig = Signature::parse(&json!({"type": "hmac", "header": "x-square-hmacsha256-signature", "encoding": "base64", "sign_url": true})).unwrap();
        let h = headers(&[("x-square-hmacsha256-signature", "2kRE5qRU2tR+tBGlDwMEw2avJ7QM4ikPYD/PJ3bd9Og=")]);
        let body = br#"{"hello":"world"}"#;
        assert!(verify(&sig, Some("asdf1234"), &h, &HashMap::new(), "https://example.com/webhook", body, 0).is_ok());
        assert!(verify(&sig, Some("asdf1234"), &h, &HashMap::new(), "https://example.com/other", body, 0).is_err(), "the URL is signed");
        assert!(Signature::parse(&json!({"type": "hmac", "header": "X", "sign_url": true, "timestamp_header": "T"})).is_err());
    }

    #[test]
    fn timestamp_separator_and_units() {
        // Webflow: hex HMAC of "<ms>:<body>", 5 minutes.
        let sig = Signature::parse(&json!({"type": "hmac", "header": "x-webflow-signature", "timestamp_header": "x-webflow-timestamp", "timestamp_separator": ":", "timestamp_unit": "milliseconds"})).unwrap();
        let s = hex::encode(hmac256(b"k", b"1722370035277:body"));
        let h = headers(&[("x-webflow-signature", &s), ("x-webflow-timestamp", "1722370035277")]);
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", 1722370100).is_ok());
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", 1722370035 + 400).is_err(), "too old");
        let s = hex::encode(hmac256(b"k", b"1722370035277.body"));
        let h = headers(&[("x-webflow-signature", &s), ("x-webflow-timestamp", "1722370035277")]);
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", 1722370100).is_err(), "separator is signed");
        // Zendesk: base64 HMAC of timestamp + body, no separator, ISO 8601 timestamp.
        let sig = Signature::parse(&json!({"type": "hmac", "header": "X-Zendesk-Webhook-Signature", "encoding": "base64", "timestamp_header": "X-Zendesk-Webhook-Signature-Timestamp", "timestamp_separator": "", "timestamp_unit": "iso8601"})).unwrap();
        let s = base64::engine::general_purpose::STANDARD.encode(hmac256(b"k", b"2026-11-17T18:00:00Zbody"));
        let h = headers(&[("x-zendesk-webhook-signature", &s), ("x-zendesk-webhook-signature-timestamp", "2026-11-17T18:00:00Z")]);
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-17T18:01:00Z").unwrap().timestamp();
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", now).is_ok());
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", now + 3600).is_err(), "too old");
        let h = headers(&[("x-zendesk-webhook-signature", &s), ("x-zendesk-webhook-signature-timestamp", "1763402400")]);
        assert!(verify(&sig, Some("k"), &h, &HashMap::new(), "", b"body", now).is_err(), "not ISO 8601");
        assert!(Signature::parse(&json!({"type": "hmac", "header": "X", "timestamp_separator": ":"})).is_err(), "needs timestamp_header");
    }

    #[test]
    fn empty_keys_never_verify() {
        // "whsec_" with nothing after it decodes to an empty key; an HMAC with an empty key is
        // something anyone can compute, so it's refused.
        let sig = Signature::parse(&json!({"type": "hmac", "header": "svix-signature", "format": "svix"})).unwrap();
        let body = b"{}";
        let mac = base64::engine::general_purpose::STANDARD.encode(hmac256(b"", &[b"msg_1.1700000000.".as_slice(), body].concat()));
        let h = headers(&[("svix-id", "msg_1"), ("svix-timestamp", "1700000000"), ("svix-signature", &format!("v1,{mac}"))]);
        assert!(verify(&sig, Some("whsec_"), &h, &HashMap::new(), "", body, 1700000010).is_err());
        let sig = Signature::parse(&json!({"type": "hmac", "header": "X-Sig", "secret_encoding": "base64"})).unwrap();
        let h = headers(&[("x-sig", &hex::encode(hmac256(b"", body)))]);
        assert!(verify(&sig, Some("="), &h, &HashMap::new(), "", body, 0).is_err());
    }

    #[test]
    fn shared_secret_header_or_query() {
        let sig = Signature::parse(&json!({"type": "shared_secret", "header": "X-Token"})).unwrap();
        assert!(verify(&sig, Some("tok"), &headers(&[("x-token", "tok")]), &HashMap::new(), "", b"", 0).is_ok());
        assert!(verify(&sig, Some("tok"), &headers(&[("x-token", "tok2")]), &HashMap::new(), "", b"", 0).is_err());
        let sig = Signature::parse(&json!({"type": "shared_secret", "query": "key"})).unwrap();
        let q = HashMap::from([("key".to_string(), "tok".to_string())]);
        assert!(verify(&sig, Some("tok"), &HeaderMap::new(), &q, "", b"", 0).is_ok());
        assert_eq!(sig.secret_query(), Some("key"));
    }

    #[test]
    fn validation() {
        assert!(Signature::parse(&json!({"type": "hmac"})).is_err());
        assert!(Signature::parse(&json!({"type": "hmac", "header": "X", "bogus": 1})).is_err());
        assert!(Signature::parse(&json!({"type": "hmac", "header": "bad header"})).is_err());
        assert!(Signature::parse(&json!({"type": "shared_secret"})).is_err());
        assert!(Signature::parse(&json!({"type": "shared_secret", "header": "X", "query": "y"})).is_err());
        assert!(Signature::parse(&json!({"type": "hmac", "header": "X", "format": "stripe", "prefix": "a"})).is_err());
        assert_eq!(Signature::parse(&json!({"type": "none"})).unwrap(), Signature::None);
        assert_eq!(hint("whsec_0123456789"), "…6789");
        assert_eq!(hint("short"), "saved");
    }
}

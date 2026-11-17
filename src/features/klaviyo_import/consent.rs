//! Klaviyo consent → what the import may do to a DumbMail profile.
//!
//! The rule: after an import nobody is easier to reach than they were in Klaviyo, and nobody's own
//! restriction recorded in DumbMail is weakened. Consent belongs to an address or a number, not to a
//! profile: Klaviyo's email consent is applied only to the same email address, its SMS consent only to the
//! same phone number.
//!
//! This module turns Klaviyo's record into an *incoming* state per channel. The merge with what the
//! DumbMail profile already has happens inside one statement in `profiles.rs` that locks the profile
//! first, so a STOP or unsubscribe committed while the import runs can't be overwritten:
//!
//! | Incoming (same address/number) | DumbMail never subscribed | subscribed by an earlier import | subscribed in DumbMail | unsubscribed | pending | suppressed |
//! | --- | --- | --- | --- | --- | --- | --- |
//! | opt-out (unsubscribed, spam) | unsubscribed | unsubscribed | unsubscribed | unsubscribed | unsubscribed | unsubscribed, stays suppressed |
//! | opt-in (subscribed, reachable) | subscribed | subscribed | subscribed | unchanged | unchanged | unchanged |
//! | never subscribed, no record, unknown, SMS "can't receive" | unchanged | never subscribed (unknown: held) | unchanged | unchanged | unchanged | unchanged |
//!
//! "Subscribed by an earlier import" means the import's own record (`klaviyo_consent_writes`) says the
//! import set that subscription for this exact address or number: it was Klaviyo's opt-in, so what
//! Klaviyo says now governs it. Only a subscription the import didn't write counts as the person's own
//! DumbMail opt-in.
//!
//! A Klaviyo suppression (bounce, complaint, invalid, by hand, a reason DumbMail doesn't recognise, or
//! "Klaviyo can't email them") is added to the profile; one already there is never removed. Klaviyo's
//! opt-outs and suppressions also apply to any other profile here that has that exact address or number.
//! Unknown or unrecognised consent (a blank SMS state included), and (when the customer chooses it)
//! never subscribed, set `marketing_hold`, which only the person opting in clears.

use serde_json::{json, Value};

/// Klaviyo's view of one profile's email marketing, as the API reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KlaviyoEmail {
    /// SUBSCRIBED | UNSUBSCRIBED | NEVER_SUBSCRIBED; anything else is unknown.
    pub consent: String,
    pub can_receive: bool,
    /// (reason, timestamp): HARD_BOUNCE | INVALID_EMAIL | SPAM_COMPLAINT | UNSUBSCRIBE | USER_SUPPRESSED, or a
    /// reason added since (which blocks too).
    pub suppressions: Vec<(String, Option<String>)>,
    /// (list id, reason, timestamp)
    pub list_suppressions: Vec<(String, String, Option<String>)>,
    pub timestamp: Option<String>,
    pub last_updated: Option<String>,
    pub method: Option<String>,
    pub method_detail: Option<String>,
    pub double_optin: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct KlaviyoSms {
    pub consent: String,
    pub can_receive: bool,
    pub timestamp: Option<String>,
    pub last_updated: Option<String>,
    pub method: Option<String>,
    pub method_detail: Option<String>,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).filter(|x| !x.is_empty())
}

pub fn read_email(subs: &Value) -> Option<KlaviyoEmail> {
    let m = subs.get("email")?.get("marketing")?;
    if !m.is_object() {
        return None;
    }
    let suppressions = match &m["suppression"] {
        Value::Array(a) => a.iter().filter_map(|x| x["reason"].as_str().map(|r| (r.to_uppercase(), s(&x["timestamp"])))).collect(),
        _ => Vec::new(),
    };
    let list_suppressions = match &m["list_suppressions"] {
        Value::Array(a) => a.iter().filter_map(|x| Some((x["list_id"].as_str()?.to_string(), x["reason"].as_str().unwrap_or("").to_uppercase(), s(&x["timestamp"])))).collect(),
        _ => Vec::new(),
    };
    Some(KlaviyoEmail {
        consent: m["consent"].as_str().unwrap_or("").trim().to_uppercase(),
        can_receive: m["can_receive_email_marketing"].as_bool().unwrap_or(false),
        suppressions,
        list_suppressions,
        timestamp: s(&m["consent_timestamp"]),
        last_updated: s(&m["last_updated"]),
        method: s(&m["method"]),
        method_detail: s(&m["method_detail"]).or_else(|| s(&m["custom_method_detail"])),
        double_optin: m["double_optin"].as_bool(),
    })
}

pub fn read_sms(subs: &Value) -> Option<KlaviyoSms> {
    let m = subs.get("sms")?.get("marketing")?;
    if !m.is_object() {
        return None;
    }
    Some(KlaviyoSms {
        consent: m["consent"].as_str().unwrap_or("").trim().to_uppercase(),
        can_receive: m["can_receive_sms_marketing"].as_bool().unwrap_or(false),
        timestamp: s(&m["consent_timestamp"]),
        last_updated: s(&m["last_updated"]),
        method: s(&m["method"]),
        method_detail: s(&m["method_detail"]),
    })
}

/// Lists this person unsubscribed from in Klaviyo (they must not be on them here).
pub fn list_suppressions(subs: &Value) -> Vec<String> {
    read_email(subs).map(|e| e.list_suppressions.into_iter().map(|(l, _, _)| l).collect()).unwrap_or_default()
}

/// What Klaviyo's email record asks of the import, for an address it matches exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct EmailIn {
    /// subscribed | unsubscribed | never | unknown
    pub state: &'static str,
    /// The DumbMail suppression to add, if any.
    pub suppressed: Option<&'static str>,
    pub hold: bool,
}

/// What Klaviyo's SMS record asks of the import, for a number it matches exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct SmsIn {
    /// subscribed | unsubscribed | unreachable (subscribed, but Klaviyo can't text it) | never | none (no
    /// record) | unknown (blank or unrecognised)
    pub state: &'static str,
    pub hold: bool,
}

/// `hold_never`: the customer chose "hold back people who never subscribed until they opt in".
pub fn email_in(k: Option<&KlaviyoEmail>, hold_never: bool) -> EmailIn {
    let Some(k) = k else {
        // No consent record at all: unknown. Never sendable on its own, and held.
        return EmailIn { state: "unknown", suppressed: None, hold: true };
    };
    let has = |r: &str| k.suppressions.iter().any(|(x, _)| x == r);
    let mut state = match k.consent.as_str() {
        "SUBSCRIBED" => "subscribed",
        "UNSUBSCRIBED" => "unsubscribed",
        "NEVER_SUBSCRIBED" => "never",
        _ => "unknown",
    };
    // An unsubscribe or a spam complaint is an opt-out whatever the consent field says.
    if has("UNSUBSCRIBE") || has("SPAM_COMPLAINT") {
        state = "unsubscribed";
    }
    let unrecognised = k.suppressions.iter().any(|(r, _)| !matches!(r.as_str(), "SPAM_COMPLAINT" | "HARD_BOUNCE" | "INVALID_EMAIL" | "USER_SUPPRESSED" | "UNSUBSCRIBE"));
    let mut suppressed = if has("SPAM_COMPLAINT") {
        Some("complaint")
    } else if has("HARD_BOUNCE") {
        Some("hard_bounce")
    } else if has("INVALID_EMAIL") {
        Some("invalid_email")
    } else if has("USER_SUPPRESSED") {
        Some("manual")
    } else if unrecognised {
        // A suppression DumbMail doesn't know yet still blocks, whatever the reachability flag says.
        Some("klaviyo_suppressed")
    } else {
        None
    };
    // Klaviyo says it wouldn't send them marketing email and nothing above stops DumbMail: suppress.
    if !k.can_receive && state != "unsubscribed" && suppressed.is_none() {
        suppressed = Some("klaviyo_suppressed");
    }
    let hold = state == "unknown" || (state == "never" && hold_never);
    EmailIn { state, suppressed, hold }
}

/// Only a subscribed, reachable number Klaviyo could text comes in as subscribed; nothing else ever makes
/// a number textable. A record whose consent is blank or a value DumbMail doesn't recognise is unknown.
pub fn sms_in(k: Option<&KlaviyoSms>, has_phone: bool) -> SmsIn {
    let Some(k) = k else { return SmsIn { state: "none", hold: false } };
    let state = match k.consent.as_str() {
        "SUBSCRIBED" if k.can_receive && has_phone => "subscribed",
        "SUBSCRIBED" => "unreachable",
        "UNSUBSCRIBED" => "unsubscribed",
        "NEVER_SUBSCRIBED" => "never",
        _ => "unknown",
    };
    SmsIn { state, hold: has_phone && state == "unknown" }
}

/// Consent for a brand-new DumbMail profile.
pub fn new_email_consent(e: &EmailIn) -> &'static str {
    match e.state {
        "subscribed" => "subscribed",
        "unsubscribed" => "unsubscribed",
        _ => "never_subscribed",
    }
}

pub fn new_sms_consent(s: &SmsIn) -> &'static str {
    match s.state {
        "subscribed" => "subscribed",
        "unsubscribed" => "unsubscribed",
        _ => "never_subscribed",
    }
}

/// Klaviyo's consent record, kept on the profile as the `klaviyo_consent` property: every suppression and
/// list suppression with its reason and time, and the method and time of each opt-in.
pub fn provenance(email: Option<&KlaviyoEmail>, sms: Option<&KlaviyoSms>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(e) = email {
        out.insert(
            "email".into(),
            json!({"status": e.consent, "can_receive": e.can_receive, "at": e.timestamp, "last_updated": e.last_updated, "method": e.method,
                   "method_detail": e.method_detail, "double_optin": e.double_optin,
                   "suppressions": e.suppressions.iter().map(|(r, t)| json!({"reason": r, "at": t})).collect::<Vec<_>>(),
                   "list_suppressions": e.list_suppressions.iter().map(|(l, r, t)| json!({"list_id": l, "reason": r, "at": t})).collect::<Vec<_>>()}),
        );
    }
    if let Some(s) = sms {
        out.insert("sms".into(), json!({"status": s.consent, "can_receive": s.can_receive, "at": s.timestamp, "last_updated": s.last_updated, "method": s.method, "method_detail": s.method_detail}));
    }
    Value::Object(out)
}

/// `a***@example.com`: enough to tell two addresses apart in a report, not the whole address.
pub fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((local, domain)) => format!("{}***@{domain}", local.chars().next().map(String::from).unwrap_or_default()),
        None => "***".into(),
    }
}

/// `+1•••••0101`
pub fn mask_phone(p: &str) -> String {
    let digits: Vec<char> = p.chars().collect();
    if digits.len() <= 6 {
        return "•••".into();
    }
    let head: String = digits[..2].iter().collect();
    let tail: String = digits[digits.len() - 4..].iter().collect();
    format!("{head}•••••{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(consent: &str, can: bool, sup: &[&str]) -> KlaviyoEmail {
        KlaviyoEmail { consent: consent.into(), can_receive: can, suppressions: sup.iter().map(|s| (s.to_string(), None)).collect(), ..Default::default() }
    }

    #[test]
    fn email_cases() {
        let st = |c: &str, can: bool, sup: &[&str]| email_in(Some(&email(c, can, sup)), false);
        assert_eq!(st("SUBSCRIBED", true, &[]), EmailIn { state: "subscribed", suppressed: None, hold: false });
        assert_eq!(st("NEVER_SUBSCRIBED", true, &[]), EmailIn { state: "never", suppressed: None, hold: false });
        assert!(email_in(Some(&email("NEVER_SUBSCRIBED", true, &[])), true).hold, "the hold-back choice holds never-subscribed people");
        assert_eq!(st("UNSUBSCRIBED", false, &[]), EmailIn { state: "unsubscribed", suppressed: None, hold: false });
        assert_eq!(st("UNSUBSCRIBED", false, &["UNSUBSCRIBE"]), EmailIn { state: "unsubscribed", suppressed: None, hold: false });
        assert_eq!(st("SUBSCRIBED", false, &["UNSUBSCRIBE"]).state, "unsubscribed", "an UNSUBSCRIBE suppression is an opt-out");
        assert_eq!(st("SUBSCRIBED", false, &["HARD_BOUNCE"]), EmailIn { state: "subscribed", suppressed: Some("hard_bounce"), hold: false });
        assert_eq!(st("NEVER_SUBSCRIBED", false, &["INVALID_EMAIL"]), EmailIn { state: "never", suppressed: Some("invalid_email"), hold: false });
        assert_eq!(st("SUBSCRIBED", false, &["USER_SUPPRESSED"]).suppressed, Some("manual"));
        assert_eq!(st("SUBSCRIBED", false, &["SPAM_COMPLAINT", "HARD_BOUNCE"]), EmailIn { state: "unsubscribed", suppressed: Some("complaint"), hold: false });
        assert_eq!(st("SUBSCRIBED", false, &[]).suppressed, Some("klaviyo_suppressed"));
        assert_eq!(st("NEVER_SUBSCRIBED", false, &[]).suppressed, Some("klaviyo_suppressed"));
        assert_eq!(st("SOMETHING_NEW", true, &[]), EmailIn { state: "unknown", suppressed: None, hold: true });
        assert_eq!(st("SUBSCRIBED", true, &["NEW_REASON"]), EmailIn { state: "subscribed", suppressed: Some("klaviyo_suppressed"), hold: false }, "an unrecognised suppression blocks even when Klaviyo says reachable");
        assert_eq!(email_in(None, false), EmailIn { state: "unknown", suppressed: None, hold: true });
    }

    #[test]
    fn sms_cases() {
        let k = |c: &str, can: bool| KlaviyoSms { consent: c.into(), can_receive: can, ..Default::default() };
        assert_eq!(sms_in(Some(&k("SUBSCRIBED", true)), true).state, "subscribed");
        assert_eq!(sms_in(Some(&k("SUBSCRIBED", true)), false).state, "unreachable", "no usable number");
        assert_eq!(sms_in(Some(&k("SUBSCRIBED", false)), true).state, "unreachable", "Klaviyo couldn't text it");
        assert_eq!(sms_in(Some(&k("UNSUBSCRIBED", false)), true).state, "unsubscribed");
        assert_eq!(sms_in(Some(&k("NEVER_SUBSCRIBED", false)), true), SmsIn { state: "never", hold: false });
        assert_eq!(sms_in(None, true), SmsIn { state: "none", hold: false });
        assert_eq!(sms_in(Some(&k("ODD", true)), true), SmsIn { state: "unknown", hold: true });
        assert_eq!(sms_in(Some(&k("", true)), true), SmsIn { state: "unknown", hold: true }, "a present but blank state is unknown");
    }

    #[test]
    fn reads_the_api_shape() {
        let subs = json!({
            "email": {"marketing": {"can_receive_email_marketing": false, "consent": "SUBSCRIBED", "consent_timestamp": "2024-01-02T03:04:05+00:00",
                "method": "FORM", "method_detail": "Footer", "suppression": [{"reason": "HARD_BOUNCE", "timestamp": "2025-01-01T00:00:00+00:00"}],
                "list_suppressions": [{"list_id": "L1", "reason": "USER_SUPPRESSED", "timestamp": "2025-01-01T00:00:00+00:00"}]}},
            "sms": {"marketing": {"can_receive_sms_marketing": true, "consent": "SUBSCRIBED", "method": "TEXT"}}
        });
        let e = read_email(&subs).unwrap();
        assert_eq!(e.suppressions, vec![("HARD_BOUNCE".to_string(), Some("2025-01-01T00:00:00+00:00".to_string()))]);
        assert_eq!(e.method.as_deref(), Some("FORM"));
        assert_eq!(list_suppressions(&subs), vec!["L1"]);
        assert!(read_sms(&subs).unwrap().can_receive);
        assert!(read_email(&json!({})).is_none());
        let p = provenance(Some(&e), None);
        assert_eq!(p["email"]["suppressions"][0]["at"], "2025-01-01T00:00:00+00:00");
        assert_eq!(p["email"]["list_suppressions"][0]["list_id"], "L1");
    }

    #[test]
    fn masks() {
        assert_eq!(mask_email("ann@example.com"), "a***@example.com");
        assert_eq!(mask_phone("+15035550101"), "+1•••••0101");
    }
}

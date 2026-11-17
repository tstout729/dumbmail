//! Klaviyo consent → DumbMail consent.
//!
//! The rule: nobody becomes more contactable than they were in Klaviyo, and an unsubscribe or a
//! suppression on either side always wins. Every case is spelled out here and tested below.

use serde_json::{json, Value};

/// How reachable a consent state is in DumbMail, lowest first. `never_subscribed` can still get email
/// campaigns in DumbMail (as in Klaviyo); `pending` gets only the confirmation email.
fn email_rank(s: &str) -> u8 {
    match s {
        "unsubscribed" => 0,
        "pending" => 1,
        "never_subscribed" => 2,
        "subscribed" => 3,
        _ => 0,
    }
}

/// Klaviyo's view of one profile's email marketing, as the API reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KlaviyoEmail {
    /// SUBSCRIBED | UNSUBSCRIBED | NEVER_SUBSCRIBED (anything else is treated as unknown).
    pub consent: String,
    pub can_receive: bool,
    /// HARD_BOUNCE | INVALID_EMAIL | SPAM_COMPLAINT | UNSUBSCRIBE | USER_SUPPRESSED
    pub suppressions: Vec<String>,
    pub timestamp: Option<String>,
    pub method: Option<String>,
    pub method_detail: Option<String>,
    pub double_optin: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct KlaviyoSms {
    pub consent: String,
    pub can_receive: bool,
    pub timestamp: Option<String>,
    pub method: Option<String>,
    pub method_detail: Option<String>,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).filter(|x| !x.is_empty())
}

pub fn read_email(subs: &Value) -> Option<KlaviyoEmail> {
    let m = subs.get("email")?.get("marketing")?;
    let suppressions = match &m["suppression"] {
        Value::Array(a) => a.iter().filter_map(|x| x["reason"].as_str().map(|r| r.to_uppercase())).collect(),
        _ => Vec::new(),
    };
    Some(KlaviyoEmail {
        consent: m["consent"].as_str().unwrap_or("").to_uppercase(),
        can_receive: m["can_receive_email_marketing"].as_bool().unwrap_or(false),
        suppressions,
        timestamp: s(&m["consent_timestamp"]),
        method: s(&m["method"]),
        method_detail: s(&m["method_detail"]).or_else(|| s(&m["custom_method_detail"])),
        double_optin: m["double_optin"].as_bool(),
    })
}

pub fn read_sms(subs: &Value) -> Option<KlaviyoSms> {
    let m = subs.get("sms")?.get("marketing")?;
    Some(KlaviyoSms {
        consent: m["consent"].as_str().unwrap_or("").to_uppercase(),
        can_receive: m["can_receive_sms_marketing"].as_bool().unwrap_or(false),
        timestamp: s(&m["consent_timestamp"]),
        method: s(&m["method"]),
        method_detail: s(&m["method_detail"]),
    })
}

/// Lists this person unsubscribed from in Klaviyo (they must not be added back to them).
pub fn list_suppressions(subs: &Value) -> Vec<String> {
    match &subs["email"]["marketing"]["list_suppressions"] {
        Value::Array(a) => a.iter().filter_map(|x| x["list_id"].as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

/// What DumbMail stores for email: consent plus a suppression reason (no email at all while set).
#[derive(Debug, Clone, PartialEq)]
pub struct EmailState {
    pub consent: &'static str,
    pub suppressed: Option<&'static str>,
}

/// Map Klaviyo's email state on its own (no existing DumbMail profile).
pub fn map_email(k: &KlaviyoEmail) -> EmailState {
    let mut consent = match k.consent.to_uppercase().as_str() {
        "SUBSCRIBED" => "subscribed",
        "UNSUBSCRIBED" => "unsubscribed",
        // NEVER_SUBSCRIBED and anything unknown: never subscribed. The safety net below catches an
        // unknown state Klaviyo itself wouldn't send to.
        _ => "never_subscribed",
    };
    // A suppression reason, strongest first.
    let has = |r: &str| k.suppressions.iter().any(|x| x.eq_ignore_ascii_case(r));
    let mut suppressed = if has("SPAM_COMPLAINT") {
        Some("complaint")
    } else if has("HARD_BOUNCE") {
        Some("hard_bounce")
    } else if has("INVALID_EMAIL") {
        Some("invalid_email")
    } else if has("USER_SUPPRESSED") {
        Some("manual")
    } else {
        None
    };
    if has("UNSUBSCRIBE") || has("SPAM_COMPLAINT") {
        consent = "unsubscribed";
    }
    // Klaviyo says it wouldn't send them marketing email, but nothing above stops DumbMail from doing
    // so: keep them out with a suppression of their own.
    let reachable = consent != "unsubscribed" && suppressed.is_none();
    if !k.can_receive && reachable {
        suppressed = Some("klaviyo_suppressed");
    }
    EmailState { consent, suppressed }
}

/// Combine with what the DumbMail profile already has: the less reachable consent wins, except that a
/// real opt-in on either side beats "never subscribed"; any unsubscribe or suppression is kept.
pub fn merge_email(existing_consent: Option<&str>, existing_suppressed: Option<&str>, k: &EmailState) -> (String, Option<String>) {
    let consent = match existing_consent {
        None => k.consent.to_string(),
        Some(e) => {
            if e == "unsubscribed" || k.consent == "unsubscribed" {
                "unsubscribed".to_string()
            } else if e == "pending" && k.consent != "subscribed" {
                "pending".to_string()
            } else if email_rank(k.consent) > email_rank(e) {
                k.consent.to_string()
            } else {
                e.to_string()
            }
        }
    };
    let suppressed = existing_suppressed.map(String::from).or_else(|| k.suppressed.map(String::from));
    (consent, suppressed)
}

/// Map Klaviyo's SMS state. DumbMail texts only `subscribed` people, so anything Klaviyo couldn't text
/// must not land as `subscribed`; a subscriber Klaviyo can't text (carrier deactivation, landline) is
/// imported as unsubscribed. A phone number is required to be subscribed.
pub fn map_sms(k: &KlaviyoSms, has_phone: bool) -> &'static str {
    match k.consent.to_uppercase().as_str() {
        "SUBSCRIBED" if k.can_receive && has_phone => "subscribed",
        "SUBSCRIBED" if has_phone => "unsubscribed",
        "UNSUBSCRIBED" => "unsubscribed",
        _ => "never_subscribed",
    }
}

pub fn merge_sms(existing: Option<&str>, k: &str) -> String {
    match existing {
        None => k.to_string(),
        Some("unsubscribed") => "unsubscribed".into(),
        Some(_) if k == "unsubscribed" => "unsubscribed".into(),
        Some("subscribed") => "subscribed".into(),
        Some(e) if k == "subscribed" && e != "pending" => "subscribed".into(),
        Some(e) => e.to_string(),
    }
}

/// Klaviyo's consent record, kept on the profile as the `klaviyo_consent` property so the timestamp and
/// method of every opt-in stay visible after the move.
pub fn provenance(email: Option<&KlaviyoEmail>, sms: Option<&KlaviyoSms>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(e) = email {
        out.insert(
            "email".into(),
            json!({"status": e.consent, "can_receive": e.can_receive, "at": e.timestamp, "method": e.method, "method_detail": e.method_detail,
                   "double_optin": e.double_optin, "suppressions": e.suppressions}),
        );
    }
    if let Some(s) = sms {
        out.insert("sms".into(), json!({"status": s.consent, "can_receive": s.can_receive, "at": s.timestamp, "method": s.method, "method_detail": s.method_detail}));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(consent: &str, can: bool, sup: &[&str]) -> KlaviyoEmail {
        KlaviyoEmail { consent: consent.into(), can_receive: can, suppressions: sup.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn reachable(s: &EmailState) -> bool {
        s.consent != "unsubscribed" && s.consent != "pending" && s.suppressed.is_none()
    }

    #[test]
    fn email_cases() {
        let st = |c: &str, can: bool, sup: &[&str]| map_email(&email(c, can, sup));
        assert_eq!(st("SUBSCRIBED", true, &[]), EmailState { consent: "subscribed", suppressed: None });
        assert_eq!(st("NEVER_SUBSCRIBED", true, &[]), EmailState { consent: "never_subscribed", suppressed: None });
        assert_eq!(st("UNSUBSCRIBED", false, &[]), EmailState { consent: "unsubscribed", suppressed: None });
        assert_eq!(st("UNSUBSCRIBED", false, &["UNSUBSCRIBE"]), EmailState { consent: "unsubscribed", suppressed: None });
        assert_eq!(st("SUBSCRIBED", false, &["HARD_BOUNCE"]), EmailState { consent: "subscribed", suppressed: Some("hard_bounce") });
        assert_eq!(st("NEVER_SUBSCRIBED", false, &["INVALID_EMAIL"]), EmailState { consent: "never_subscribed", suppressed: Some("invalid_email") });
        assert_eq!(st("SUBSCRIBED", false, &["USER_SUPPRESSED"]), EmailState { consent: "subscribed", suppressed: Some("manual") });
        assert_eq!(st("SUBSCRIBED", false, &["SPAM_COMPLAINT", "HARD_BOUNCE"]), EmailState { consent: "unsubscribed", suppressed: Some("complaint") });
        // Klaviyo won't send but gives no reason we know: suppressed all the same.
        assert_eq!(st("SUBSCRIBED", false, &[]), EmailState { consent: "subscribed", suppressed: Some("klaviyo_suppressed") });
        assert_eq!(st("SOMETHING_NEW", false, &[]), EmailState { consent: "never_subscribed", suppressed: Some("klaviyo_suppressed") });
        assert_eq!(st("SOMETHING_NEW", true, &[]), EmailState { consent: "never_subscribed", suppressed: None });
        assert_eq!(st("subscribed", true, &[]).consent, "subscribed", "case-insensitive");
    }

    #[test]
    fn email_never_more_reachable_than_klaviyo() {
        let consents = ["SUBSCRIBED", "UNSUBSCRIBED", "NEVER_SUBSCRIBED", "OTHER"];
        let sups: [&[&str]; 6] = [&[], &["HARD_BOUNCE"], &["SPAM_COMPLAINT"], &["UNSUBSCRIBE"], &["USER_SUPPRESSED"], &["INVALID_EMAIL"]];
        for c in consents {
            for sup in sups {
                for can in [true, false] {
                    let k = email(c, can, sup);
                    let m = map_email(&k);
                    if !can {
                        assert!(!reachable(&m), "{c} {sup:?} can={can} became reachable: {m:?}");
                    }
                    if c == "NEVER_SUBSCRIBED" {
                        assert_ne!(m.consent, "subscribed");
                    }
                    if !sup.is_empty() && *sup != ["UNSUBSCRIBE"] {
                        assert!(m.suppressed.is_some(), "{sup:?} must stay suppressed");
                    }
                    for existing in [None, Some("subscribed"), Some("unsubscribed"), Some("never_subscribed"), Some("pending")] {
                        for ex_sup in [None, Some("hard_bounce")] {
                            let (mc, ms) = merge_email(existing, ex_sup, &m);
                            if existing == Some("unsubscribed") || m.consent == "unsubscribed" {
                                assert_eq!(mc, "unsubscribed");
                            }
                            if ex_sup.is_some() || m.suppressed.is_some() {
                                assert!(ms.is_some());
                            }
                            if mc == "subscribed" {
                                assert!(existing == Some("subscribed") || m.consent == "subscribed");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn email_merge_examples() {
        let sub = EmailState { consent: "subscribed", suppressed: None };
        let never = EmailState { consent: "never_subscribed", suppressed: None };
        assert_eq!(merge_email(Some("never_subscribed"), None, &sub), ("subscribed".into(), None));
        assert_eq!(merge_email(Some("subscribed"), None, &never), ("subscribed".into(), None));
        assert_eq!(merge_email(Some("unsubscribed"), None, &sub), ("unsubscribed".into(), None));
        assert_eq!(merge_email(Some("pending"), None, &never), ("pending".into(), None));
        assert_eq!(merge_email(Some("pending"), None, &sub), ("subscribed".into(), None));
        assert_eq!(merge_email(Some("subscribed"), Some("complaint"), &sub), ("subscribed".into(), Some("complaint".into())));
    }

    #[test]
    fn sms_cases() {
        let k = |c: &str, can: bool| KlaviyoSms { consent: c.into(), can_receive: can, ..Default::default() };
        assert_eq!(map_sms(&k("SUBSCRIBED", true), true), "subscribed");
        assert_eq!(map_sms(&k("SUBSCRIBED", true), false), "never_subscribed");
        assert_eq!(map_sms(&k("SUBSCRIBED", false), true), "unsubscribed");
        assert_eq!(map_sms(&k("UNSUBSCRIBED", false), true), "unsubscribed");
        assert_eq!(map_sms(&k("NEVER_SUBSCRIBED", false), true), "never_subscribed");
        assert_eq!(map_sms(&k("", false), true), "never_subscribed");
        assert_eq!(merge_sms(Some("unsubscribed"), "subscribed"), "unsubscribed");
        assert_eq!(merge_sms(Some("subscribed"), "unsubscribed"), "unsubscribed");
        assert_eq!(merge_sms(Some("subscribed"), "never_subscribed"), "subscribed");
        assert_eq!(merge_sms(Some("never_subscribed"), "subscribed"), "subscribed");
        assert_eq!(merge_sms(Some("pending"), "subscribed"), "pending");
        assert_eq!(merge_sms(None, "never_subscribed"), "never_subscribed");
    }

    #[test]
    fn reads_the_api_shape() {
        let subs = serde_json::json!({
            "email": {"marketing": {"can_receive_email_marketing": false, "consent": "SUBSCRIBED", "consent_timestamp": "2024-01-02T03:04:05+00:00",
                "method": "FORM", "method_detail": "Footer", "suppression": [{"reason": "HARD_BOUNCE", "timestamp": "2025-01-01T00:00:00+00:00"}],
                "list_suppressions": [{"list_id": "L1", "reason": "USER_SUPPRESSED", "timestamp": "2025-01-01T00:00:00+00:00"}]}},
            "sms": {"marketing": {"can_receive_sms_marketing": true, "consent": "SUBSCRIBED", "method": "TEXT"}}
        });
        let e = read_email(&subs).unwrap();
        assert_eq!(e.suppressions, vec!["HARD_BOUNCE"]);
        assert_eq!(e.method.as_deref(), Some("FORM"));
        assert_eq!(list_suppressions(&subs), vec!["L1"]);
        assert!(read_sms(&subs).unwrap().can_receive);
        assert!(read_email(&serde_json::json!({})).is_none());
    }
}

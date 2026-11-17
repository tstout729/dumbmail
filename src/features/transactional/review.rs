//! Simple rules that keep marketing out of the transactional API. Transactional mail skips
//! marketing consent, so a promotion sent through it would reach people who never agreed to
//! promotions. Receipts mention prices and discounts, so single words never block; promotional
//! calls to action do.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

/// Phrases that sell rather than inform. Two different ones block a send; one is a warning.
static PROMO: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    [
        ("percent off", r"\b\d{1,2}\s?%\s?off\b"),
        ("shop now", r"\bshop\s+(now|today|the\s+sale)\b"),
        ("buy now", r"\bbuy\s+(now|today)\b"),
        ("limited time", r"\blimited[-\s]time\b"),
        ("promo code", r"\b(use|enter|apply)\s+(the\s+)?(promo\s+|discount\s+|coupon\s+)?code\b"),
        ("sale", r"\b(flash|summer|winter|spring|holiday|black\s+friday|cyber\s+monday|clearance)\s+sale\b"),
        ("don't miss out", r"\bdon'?t\s+miss\s+(out|this)\b"),
        ("exclusive offer", r"\bexclusive\s+(offer|deal|discount)s?\b"),
        ("save up to", r"\bsave\s+up\s+to\b"),
        ("ends soon", r"\b(offer|sale|deal)s?\s+ends?\s+(soon|tonight|today|tomorrow)\b"),
        ("free shipping offer", r"\bfree\s+shipping\s+on\s+(all\s+)?orders\s+over\b"),
        ("new arrivals", r"\b(new\s+arrivals|just\s+dropped|new\s+collection)\b"),
    ]
    .into_iter()
    .map(|(label, re)| (label, Regex::new(&format!("(?i){re}")).expect("promo regex")))
    .collect()
});

/// What the rules found in a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Review {
    /// Promotional phrases found (distinct).
    pub flags: Vec<String>,
    /// True when the message reads as marketing and can't go through the transactional API.
    pub blocked: bool,
}

/// All the text a message would show: subject, preheader, blocks, raw html and text.
fn text_of(content: &Value) -> String {
    let mut out = String::new();
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.iter().filter(|(k, _)| !matches!(k.as_str(), "src" | "href" | "image" | "color" | "type" | "align")).for_each(|(_, x)| walk(x, out)),
            _ => {}
        }
    }
    walk(content, &mut out);
    out
}

pub fn review(content: &Value) -> Review {
    static TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
    // A marketing unsubscribe link anywhere (attributes and block links included) means marketing.
    static UNSUB: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{\{\s*unsubscribe_url\s*\}\}").unwrap());
    let text = TAGS.replace_all(&text_of(content), " ").replace("&nbsp;", " ").replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\"");
    let mut flags: Vec<String> = PROMO.iter().filter(|(_, re)| re.is_match(&text)).map(|(l, _)| l.to_string()).collect();
    let unsubscribe = UNSUB.is_match(&content.to_string());
    if unsubscribe {
        flags.push("unsubscribe link".into());
    }
    let blocked = flags.len() >= 2 || unsubscribe;
    Review { flags, blocked }
}

/// The message a blocked review returns to the caller.
pub fn blocked_message(r: &Review) -> String {
    format!(
        "This reads like marketing ({}), so it can't go through the transactional API, which skips marketing consent. Send it as a campaign or flow instead.",
        r.flags.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn receipts_pass_promotions_dont() {
        let receipt = json!({"subject": "Your order #1042", "blocks": [{"type": "text", "html": "<p>Thanks! Subtotal $40.00, Discount (SAVE10) -$4.00, Total $36.00. Free shipping applied.</p>"}]});
        assert_eq!(review(&receipt), Review::default());
        let reset = json!({"subject": "Reset your password", "html": "<p>Click <a href=\"https://x.test/r?sale=1\">here</a> to reset.</p>"});
        assert!(!review(&reset).blocked);
        let one = json!({"subject": "Your order shipped", "text": "It's on the way. Limited time: track it below."});
        let r = review(&one);
        assert_eq!(r.flags, vec!["limited time"]);
        assert!(!r.blocked, "one phrase is a warning");
        let promo = json!({"subject": "20% off everything!", "blocks": [{"type": "button", "text": "Shop now", "href": "https://x.test"}]});
        let r = review(&promo);
        assert!(r.blocked, "{r:?}");
        assert_eq!(r.flags, vec!["percent off", "shop now"]);
        let unsub = json!({"html": "<a href=\"{{ unsubscribe_url }}\">Unsubscribe</a>"});
        assert!(review(&unsub).blocked, "a marketing unsubscribe link means it's marketing");
        let unsub = json!({"blocks": [{"type": "button", "text": "Opt out", "href": "{{unsubscribe_url}}"}]});
        assert!(review(&unsub).blocked);
        // Rendered HTML escapes apostrophes; the rules still see them.
        let rendered = json!({"html": "<p>Don&#39;t miss out on our exclusive offer</p>"});
        assert!(review(&rendered).blocked);
    }
}

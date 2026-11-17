//! Simple rules that keep marketing out of the transactional API. Transactional mail skips
//! marketing consent, so a promotion sent through it would reach people who never agreed to
//! promotions. One clearly promotional thing is enough to refuse a message: discount or offer
//! language, a "shop now" style call to action, links to several products, or a marketing
//! unsubscribe link. Receipts can still show prices, totals and an applied discount amount.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

/// Phrases that sell rather than inform. Any one of them refuses the message.
static PROMO: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    [
        ("percent off", r"\b\d{1,3}\s?%\s?off\b"),
        ("money off", r"[$€£]\s?\d+(?:[.,]\d{2})?\s+off\b"),
        ("off your next order", r"\boff\s+your\s+next\s+(order|purchase|visit)\b"),
        ("shop now", r"\bshop\s+(now|today|the\s+(sale|collection|new))\b"),
        ("buy now", r"\bbuy\s+(now|today)\b"),
        ("limited time", r"\blimited[-\s]time\b"),
        ("promo code", r"\b(promo|coupon)\s+code\b|\b(use|enter|apply)\s+(the\s+|our\s+)?discount\s+code\b|\b(use|enter|apply)\s+code\s+\S+\s+at\s+checkout\b"),
        ("sale", r"\b(flash|summer|winter|spring|fall|autumn|holiday|black\s+friday|cyber\s+monday|clearance|end\s+of\s+season)\s+sale\b"),
        ("don't miss out", r"\bdon'?t\s+miss\s+(out|this)\b"),
        ("exclusive offer", r"\bexclusive\s+(offer|deal|discount)s?\b"),
        ("special offer", r"\bspecial\s+(offer|deal)s?\b"),
        ("save up to", r"\bsave\s+up\s+to\b"),
        ("ends soon", r"\b(offer|sale|deal)s?\s+ends?\s+(soon|tonight|today|tomorrow)\b"),
        ("free shipping offer", r"\bfree\s+shipping\s+on\s+(all\s+)?orders\s+over\b"),
        ("new arrivals", r"\b(new\s+arrivals|just\s+dropped|new\s+collection)\b"),
    ]
    .into_iter()
    .map(|(label, re)| (label, Regex::new(&format!("(?i){re}")).expect("promo regex")))
    .collect()
});

/// Links to this many different product or collection pages read as a catalogue, not a receipt.
pub const PRODUCT_LINKS: usize = 3;

/// What the rules found in a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Review {
    /// Promotional things found (distinct).
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

/// Distinct links to product or collection pages anywhere in the message (block links and html).
fn product_links(content: &Value) -> usize {
    static HREF: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)href\s*=\s*(?:\\?["'])([^"'\\\s>]+)"#).unwrap());
    static PRODUCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)/(products?|collections?|shop|item|p)/[^/?#\s]+").unwrap());
    let mut urls: HashSet<String> = HashSet::new();
    fn walk(v: &Value, urls: &mut HashSet<String>) {
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, urls)),
            Value::Object(o) => {
                for (k, x) in o {
                    match (k.as_str(), x) {
                        ("href", Value::String(s)) => {
                            urls.insert(s.trim().to_string());
                        }
                        _ => walk(x, urls),
                    }
                }
            }
            _ => {}
        }
    }
    walk(content, &mut urls);
    for c in HREF.captures_iter(&content.to_string()) {
        urls.insert(c[1].to_string());
    }
    urls.iter().filter_map(|u| PRODUCT.find(u).map(|m| m.as_str().to_lowercase())).collect::<HashSet<_>>().len()
}

pub fn review(content: &Value) -> Review {
    static TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
    // A marketing unsubscribe link anywhere (attributes and block links included) means marketing.
    static UNSUB: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{\{\s*unsubscribe_url\s*\}\}").unwrap());
    let text = TAGS.replace_all(&text_of(content), " ").replace("&nbsp;", " ").replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\"").replace("&#36;", "$");
    let mut flags: Vec<String> = PROMO.iter().filter(|(_, re)| re.is_match(&text)).map(|(l, _)| l.to_string()).collect();
    if product_links(content) >= PRODUCT_LINKS {
        flags.push("links to several products".into());
    }
    if UNSUB.is_match(&content.to_string()) {
        flags.push("unsubscribe link".into());
    }
    let blocked = !flags.is_empty();
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
    fn receipts_and_codes_pass() {
        let receipt = json!({"subject": "Your order #1042", "blocks": [{"type": "text", "html": "<p>Thanks! Subtotal $40.00, Discount (SAVE10) -$4.00, Total $36.00. Free shipping applied.</p>"}]});
        assert_eq!(review(&receipt), Review::default());
        let reset = json!({"subject": "Reset your password", "html": "<p>Click <a href=\"https://x.test/r?sale=1\">here</a> to reset.</p>"});
        assert!(!review(&reset).blocked);
        let code = json!({"subject": "Your sign-in code", "text": "Enter the code 482913 to sign in. It expires in 10 minutes."});
        assert!(!review(&code).blocked, "{:?}", review(&code));
        let two_items = json!({"html": "<a href=\"https://x.test/products/mug\">Mug</a> <a href=\"https://x.test/products/tee\">Tee</a>"});
        assert!(!review(&two_items).blocked, "a receipt can link the items ordered");
        let shipped = json!({"subject": "Order shipped", "text": "Your order now ships today. Discount code SAVE10 applied. Track it below."});
        assert!(!review(&shipped).blocked);
    }

    #[test]
    fn one_promotional_thing_is_enough() {
        for (content, flag) in [
            (json!({"subject": "Your order shipped", "text": "It's on the way. Limited time: track it below."}), "limited time"),
            (json!({"subject": "Thanks!", "text": "Here's 15% off."}), "percent off"),
            (json!({"text": "Come back for a treat off your next visit."}), "off your next order"),
            (json!({"html": "<p>Get $10 off when you come back</p>"}), "money off"),
            (json!({"blocks": [{"type": "button", "text": "Shop now", "href": "https://x.test"}]}), "shop now"),
            (json!({"text": "Use promo code WELCOME"}), "promo code"),
            (json!({"html": "<p>Don&#39;t miss out</p>"}), "don't miss out"),
        ] {
            let r = review(&content);
            assert!(r.blocked, "{content}: {r:?}");
            assert_eq!(r.flags, vec![flag], "{content}");
        }
        let catalogue = json!({"blocks": [
            {"type": "button", "text": "A", "href": "https://x.test/products/a"},
            {"type": "button", "text": "B", "href": "https://x.test/products/b"},
            {"type": "text", "html": "<a href=\"https://x.test/collections/new\">C</a>"}]});
        assert_eq!(review(&catalogue).flags, vec!["links to several products"]);
        let unsub = json!({"html": "<a href=\"{{ unsubscribe_url }}\">Unsubscribe</a>"});
        assert!(review(&unsub).blocked, "a marketing unsubscribe link means it's marketing");
        let unsub = json!({"blocks": [{"type": "button", "text": "Opt out", "href": "{{unsubscribe_url}}"}]});
        assert!(review(&unsub).blocked);
    }
}

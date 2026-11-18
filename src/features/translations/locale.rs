//! Language tags (BCP 47): checking and normalizing them, matching a person's language to the
//! versions a message has, reading `Accept-Language`, and which languages are written right to left.

/// The longest tag accepted.
pub const MAX_LEN: usize = 35;

/// A valid tag in its usual form (`fr`, `fr-CA`, `pt-BR`, `zh-Hant-TW`, `sr-Latn`), or None.
/// Underscores (`pt_BR`) are accepted; case is normalized (language lower, script title, region upper).
pub fn normalize(raw: &str) -> Option<String> {
    let t = raw.trim().replace('_', "-");
    if t.is_empty() || t.len() > MAX_LEN || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let mut parts = t.split('-');
    let lang = parts.next()?;
    if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut out = vec![lang.to_ascii_lowercase()];
    let mut stage = 0; // 0: script allowed, 1: region allowed, 2: variants only
    let mut extra = 0;
    for p in parts {
        if p.is_empty() {
            return None;
        }
        if stage == 0 && p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic()) {
            let mut s = p.to_ascii_lowercase();
            s[..1].make_ascii_uppercase();
            out.push(s);
            stage = 1;
        } else if stage <= 1 && ((p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic())) || (p.len() == 3 && p.chars().all(|c| c.is_ascii_digit()))) {
            out.push(p.to_ascii_uppercase());
            stage = 2;
        } else if (5..=8).contains(&p.len()) || (p.len() == 4 && p.starts_with(|c: char| c.is_ascii_digit())) {
            extra += 1;
            if extra > 2 {
                return None;
            }
            out.push(p.to_ascii_lowercase());
            stage = 2;
        } else {
            return None;
        }
    }
    Some(out.join("-"))
}

/// The language part of a tag (`fr` for `fr-CA`).
pub fn primary(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// Whether a tag has more than its language part (a region or script).
pub fn has_region(tag: &str) -> bool {
    tag.contains('-')
}

/// Languages written right to left.
pub fn is_rtl(tag: &str) -> bool {
    let lower = tag.to_ascii_lowercase();
    if lower.split('-').any(|p| p == "arab" || p == "hebr" || p == "thaa" || p == "syrc") {
        return true;
    }
    matches!(primary(&lower), "ar" | "he" | "iw" | "fa" | "ur" | "ps" | "sd" | "ug" | "yi" | "dv" | "ckb" | "ku" | "syr")
}

/// The version of `available` that suits someone whose language is `wanted`: the same tag, else the
/// bare language (`fr` for `fr-CA`), else another region of the same language. None: the default.
pub fn best_match<'a>(wanted: &str, available: &'a [String]) -> Option<&'a String> {
    let w = normalize(wanted)?;
    if let Some(a) = available.iter().find(|a| a.eq_ignore_ascii_case(&w)) {
        return Some(a);
    }
    let p = primary(&w);
    available.iter().find(|a| a.eq_ignore_ascii_case(p)).or_else(|| available.iter().find(|a| primary(a).eq_ignore_ascii_case(p)))
}

/// The languages in an `Accept-Language` header, most preferred first (at most 10; `*` and bad tags left out).
pub fn accept_language(header: &str) -> Vec<String> {
    let mut v: Vec<(String, f32, usize)> = header
        .split(',')
        .take(20)
        .enumerate()
        .filter_map(|(i, part)| {
            let mut it = part.split(';');
            let tag = normalize(it.next()?.trim())?;
            let q = it.find_map(|p| p.trim().strip_prefix("q=").and_then(|q| q.trim().parse::<f32>().ok())).unwrap_or(1.0);
            (q > 0.0).then_some((tag, q, i))
        })
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.2.cmp(&b.2)));
    v.into_iter().take(10).map(|(t, _, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_normalized() {
        assert_eq!(normalize("fr").as_deref(), Some("fr"));
        assert_eq!(normalize(" FR-ca ").as_deref(), Some("fr-CA"));
        assert_eq!(normalize("pt_br").as_deref(), Some("pt-BR"));
        assert_eq!(normalize("zh-hant-tw").as_deref(), Some("zh-Hant-TW"));
        assert_eq!(normalize("es-419").as_deref(), Some("es-419"));
        for bad in ["", "f", "french", "fr--CA", "fr-CA-", "fr CA", "*", "x-private", "fr-<b>", "en-US-aaaaa-bbbbb-ccccc", &"a".repeat(40)] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
    }

    #[test]
    fn matching_prefers_exact_then_language() {
        let v = vec!["fr".to_string(), "pt-BR".to_string(), "es-MX".to_string()];
        assert_eq!(best_match("fr-CA", &v).map(String::as_str), Some("fr"));
        assert_eq!(best_match("pt-BR", &v).map(String::as_str), Some("pt-BR"));
        assert_eq!(best_match("pt-PT", &v).map(String::as_str), Some("pt-BR"));
        assert_eq!(best_match("es", &v).map(String::as_str), Some("es-MX"));
        assert_eq!(best_match("de", &v), None);
        let both = vec!["fr-CA".to_string(), "fr".to_string()];
        assert_eq!(best_match("fr-CA", &both).map(String::as_str), Some("fr-CA"));
        assert_eq!(best_match("fr-BE", &both).map(String::as_str), Some("fr"));
    }

    #[test]
    fn accept_language_orders_by_quality() {
        assert_eq!(accept_language("de;q=0.5, fr-CA, *;q=0.1, en;q=0.8"), vec!["fr-CA", "en", "de"]);
        assert!(accept_language("").is_empty());
    }

    #[test]
    fn rtl_languages() {
        assert!(is_rtl("ar") && is_rtl("he-IL") && is_rtl("fa") && is_rtl("az-Arab"));
        assert!(!is_rtl("fr") && !is_rtl("en-US"));
    }
}

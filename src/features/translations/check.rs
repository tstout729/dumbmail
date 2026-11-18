//! What the check before sending says about language versions: text that isn't translated, is out
//! of date or is an unreviewed machine draft (all warnings: the default text goes out where a
//! translation is missing or out of date, and drafts go out as drafted), and each language's own
//! problems (a subject that's too long or shouts, preview text that's too long, a version that
//! can't be rendered). Localized links are checked with the email's other links.

use serde_json::Value;

use super::fields::{self, MACHINE};
use crate::models::Account;
use crate::state::AppState;

pub type Finding = (&'static str, &'static str, String, String);

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// A language's name for people ("French (fr)"); the tag when unknown.
pub fn label(tag: &str) -> String {
    let name = match super::locale::primary(tag) {
        "en" => "English",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        "ar" => "Arabic",
        "he" => "Hebrew",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "ko" => "Korean",
        "pl" => "Polish",
        "sv" => "Swedish",
        "da" => "Danish",
        "no" | "nb" => "Norwegian",
        "fi" => "Finnish",
        "tr" => "Turkish",
        "ru" => "Russian",
        "uk" => "Ukrainian",
        "cs" => "Czech",
        "el" => "Greek",
        "hi" => "Hindi",
        "fa" => "Persian",
        _ => return tag.to_string(),
    };
    if super::locale::has_region(tag) { format!("{name} ({tag})") } else { name.to_string() }
}

/// Findings for `content`'s language versions (none when it has none).
pub async fn findings(state: &AppState, account: &Account, content: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    let (default, langs) = fields::languages(content);
    if content.get("translations").is_none() || langs.is_empty() {
        return out;
    }
    let default_name = label(default.as_deref().unwrap_or("en"));
    let own = super::own_hosts(state, Some(account));
    for lang in &langs {
        let name = label(lang);
        let sum = fields::summary(content, lang);
        let n = |k: &str| sum.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        if n("missing") > 0 {
            out.push(("translation_missing", "warning", format!("{name}: {} not translated", plural(n("missing"), "text is", "texts are")), format!("People who get the {name} version see the {default_name} text there.")));
        }
        if n("stale") > 0 {
            out.push(("translation_stale", "warning", format!("{name}: {} out of date", plural(n("stale"), "translation is", "translations are")), format!("The {default_name} text changed after it was translated, so the {default_name} text is sent there until you update the translation.")));
        }
        if n(MACHINE) > 0 {
            out.push(("translation_unreviewed", "warning", format!("{name}: {} not reviewed", plural(n(MACHINE), "machine draft is", "machine drafts are")), "They'll be sent as drafted. Have someone read them and mark them reviewed.".to_string()));
        }
        let local = fields::localize(content, lang, &own);
        let subject = local.get("subject").and_then(|v| v.as_str()).unwrap_or("").trim();
        if subject.chars().count() > 60 {
            out.push(("translation_subject_long", "warning", format!("{name}: shorten the subject line"), format!("It's {} characters; phones show about 40 and desktops about 60.", subject.chars().count())));
        }
        let letters: Vec<char> = subject.chars().filter(|c| c.is_alphabetic()).collect();
        if letters.len() >= 8 && letters.iter().filter(|c| c.is_uppercase()).count() * 10 >= letters.len() * 7 {
            out.push(("translation_subject_caps", "warning", format!("{name}: don't write the subject in capitals"), "All-caps subjects look like shouting and spam filters count them against you.".to_string()));
        }
        if subject.contains("!!") || subject.contains("$$") {
            out.push(("translation_subject_punctuation", "warning", format!("{name}: drop the repeated punctuation in the subject"), "\"!!\" and \"$$\" are classic spam signals.".to_string()));
        }
        let pre = local.get("preheader").and_then(|v| v.as_str()).unwrap_or("").trim();
        if pre.chars().count() > 120 {
            out.push(("translation_preheader_long", "info", format!("{name}: preview text is long"), "Inboxes show 40 to 120 characters of it; the rest is cut off.".to_string()));
        }
        match crate::features::content::prepare::check_sendable_now(state, account, &local).await {
            Ok(Err(e)) => out.push(("translation_render_error", "warning", format!("The {name} version can't be rendered"), format!("{e} People who'd get it are sent nothing until it's fixed."))),
            Err(_) => {}
            Ok(Ok(())) => {
                // Content without the content module's blocks isn't rendered by the check above.
                if !crate::features::content::prepare::uses_content_features(&local) && local.get("subject").is_some() {
                    let ctx = crate::features::content::prepare::sample_ctx(account, None);
                    if let Ok(ec) = serde_json::from_value::<crate::render::EmailContent>(local.clone()) {
                        if let Err(e) = crate::render::render_email(account, &ec, &ctx, None) {
                            out.push(("translation_render_error", "warning", format!("The {name} version can't be rendered"), e.public_message()));
                        }
                    }
                }
            }
        }
    }
    out
}

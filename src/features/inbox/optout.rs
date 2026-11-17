//! Reading what an inbound text means for consent. Opt-out is decided first, before anything else
//! looks at the text, and it errs toward opting out: a standalone opt-out word, an opt-out phrase
//! anywhere in the text, or the provider saying it handled the text as STOP.

/// One word on its own (after trimming whitespace and punctuation, any case) that opts out.
pub const STANDALONE_STOP: &[&str] = &["stop", "stopall", "unsubscribe", "cancel", "end", "quit", "revoke", "optout"];

/// Whole-word phrases that opt out wherever they appear. CANCEL, END and QUIT count only on their
/// own, because they're common in ordinary sentences.
const STOP_PHRASES: &[&[&str]] = &[&["stop"], &["unsubscribe"], &["opt", "out"], &["optout"], &["remove", "me"], &["do", "not", "text"], &["dont", "text"], &["take", "me", "off"]];

/// Words that ask for help. DumbMail (or the provider) answers them with the program's help text.
pub const HELP_WORDS: &[&str] = &["help", "info"];

/// The words that mark a known number subscribed when it hasn't opted out at the carrier level, as
/// before the inbox. YES counts only as the answer to a double opt-in prompt (see `keywords`), and
/// SUBSCRIBE no longer subscribes anyone without one.
pub const START_WORDS: &[&str] = &["start", "unstop"];

/// Words reserved for consent that no keyword may use (with the opt-out and help words).
pub const RESERVED_CONSENT_WORDS: &[&str] = &["start", "unstop", "yes", "y", "subscribe"];

/// Telnyx toll-free and US numbers lift a carrier opt-out on START and UNSTOP; Twilio also on YES,
/// except on toll-free numbers, where Twilio documents that YES doesn't opt a person back in.
pub fn lifts_opt_out(provider: &str, our_number: &str, word: &str) -> bool {
    match word {
        "start" | "unstop" => true,
        "yes" => provider == "twilio" && !is_toll_free(our_number),
        _ => false,
    }
}

pub fn is_toll_free(number: &str) -> bool {
    ["+1800", "+1833", "+1844", "+1855", "+1866", "+1877", "+1888"].iter().any(|p| number.starts_with(p))
}

/// The text in lowercase letters and digits only: how keywords and single words are matched.
pub fn word(text: &str) -> String {
    text.trim().to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// The whole text, lowercased, with whitespace and punctuation trimmed from its ends only: how
/// words that grant permission (START, UNSTOP, YES, Y, sign-up keywords) are matched. "Start!" is
/// "start"; "s-t-a-r-t" and "start now" aren't.
pub fn strict_word(text: &str) -> String {
    text.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Lowercase words, apostrophes dropped ("don't" is "dont"), split on everything else.
fn tokens(text: &str) -> Vec<String> {
    let cleaned: String = text.to_lowercase().chars().filter(|c| !matches!(c, '\'' | '\u{2019}' | '\u{02bc}' | '`')).collect();
    cleaned.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// Does this text ask to stop texts?
pub fn is_opt_out(text: &str) -> bool {
    let w = word(text);
    if STANDALONE_STOP.contains(&w.as_str()) {
        return true;
    }
    let t = tokens(text);
    STOP_PHRASES.iter().any(|p| t.windows(p.len()).any(|win| win.iter().zip(p.iter()).all(|(a, b)| a == b)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Opt out. Always wins.
    Stop,
    /// The provider says this opted the person back in (it lifted its own block).
    ProviderStart,
    Help,
    Other,
}

/// Classify a text. `signal` is what the provider said it did with it (Twilio `OptOutType`, Telnyx
/// `autoresponse_type`: STOP, START or HELP), which is authoritative, except that an opt-out in the
/// words themselves still opts out even if the provider read it as START or HELP.
pub fn classify(text: &str, signal: Option<&str>) -> Class {
    let signal = signal.map(str::to_ascii_uppercase);
    if signal.as_deref() == Some("STOP") || is_opt_out(text) {
        return Class::Stop;
    }
    match signal.as_deref() {
        Some("START") => Class::ProviderStart,
        Some("HELP") => Class::Help,
        _ if HELP_WORDS.contains(&word(text).as_str()) => Class::Help,
        _ => Class::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_words_any_case_and_punctuation() {
        for t in ["STOP", "stop", " Stop. ", "STOP!!!", "stopall", "Stop all", "UNSUBSCRIBE", "Cancel", "end", "QUIT", "revoke", "OptOut", "opt-out", "Opt out"] {
            assert_eq!(classify(t, None), Class::Stop, "{t}");
        }
    }

    #[test]
    fn phrases_anywhere_as_whole_words() {
        for t in [
            "Please STOP texting me",
            "stop it",
            "I want to unsubscribe from this",
            "how do I opt out?",
            "please opt-out my number",
            "remove me from your list",
            "DO NOT TEXT this number",
            "don't text me again",
            "Don\u{2019}t text me",
            "dont text me",
            "take me off your list",
            "Stop. Thanks",
        ] {
            assert_eq!(classify(t, None), Class::Stop, "{t}");
        }
    }

    #[test]
    fn ordinary_sentences_are_not_opt_outs() {
        for t in ["Can I cancel my order?", "What time do you end today?", "I quit my job lol", "stopping by later", "unstop", "START", "Is the sale ending?", "remember me?", "text me the code", "take off 10%?"] {
            assert_ne!(classify(t, None), Class::Stop, "{t}");
        }
    }

    #[test]
    fn provider_signals_are_authoritative_but_opt_out_words_still_win() {
        assert_eq!(classify("Arret", Some("STOP")), Class::Stop);
        assert_eq!(classify("hello", Some("stop")), Class::Stop);
        assert_eq!(classify("unstop", Some("START")), Class::ProviderStart);
        assert_eq!(classify("help me stop these", Some("HELP")), Class::Stop);
        assert_eq!(classify("support", Some("HELP")), Class::Help);
        assert_eq!(classify("Info", None), Class::Help);
        assert_eq!(classify("JOIN", None), Class::Other);
    }

    #[test]
    fn permission_words_match_only_the_whole_text() {
        assert_eq!(strict_word("  Start! "), "start");
        assert_eq!(strict_word("YES."), "yes");
        assert_eq!(strict_word("yes please"), "yes please");
        assert_eq!(strict_word("s-t-a-r-t"), "s-t-a-r-t");
        assert!(!START_WORDS.contains(&strict_word("start now").as_str()));
        assert!(!START_WORDS.contains(&"yes") && !START_WORDS.contains(&"subscribe"));
    }

    #[test]
    fn re_opt_in_words_follow_each_provider() {
        assert!(lifts_opt_out("telnyx", "+15035550100", "start"));
        assert!(lifts_opt_out("telnyx", "+18335550100", "unstop"));
        assert!(!lifts_opt_out("telnyx", "+15035550100", "yes"));
        assert!(lifts_opt_out("twilio", "+15035550100", "yes"));
        assert!(!lifts_opt_out("twilio", "+18335550100", "yes"));
        assert!(!lifts_opt_out("twilio", "+15035550100", "join"));
        assert!(!lifts_opt_out("twilio", "+15035550100", "subscribe"));
    }
}

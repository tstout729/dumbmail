//! Account email (invites, password resets) sent through the platform's own email path from
//! SUPPORT_EMAIL, never through a customer's marketing sender, and not billed.

use uuid::Uuid;

use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Tenant-chosen text on one line, at most `max` characters.
pub fn plain(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// Whether this server can send account email.
pub fn available(state: &AppState) -> bool {
    state.cfg.support_email.as_deref().is_some_and(|s| s.contains('@'))
}

/// Send a short account email with one button. `lines` are plain text paragraphs. Returns whether
/// the provider took it; failures are logged, never shown with details.
pub async fn send(state: &AppState, to: &str, subject: &str, lines: &[String], button: (&str, &str), footer: &str) -> bool {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(subject, "no SUPPORT_EMAIL set; account email not sent");
        return false;
    };
    let (label, url) = button;
    let mut text = lines.join("\n\n");
    text.push_str(&format!("\n\n{label}: {url}\n\n{footer}\n"));
    let paras: String = lines.iter().map(|l| format!("<p style=\"margin:0 0 16px\">{}</p>", esc(l))).collect();
    let html = format!(
        "<div style=\"font-family:system-ui,-apple-system,sans-serif;font-size:15px;line-height:1.5;color:#18181b;max-width:520px\">{paras}\
         <p style=\"margin:24px 0\"><a href=\"{u}\" style=\"background:#115f42;color:#fff;text-decoration:none;padding:10px 18px;border-radius:6px;font-weight:600;display:inline-block\">{l}</a></p>\
         <p style=\"margin:0 0 16px;font-size:13px;color:#52525b\">Or open this link: <a href=\"{u}\" style=\"color:#115f42\">{u}</a></p>\
         <p style=\"margin:0;font-size:13px;color:#71717a\">{f}</p></div>",
        u = esc(url),
        l = esc(label),
        f = esc(footer),
    );
    let email = OutgoingEmail {
        message_id: Uuid::now_v7(),
        to: to.to_string(),
        from_name: "DumbMail".into(),
        from_email: from.to_string(),
        reply_to: Some(from.to_string()),
        subject: plain(subject, 150),
        html,
        text,
        unsubscribe_url: None,
    };
    match state.email.send(&email).await {
        Ok(_) => true,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "couldn't send account email");
            false
        }
    }
}

//! Email preferences. The unsubscribe link in an email opens a page where the person can stay on
//! some lists and leave others, instead of only unsubscribing from all email. Only lists the account
//! marked `show_on_preferences` appear there. Leaving a list records "Unsubscribed from List";
//! joining one adds them like any other sign-up (list flows start).
//!
//! The one-click unsubscribe that mail apps send (RFC 8058, `List-Unsubscribe=One-Click`) still
//! unsubscribes from all email: it carries no `action` field.

use std::collections::HashSet;

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::audience::{self, NewEvent};
use crate::error::AppResult;

pub struct PrefList {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub member: bool,
}

/// The account's preference-page lists, with whether this person is on each.
pub async fn lists_for(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Vec<PrefList>> {
    let rows: Vec<(Uuid, String, String, bool)> = sqlx::query_as(
        "SELECT l.id, l.name, l.description, EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = l.id AND lm.profile_id = $2)
         FROM lists l WHERE l.account_id = $1 AND l.show_on_preferences ORDER BY l.name",
    )
    .bind(account_id)
    .bind(profile_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(id, name, description, member)| PrefList { id, name, description, member }).collect())
}

/// What a submitted preference form asked for.
#[derive(Debug, PartialEq)]
pub enum Choice {
    /// Keep exactly these preference lists (any others shown are left).
    Save(HashSet<Uuid>),
    /// Unsubscribe from all email (also what a one-click unsubscribe means).
    All,
}

/// Read the POST body of the unsubscribe page. Anything that isn't an explicit `action=save` is a
/// full unsubscribe, so mail apps' one-click requests keep working.
pub fn parse_choice(body: &[u8]) -> Choice {
    let mut action = None;
    let mut lists = HashSet::new();
    for (k, v) in url::form_urlencoded::parse(body) {
        match k.as_ref() {
            "action" => action = Some(v.into_owned()),
            "list" => {
                if let Ok(id) = v.parse() {
                    lists.insert(id);
                }
            }
            _ => {}
        }
    }
    if action.as_deref() == Some("save") { Choice::Save(lists) } else { Choice::All }
}

/// Apply a saved form: join the checked preference lists, leave the unchecked ones. Returns how many
/// lists changed. Ids that aren't this account's preference lists are ignored.
pub async fn save(db: &PgPool, account_id: Uuid, profile_id: Uuid, keep: &HashSet<Uuid>, message: Option<(Uuid, Option<Uuid>, Option<Uuid>)>) -> AppResult<usize> {
    let mut changed = 0;
    for l in lists_for(db, account_id, profile_id).await? {
        let want = keep.contains(&l.id);
        if want && !l.member {
            audience::add_to_list(db, account_id, l.id, &[profile_id]).await?;
            changed += 1;
        } else if !want && l.member {
            audience::remove_from_list(db, account_id, l.id, &[profile_id]).await?;
            let mut props = json!({"list_id": l.id, "list": l.name, "method": "preferences"});
            if let Some((message_id, campaign_id, flow_id)) = message {
                props["message_id"] = json!(message_id);
                props["campaign_id"] = json!(campaign_id);
                props["flow_id"] = json!(flow_id);
            }
            audience::record_event(
                db,
                NewEvent { account_id, profile_id, metric: "Unsubscribed from List", properties: props, value: None, occurred_at: None, unique_id: None, message_id: message.map(|m| m.0) },
            )
            .await?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Where an opt-out came from: the unsubscribe link in a message (with that message), or a page
/// such as the customer hub.
pub struct OptOutSource {
    /// The event's `method` property (`link`, `hub`).
    pub method: &'static str,
    /// The webhook's reason (`unsubscribe_link`, `hub`).
    pub reason: &'static str,
    /// The message whose link was used: (message, campaign, flow), and whether it was a test.
    pub message: Option<(Uuid, Option<Uuid>, Option<Uuid>, bool)>,
}

/// Unsubscribe a person from all email (`channel` "email") or all texts ("sms"): under a lock on
/// their row, written only when it changes something, with the channel's event (`Unsubscribed`,
/// `Unsubscribed from SMS`; none for a test message's link) and the webhook notice. Returns whether
/// it changed anything. The unsubscribe page and the customer hub both use it.
pub async fn opt_out(db: &PgPool, account_id: Uuid, profile_id: Uuid, channel: &str, src: &OptOutSource) -> AppResult<bool> {
    let (col, metric, channel) = if channel == "sms" { ("sms_consent", "Unsubscribed from SMS", "sms") } else { ("email_consent", "Unsubscribed", "email") };
    let before: Option<String> = sqlx::query_scalar(&format!(
        "UPDATE profiles p SET {col} = 'unsubscribed', updated_at = now() FROM (SELECT id, {col} AS before FROM profiles WHERE id = $1 AND account_id = $2 FOR UPDATE) old
         WHERE p.id = old.id AND old.before <> 'unsubscribed' RETURNING old.before"
    ))
    .bind(profile_id)
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let Some(before) = before else { return Ok(false) };
    let message_id = src.message.map(|m| m.0);
    if !src.message.is_some_and(|m| m.3) {
        let mut props = json!({"method": src.method});
        if let Some((id, campaign_id, flow_id, _)) = src.message {
            props["campaign_id"] = json!(campaign_id);
            props["flow_id"] = json!(flow_id);
            props["message_id"] = json!(id);
        }
        audience::record_event(db, NewEvent { account_id, profile_id, metric, properties: props, value: None, occurred_at: None, unique_id: None, message_id }).await?;
    }
    crate::features::webhooks::consent_changed(db, account_id, profile_id, channel, &before, "unsubscribed", src.reason, message_id).await;
    Ok(true)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The preference form: a checkbox per list, "Save" and "Unsubscribe from all".
pub fn form_html(sender: &str, lists: &[PrefList], notice: Option<&str>) -> String {
    form_html_in(&crate::features::translations::texts::Texts::english(), sender, lists, notice)
}

/// [`form_html`] in a language (see features::translations::texts).
pub fn form_html_in(t: &crate::features::translations::texts::Texts, sender: &str, lists: &[PrefList], notice: Option<&str>) -> String {
    let mut h = String::new();
    h.push_str(&format!("<h1>{}</h1>", t.html("prefs.title", "")));
    if let Some(n) = notice {
        h.push_str(&format!(r#"<p style="background:#ecfdf5;color:#065f46;border-radius:8px;padding:10px 12px">{}</p>"#, esc(n)));
    }
    h.push_str(&format!("<p>{}</p>", t.html("prefs.intro", sender)));
    h.push_str(r#"<form method="post"><input type="hidden" name="action" value="save">"#);
    for l in lists {
        h.push_str(&format!(
            r#"<label style="display:flex;gap:10px;align-items:flex-start;margin:0 0 14px;font-size:15px;color:#111827"><input type="checkbox" name="list" value="{id}"{checked} style="margin-top:3px;width:18px;height:18px"><span><strong>{name}</strong>{desc}</span></label>"#,
            id = l.id,
            checked = if l.member { " checked" } else { "" },
            name = esc(&l.name),
            desc = if l.description.trim().is_empty() { String::new() } else { format!(r#"<br><span style="color:#6b7280;font-size:13px">{}</span>"#, esc(&l.description)) },
        ));
    }
    h.push_str(&format!(r#"<button type="submit">{}</button></form>"#, t.html("prefs.save", "")));
    h.push_str(&format!(
        r#"<form method="post" style="margin-top:12px"><input type="hidden" name="action" value="all"><button type="submit" style="background:#fff;color:#374151;border:1px solid #d1d5db">{}</button></form>"#,
        t.html("prefs.all", "")
    ));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_click_and_plain_posts_unsubscribe_from_all() {
        assert_eq!(parse_choice(b"List-Unsubscribe=One-Click"), Choice::All);
        assert_eq!(parse_choice(b""), Choice::All);
        assert_eq!(parse_choice(b"action=all"), Choice::All);
    }

    #[test]
    fn save_keeps_checked_lists() {
        let a = Uuid::now_v7();
        let body = format!("action=save&list={a}&list=not-a-uuid");
        assert_eq!(parse_choice(body.as_bytes()), Choice::Save(HashSet::from([a])));
        assert_eq!(parse_choice(b"action=save"), Choice::Save(HashSet::new()));
    }

    #[test]
    fn form_escapes_names() {
        let html = form_html("Shop <3", &[PrefList { id: Uuid::nil(), name: "<b>News</b>".into(), description: "Fridays & more".into(), member: true }], None);
        assert!(html.contains("&lt;b&gt;News") && html.contains("Fridays &amp; more") && html.contains("Shop &lt;3") && html.contains(" checked"));
    }
}

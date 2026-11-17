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
            crate::features::klaviyo_import::remove_members(db, account_id, l.id, &[profile_id]).await?;
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

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The preference form: a checkbox per list, "Save" and "Unsubscribe from all".
pub fn form_html(sender: &str, lists: &[PrefList], notice: Option<&str>) -> String {
    let mut h = String::new();
    h.push_str("<h1>Email preferences</h1>");
    if let Some(n) = notice {
        h.push_str(&format!(r#"<p style="background:#ecfdf5;color:#065f46;border-radius:8px;padding:10px 12px">{}</p>"#, esc(n)));
    }
    h.push_str(&format!("<p>Choose what you'd like to get from {}.</p>", esc(sender)));
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
    h.push_str(r#"<button type="submit">Save preferences</button></form>"#);
    h.push_str(
        r#"<form method="post" style="margin-top:12px"><input type="hidden" name="action" value="all"><button type="submit" style="background:#fff;color:#374151;border:1px solid #d1d5db">Unsubscribe from all emails</button></form>"#,
    );
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

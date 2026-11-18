//! Content imported from another platform (today: Klaviyo) that needs a person's review before it can
//! reach anyone: a campaign whose audience or send settings didn't map exactly, a flow with an ambiguous
//! step, a template whose conversion had to guess, a form whose sign-up list is missing or ambiguous.
//!
//! The review lives in two places, both read from the row a send path has already loaded, so checking
//! costs no SQL (and an account that never imported pays nothing):
//! - the item's own `import_review` column (the notes): normal edits never touch it, and while it's set
//!   an edit can't take the marker below out either;
//! - a marker in the content itself (`imported.review` in an email or text, `imported.review` at the top
//!   of a flow definition), so it travels with the content: a duplicated campaign or flow, a campaign or
//!   flow email loaded from an imported template, an A/B version made from one.
//!
//! Until a person clears it (see `features::klaviyo_import::review::clear`): a campaign can't be sent,
//! scheduled or test-sent (no A/B version or winner either), a flow can't be turned on and a live flow
//! can't be edited to use such content, and a form with no list chosen can't be published.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::models::{CampaignRow, FlowRow, FormRow};

pub fn notes_of(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()
}

/// The review marker carried in content (an email, a text, a flow definition).
pub fn marker(content: &Value) -> Vec<String> {
    notes_of(content.get("imported").and_then(|i| i.get("review")))
}

/// Put the notes into content (`imported.review`), or take them out when there are none.
pub fn mirror(content: &mut Value, notes: &[String]) {
    if notes.is_empty() {
        if let Some(i) = content.get_mut("imported").and_then(|i| i.as_object_mut()) {
            i.remove("review");
        }
        return;
    }
    if !content["imported"].is_object() {
        content["imported"] = json!({"from": "klaviyo"});
    }
    content["imported"]["review"] = json!(notes);
}

/// The notes to store in an `import_review` column (bounded), or NULL when there are none.
pub fn column(notes: &[String]) -> Option<Value> {
    if notes.is_empty() {
        return None;
    }
    Some(json!(notes.iter().take(50).map(|n| n.chars().take(500).collect::<String>()).collect::<Vec<_>>()))
}

fn sentence(what: &str, notes: &[String]) -> String {
    format!(
        "{what} was imported from Klaviyo and needs review: {}{}. Fix it, then mark it reviewed under Settings → Move from Klaviyo",
        notes[0].trim_end_matches('.'),
        if notes.len() > 1 { format!(" (and {} more)", notes.len() - 1) } else { String::new() }
    )
}

/// Every review note on a campaign: its own, and any carried by its content or an A/B version.
pub fn campaign_notes(c: &CampaignRow) -> Vec<String> {
    let mut notes = notes_of(c.import_review.as_ref());
    notes.extend(marker(&c.content));
    for v in c.ab_test.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array()).into_iter().flatten() {
        notes.extend(marker(&v["content"]));
    }
    notes.dedup();
    notes
}

/// Why a campaign can't be sent, scheduled or test-sent yet, if it needs review.
pub fn campaign_problem(c: &CampaignRow) -> Option<String> {
    let notes = campaign_notes(c);
    (!notes.is_empty()).then(|| sentence("It", &notes))
}

/// Every review note on a flow definition: the flow's own marker and its emails' and texts'.
pub fn definition_notes(definition: &Value) -> Vec<String> {
    let mut notes = marker(definition);
    for n in definition["nodes"].as_array().into_iter().flatten() {
        notes.extend(marker(&n["content"]));
    }
    notes.dedup();
    notes
}

/// Why a flow can't go live yet, if it needs review.
pub fn flow_problem(f: &FlowRow) -> Option<String> {
    let mut notes = notes_of(f.import_review.as_ref());
    notes.extend(definition_notes(&f.definition));
    notes.dedup();
    (!notes.is_empty()).then(|| sentence("This flow", &notes))
}

/// Why a live flow can't take this new definition: it brings in imported content still needing review.
pub fn live_edit_problem(definition: &Value) -> Option<String> {
    let notes = definition_notes(definition);
    (!notes.is_empty()).then(|| format!("{}. Turn the flow off first, or use content that's been reviewed", sentence("Some of this content", &notes)))
}

/// Why a form can't be published yet: imported without a sign-up list it could be sure of, and no list
/// chosen since. `list_id` is the list it will have after this change.
pub fn form_problem(import_review: Option<&Value>, list_id: Option<Uuid>) -> Option<String> {
    let notes = notes_of(import_review);
    (list_id.is_none() && !notes.is_empty()).then(|| format!("{}. Choose the list it signs people up to before publishing it", notes[0].trim_end_matches('.')))
}

/// For a form already loaded.
pub fn form_row_problem(f: &FormRow) -> Option<String> {
    form_problem(f.import_review.as_ref(), f.list_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_problems() {
        let mut c = json!({"subject": "Hi"});
        mirror(&mut c, &["Choose the audience.".into()]);
        assert_eq!(marker(&c), vec!["Choose the audience."]);
        mirror(&mut c, &[]);
        assert!(marker(&c).is_empty());
        let def = json!({"nodes": [{"id": "e", "type": "email", "content": {"imported": {"review": ["Check the split."]}}}]});
        assert_eq!(definition_notes(&def), vec!["Check the split."]);
        assert!(live_edit_problem(&def).unwrap().contains("Check the split"));
        assert!(form_problem(Some(&json!(["Two lists."])), None).is_some());
        assert!(form_problem(Some(&json!(["Two lists."])), Some(Uuid::nil())).is_none(), "a list was chosen");
        assert!(form_problem(None, None).is_none());
    }
}

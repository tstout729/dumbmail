//! MCP tools. Each reuses the same functions as the REST routes, so the rules are the same.

use serde_json::{json, Value};
use uuid::Uuid;

use super::{conversations, keywords, replies, settings};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{obj, tool};
use crate::state::AppState;

pub fn definitions() -> Vec<Value> {
    let id = |d: &str| json!({"type": "string", "description": d});
    let keyword_props = json!({
        "id": id("Keyword id to update; leave out to create one."),
        "keyword": {"type": "string", "maxLength": 20, "description": "One word of letters and digits, e.g. JOIN. Case doesn't matter. STOP, START, HELP, INFO, YES, Y and similar carrier words are reserved."},
        "aliases": {"type": "array", "items": {"type": "string", "maxLength": 20}, "maxItems": 10, "description": "Other words that do the same thing."},
        "match_type": {"enum": ["exact", "starts_with"], "description": "exact: the whole text is the word. starts_with: the text begins with it (\"PROMO codes?\"). Keywords that subscribe must be exact."},
        "subscribe": {"type": "boolean", "description": "Subscribe the sender to SMS marketing. Needs program_name and message_frequency in the inbox settings, and match_type exact. They're subscribed only once the confirmation text (with the required disclosures, added automatically) is accepted by the provider."},
        "double_opt_in": {"type": "boolean", "description": "With subscribe: first ask them to reply Y; a Y within 7 days of that prompt going out sends the confirmation, and they're subscribed once it's accepted."},
        "list_id": {"type": ["string", "null"], "description": "Also add the sender to this list. null removes it."},
        "reply": {"type": "string", "maxLength": 480, "description": "Text sent back. For sign-ups it opens the confirmation (the disclosures are added after it). Personalization tags like {{ first_name|default('there') }} work."},
        "enabled": {"type": "boolean", "description": "Turning a keyword on needs support_contact in the inbox settings (the HELP answer uses it)."},
    });
    vec![
        tool("list_conversations", "List SMS conversations", "Texting conversations, newest activity first: each person who texted in, their last message, unread count and whether you can reply now.",
            obj(json!({
                "status": {"enum": ["open", "closed", "all"], "description": "Default open."},
                "unread": {"type": "boolean", "description": "Only conversations with unread texts."},
                "q": {"type": "string", "maxLength": 100, "description": "Search names, phone numbers, emails and the words in their texts."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100},
                "cursor": {"type": "string", "description": "next_cursor from the previous page."},
                "offset": {"type": "integer", "minimum": 0, "description": "Skip this many (ignored with cursor). Prefer cursor: it doesn't skip or repeat conversations that move while you page."}
            }), &[]), true, false),
        tool("get_conversation", "Read an SMS conversation", "One conversation's two-way history, newest first (their texts, your replies, automatic replies, and campaign and flow texts), plus whether you can reply now and why not. Page back with `cursor` set to the `next_cursor` you got.",
            obj(json!({"conversation_id": id("From list_conversations."), "cursor": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 500}}), &["conversation_id"]), true, false),
        tool("send_sms_reply", "Reply to a text", "Text a reply to someone who texted in, to the number that texted and from the number they texted. Allowed when they're subscribed to texts on that number, or within 24 hours of their last verified text; never after they opted out. The rules are checked again at send time. Billed like any text. Without confirm=true it returns the text as it would go out (personalization filled in, sender name and STOP line added where required). Get the user's go-ahead before confirming.",
            obj(json!({"conversation_id": id("From list_conversations."), "body": {"type": "string", "maxLength": 1600, "description": "The reply. Personalization tags like {{ first_name|default('there') }} work."}, "confirm": {"type": "boolean"}}), &["conversation_id", "body"]), false, true),
        tool("close_conversation", "Close or reopen a conversation", "Close a conversation once it's handled (it reopens by itself when they text again), or reopen it with reopen=true.",
            obj(json!({"conversation_id": id("From list_conversations."), "reopen": {"type": "boolean"}}), &["conversation_id"]), false, false),
        tool("mark_conversation_read", "Mark a conversation read", "Clear a conversation's unread count.",
            obj(json!({"conversation_id": id("From list_conversations.")}), &["conversation_id"]), false, false),
        tool("list_sms_keywords", "List texting keywords", "Keywords people can text in (like JOIN), and what each does: subscribe, add to a list, send a reply.", obj(json!({}), &[]), true, false),
        tool("upsert_sms_keyword", "Create or change a texting keyword", "Create a keyword (no id) or change one (with id; fields left out stay the same). Texts matching it also record \"Received SMS Reply\" with property keyword, so a flow can start from it (see dumbmail://docs/sms-inbox).",
            obj(keyword_props, &[]), false, false),
        tool("delete_sms_keyword", "Delete a texting keyword", "Delete a keyword; texts with that word become ordinary messages. Requires confirm=true.",
            obj(json!({"id": id("Keyword id."), "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("list_saved_replies", "List saved replies", "Canned responses for SMS replies.", obj(json!({}), &[]), true, false),
        tool("upsert_saved_reply", "Create or change a saved reply", "Create a saved reply (no id) or change one. Personalization tags like {{ first_name|default('there') }} are filled in when it's sent.",
            obj(json!({"id": id("Saved reply id to update."), "name": {"type": "string", "maxLength": 80}, "body": {"type": "string", "maxLength": 1600}}), &[]), false, false),
        tool("delete_saved_reply", "Delete a saved reply", "Delete a saved reply. Requires confirm=true.",
            obj(json!({"id": id("Saved reply id."), "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("get_sms_inbox_settings", "Read SMS inbox settings", "Program name, message frequency and support contact (used in sign-up disclosures and the HELP answer), who answers HELP, the default and away replies, opening hours, hourly budgets for automatic texts (and whether one ran out this hour), whether inbound texts can be verified, who gets email digests of new texts, and the account's members.", obj(json!({}), &[]), true, false),
        tool("update_sms_inbox_settings", "Change SMS inbox settings", "Change any of the inbox settings; fields left out stay the same.",
            obj(json!({
                "program_name": {"type": "string", "maxLength": 60, "description": "Shown at the start of sign-up texts, e.g. \"Acme VIP Texts\"."},
                "message_frequency": {"type": "string", "maxLength": 60, "description": "e.g. \"Up to 4 msgs/month\"."},
                "support_contact": {"type": "string", "maxLength": 80, "description": "An email or phone people can reach for help. It goes in the answer to HELP, and keywords can't be turned on without it."},
                "help_replies": {"enum": ["auto", "dumbmail", "provider"], "description": "Who answers HELP and INFO. auto (default): DumbMail on Telnyx, Twilio itself on Twilio. dumbmail: always DumbMail. provider: never DumbMail."},
                "auto_budget_account": {"type": "integer", "minimum": 0, "maximum": 2000, "description": "Most automatic texts (keyword answers, sign-up texts, default/away replies, HELP) the account sends per hour. Default 200."},
                "auto_budget_number": {"type": "integer", "minimum": 0, "maximum": 1000, "description": "Most automatic texts per hour from each of your numbers. Default 100."},
                "telnyx_public_key": {"type": ["string", "null"], "description": "Only if you connected your own Telnyx account: its webhook public key (Telnyx portal → Keys & Credentials → Public Key), so inbound texts can be verified."},
                "default_reply_enabled": {"type": "boolean", "description": "Answer texts no keyword matched (at most once per person every 12 hours)."},
                "default_reply": {"type": "string", "maxLength": 480},
                "away_reply_enabled": {"type": "boolean", "description": "Outside opening hours, send the away reply instead."},
                "away_reply": {"type": "string", "maxLength": 480},
                "open_hour": {"type": "integer", "minimum": 0, "maximum": 23, "description": "In the account's time zone."},
                "close_hour": {"type": "integer", "minimum": 1, "maximum": 24},
                "open_days": {"type": "array", "items": {"type": "integer", "minimum": 1, "maximum": 7}, "description": "1 = Monday … 7 = Sunday."},
                "notify_user_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 50, "description": "Members emailed a digest of new texts (at most every 30 minutes). Ids from get_sms_inbox_settings members."}
            }), &[]), false, false),
    ]
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("{k} must be an id")))
}

fn without(a: &Value, keys: &[&str]) -> Value {
    let mut v = a.clone();
    if let Some(o) = v.as_object_mut() {
        for k in keys {
            o.remove(*k);
        }
    }
    v
}

pub async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
    let acct = ctx.account_id;
    let r = match name {
        "list_conversations" => async {
            let q: conversations::ListQuery = crate::mcp::from(a.clone())?;
            Ok(conversations::list(&state.db, acct, &q).await?.json())
        }
        .await,
        "get_conversation" => async {
            conversations::get(&state.db, acct, uuid_arg(a, "conversation_id")?, a["cursor"].as_str(), a["limit"].as_i64()).await
        }
        .await,
        "send_sms_reply" => async {
            let body = a["body"].as_str().unwrap_or_default();
            replies::send_reply(state, acct, uuid_arg(a, "conversation_id")?, body, ctx.user_id, a["confirm"].as_bool() == Some(true)).await
        }
        .await,
        "close_conversation" => async {
            let status = if a["reopen"].as_bool() == Some(true) { "open" } else { "closed" };
            conversations::set_status(&state.db, acct, uuid_arg(a, "conversation_id")?, status).await?;
            Ok(json!({"ok": true, "status": status}))
        }
        .await,
        "mark_conversation_read" => async {
            conversations::mark_read(&state.db, acct, uuid_arg(a, "conversation_id")?).await?;
            Ok(json!({"ok": true}))
        }
        .await,
        "list_sms_keywords" => async { Ok(json!({"data": keywords::list(&state.db, acct).await?})) }.await,
        "upsert_sms_keyword" => async {
            let account = crate::campaigns::load_account(&state.db, acct).await?;
            let id = match a.get("id").and_then(Value::as_str) {
                Some(_) => Some(uuid_arg(a, "id")?),
                None => None,
            };
            Ok(json!(keywords::upsert(&state.db, &account, id, &without(a, &["id"])).await?))
        }
        .await,
        "delete_sms_keyword" => async {
            let id = uuid_arg(a, "id")?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "note": "Nothing deleted. Texts with this word will become ordinary messages. Call again with confirm=true once the user approves."}));
            }
            keywords::delete(&state.db, acct, id).await?;
            Ok(json!({"ok": true}))
        }
        .await,
        "list_saved_replies" => async { Ok(json!({"data": settings::saved_replies(&state.db, acct).await?})) }.await,
        "upsert_saved_reply" => async {
            let account = crate::campaigns::load_account(&state.db, acct).await?;
            let id = match a.get("id").and_then(Value::as_str) {
                Some(_) => Some(uuid_arg(a, "id")?),
                None => None,
            };
            Ok(json!(settings::upsert_saved_reply(&state.db, &account, id, a).await?))
        }
        .await,
        "delete_saved_reply" => async {
            let id = uuid_arg(a, "id")?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "note": "Nothing deleted. Call again with confirm=true once the user approves."}));
            }
            settings::delete_saved_reply(&state.db, acct, id).await?;
            Ok(json!({"ok": true}))
        }
        .await,
        "get_sms_inbox_settings" => settings::settings_json(state, acct).await,
        "update_sms_inbox_settings" => async {
            let account = crate::campaigns::load_account(&state.db, acct).await?;
            settings::update_all(state, &account, a).await?;
            settings::settings_json(state, acct).await
        }
        .await,
        _ => return None,
    };
    Some(r)
}

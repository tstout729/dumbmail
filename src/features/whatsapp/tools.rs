//! MCP tools. Each calls the same functions as the REST routes, so the rules are the same.

use serde_json::{json, Value};
use uuid::Uuid;

use super::{content, inbox};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub fn definitions() -> Vec<Value> {
    let id = |d: &str| json!({"type": "string", "description": d});
    let button = json!({"type": "object", "additionalProperties": false, "required": ["type", "text"], "properties": {
        "type": {"enum": ["quick_reply", "url"]}, "text": {"type": "string", "maxLength": 25},
        "url": {"type": "string", "description": "url buttons: a full https link."},
        "track": {"type": "boolean", "description": "url buttons: count clicks through DumbMail's link tracking (default true)."}}});
    vec![
        tool("get_whatsapp_settings", "Get WhatsApp settings", "The WhatsApp connection (business account id, phone number id, whether the token and app secret are saved, never their values), the webhook URL and verify token to paste into the Meta app, the phone number's name, quality rating and messaging limit from the last check, the team's test numbers and template counts.",
            obj(json!({}), &[]), true, false),
        tool("update_whatsapp_settings", "Connect or change WhatsApp", "Save the WhatsApp Business Account id, the phone number id, a permanent system-user access token and the Meta app's app secret (tokens and secrets are encrypted and never shown again; leave them out to keep the saved ones). Sending turns on once all four are saved. Changing the business account or number starts fresh: nobody stays subscribed and queued messages are skipped. Also WhatsApp's own sign-up words and opt-in buttons. Then check_whatsapp_connection.",
            obj(json!({"waba_id": {"type": "string"}, "phone_number_id": {"type": "string"}, "access_token": {"type": "string"}, "app_secret": {"type": "string"},
                "signup_keywords": {"type": "array", "maxItems": 10, "items": {"type": "string"}, "description": "Words that sign someone up to WhatsApp marketing when they send exactly that word on WhatsApp, e.g. JOIN."},
                "signup_reply": {"type": "string", "maxLength": 1000, "description": "The answer to a sign-up word (\"Reply STOP to unsubscribe\" is added)."},
                "optin_buttons": {"type": "array", "maxItems": 10, "items": {"type": "string"}, "description": "Quick-reply button payloads that mean yes to WhatsApp marketing."}}), &[]), false, false),
        tool("disconnect_whatsapp", "Disconnect WhatsApp", "Forget the WhatsApp connection (ids, token, app secret). Nothing can be sent on WhatsApp until it's connected again. Requires confirm=true.",
            obj(json!({"confirm": {"type": "boolean"}}), &[]), false, true),
        tool("check_whatsapp_connection", "Check the WhatsApp connection", "Read the phone number's display name, quality rating and messaging limit from Meta with the saved token. A token or permission problem shows in error.",
            obj(json!({}), &[]), false, false),
        tool("list_whatsapp_templates", "List WhatsApp templates", "The account's message templates as last synced from Meta: name, language, category (MARKETING needs a WhatsApp opt-in; UTILITY is for order and booking updates), status (only APPROVED can be sent), components and the variables a send fills. sync_whatsapp_templates reads them from Meta again.",
            obj(json!({}), &[]), true, false),
        tool("sync_whatsapp_templates", "Sync WhatsApp templates", "Read every template from Meta now (status changes also arrive by webhook). Needs settings write.", obj(json!({}), &[]), false, false),
        tool("list_whatsapp_test_numbers", "List WhatsApp test numbers", "Team members' own phone numbers that WhatsApp tests may go to, and whether each is verified.", obj(json!({}), &[]), true, false),
        tool("add_whatsapp_test_number", "Add your WhatsApp test number", "Start verifying your own phone as a WhatsApp test number (needs a signed-in member, not an API key): send the code it returns from that phone, on WhatsApp, to the business number within 30 minutes.",
            obj(json!({"phone_number": {"type": "string"}}), &["phone_number"]), false, false),
        tool("create_whatsapp_template", "Create a WhatsApp template", "Submit a new template to Meta for review (status starts PENDING; approval usually takes minutes to a day). Variables are {{1}}, {{2}} ... with one example each, never at the very start or end of the message. Link buttons are tracked by default. Without confirm=true it returns what would be submitted.",
            obj(json!({
                "name": {"type": "string", "description": "lowercase letters, digits and underscores, e.g. order_update"},
                "language": {"type": "string", "description": "e.g. en, en_US, pt_BR"},
                "category": {"enum": ["MARKETING", "UTILITY"]},
                "header": {"type": "string", "maxLength": 60, "description": "Optional text header, at most one variable."},
                "header_example": {"type": "string"},
                "body": {"type": "string", "maxLength": 1024},
                "body_examples": {"type": "array", "items": {"type": "string"}, "description": "One example per body variable, in order."},
                "footer": {"type": "string", "maxLength": 60},
                "buttons": {"type": "array", "maxItems": 10, "items": button},
                "confirm": {"type": "boolean"}
            }), &["name", "language", "category", "body"]), false, true),
        tool("send_test_whatsapp", "Send a WhatsApp test", "Send WhatsApp content (a template and its variables) to a team member's verified test number (Settings → WhatsApp). Personalized as profile_id when given, otherwise as the profile with that number.",
            obj(json!({"to": {"type": "string"}, "content": content::schema(), "profile_id": id("Optional sample person.")}), &["to", "content"]), false, false),
        tool("get_whatsapp_consent", "Get a person's WhatsApp consent", "WhatsApp consent for a profile's current number (separate from texts): subscribed, unsubscribed or never_subscribed, where it came from, its history, whether the number is on WhatsApp, and whether the 24-hour reply window is open.",
            obj(json!({"profile_id": id("Profile id.")}), &["profile_id"]), true, false),
        tool("set_whatsapp_consent", "Set a person's WhatsApp consent", "Subscribe a person's current number to WhatsApp marketing (only when they explicitly agreed to WhatsApp messages; say how in evidence) or unsubscribe it. Texts are untouched.",
            obj(json!({"profile_id": id("Profile id."), "consent": {"enum": ["subscribed", "unsubscribed"]}, "evidence": {"type": "string", "maxLength": 300, "description": "Required to subscribe: how they agreed, e.g. \"checkout checkbox naming WhatsApp\"."}}), &["profile_id", "consent"]), false, false),
        tool("import_whatsapp_consent", "Import WhatsApp consent", "Record WhatsApp consent for up to 1,000 numbers at once (numbers without a profile get one). Subscribing needs evidence that they agreed to WhatsApp marketing.",
            obj(json!({"phone_numbers": {"type": "array", "maxItems": 1000, "items": {"type": "string"}}, "consent": {"enum": ["subscribed", "unsubscribed"]}, "evidence": {"type": "string", "maxLength": 300}}), &["phone_numbers", "consent"]), false, false),
        tool("list_whatsapp_conversations", "List WhatsApp conversations", "WhatsApp conversations, newest activity first: who wrote, their last message, unread count and whether the 24-hour window for a free-form reply is open.",
            obj(json!({"status": {"enum": ["open", "closed"]}, "before": {"type": "string", "format": "date-time", "description": "last_message_at of the last one you got, for the next page."}, "limit": {"type": "integer", "minimum": 1, "maximum": 100}}), &[]), true, false),
        tool("get_whatsapp_conversation", "Read a WhatsApp conversation", "One conversation's messages both ways (theirs, replies, campaign and flow messages with their WhatsApp status), their WhatsApp consent, and how you can reply now (free-form inside 24 hours, else an approved template).",
            obj(json!({"conversation_id": id("From list_whatsapp_conversations.")}), &["conversation_id"]), true, false),
        tool("send_whatsapp_reply", "Reply on WhatsApp", "Reply in a WhatsApp conversation: text (only within 24 hours of their last message) or an approved template (content: template_id and variables; a marketing template only if they opted in). Checked again right before it goes. Without confirm=true it returns the message as it would go out. Get the user's go-ahead before confirming.",
            obj(json!({"conversation_id": id("From list_whatsapp_conversations."), "text": {"type": "string", "maxLength": 4096}, "template": content::schema(), "confirm": {"type": "boolean"}}), &["conversation_id"]), false, true),
        tool("close_whatsapp_conversation", "Close or reopen a WhatsApp conversation", "Close a conversation once it's handled (it reopens when they write again), or reopen it with reopen=true.",
            obj(json!({"conversation_id": id("From list_whatsapp_conversations."), "reopen": {"type": "boolean"}}), &["conversation_id"]), false, false),
        tool("get_whatsapp_report", "WhatsApp results", "Sent, delivered, read, clicked, failed and skipped WhatsApp messages, with the top reasons, for a campaign, a flow, or the account over the last `days` (default 30).",
            obj(json!({"campaign_id": {"type": "string"}, "flow_id": {"type": "string"}, "days": {"type": "integer", "minimum": 1, "maximum": 365}}), &[]), true, false),
    ]
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("{k} must be an id")))
}

pub async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
    let acct = ctx.account_id;
    Some(match name {
        "get_whatsapp_settings" => super::settings::settings_json(state, acct).await,
        "update_whatsapp_settings" => match from::<super::settings::SettingsBody>(a.clone()) {
            Ok(b) => super::update_settings(state, ctx, b).await,
            Err(e) => Err(e),
        },
        "disconnect_whatsapp" => {
            if a["confirm"].as_bool() != Some(true) {
                Ok(json!({"dry_run": true, "note": "Nothing changed. Disconnecting stops every WhatsApp send until it's connected again. Call again with confirm=true once the user approves."}))
            } else {
                super::disconnect(state, ctx).await
            }
        }
        "check_whatsapp_connection" => super::settings::check(state, acct).await,
        "list_whatsapp_templates" => super::templates_json(state, acct).await,
        "sync_whatsapp_templates" => super::sync_templates(state, ctx).await,
        "list_whatsapp_test_numbers" => super::testers::list(&state.db, acct).await.map(|l| json!({"test_numbers": l})).map_err(Into::into),
        "add_whatsapp_test_number" => match a["phone_number"].as_str() {
            Some(p) => super::add_test_number(state, ctx, p).await,
            None => Err(AppError::bad("phone_number is required")),
        },
        "create_whatsapp_template" => match from::<super::CreateBody>(a.clone()) {
            Ok(b) => super::create_template(state, ctx, b).await,
            Err(e) => Err(e),
        },
        "send_test_whatsapp" => match from::<super::TestBody>(a.clone()) {
            Ok(b) => super::send_test(state, ctx, b).await,
            Err(e) => Err(e),
        },
        "get_whatsapp_consent" => match uuid_arg(a, "profile_id") {
            Ok(id) => super::profile_json(state, acct, id).await,
            Err(e) => Err(e),
        },
        "set_whatsapp_consent" => match (uuid_arg(a, "profile_id"), a["consent"].as_str()) {
            (Ok(id), Some(c)) => super::set_profile_consent(state, ctx, id, &super::ConsentBody { consent: c.to_string(), evidence: a["evidence"].as_str().map(str::to_string) }).await,
            (Err(e), _) => Err(e),
            (_, None) => Err(AppError::bad("consent is subscribed or unsubscribed")),
        },
        "import_whatsapp_consent" => match from::<super::ImportBody>(a.clone()) {
            Ok(b) => super::import_consent(state, ctx, &b).await,
            Err(e) => Err(e),
        },
        "list_whatsapp_conversations" => {
            let q = inbox::ListQuery {
                status: a["status"].as_str().map(str::to_string),
                before: a["before"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&chrono::Utc)),
                limit: a["limit"].as_i64(),
            };
            inbox::list(&state.db, acct, &q).await
        }
        "get_whatsapp_conversation" => match uuid_arg(a, "conversation_id") {
            Ok(id) => inbox::get(state, acct, id).await,
            Err(e) => Err(e),
        },
        "send_whatsapp_reply" => match uuid_arg(a, "conversation_id") {
            Ok(id) => {
                let b = inbox::ReplyBody { text: a["text"].as_str().map(str::to_string), template: a.get("template").filter(|v| !v.is_null()).cloned(), confirm: a["confirm"].as_bool() == Some(true) };
                super::reply(state, ctx, id, &b).await
            }
            Err(e) => Err(e),
        },
        "close_whatsapp_conversation" => match uuid_arg(a, "conversation_id") {
            Ok(id) => inbox::set_status(&state.db, acct, id, if a["reopen"].as_bool() == Some(true) { "open" } else { "closed" }).await.map(|_| json!({"ok": true})),
            Err(e) => Err(e),
        },
        "get_whatsapp_report" => {
            let opt = |k: &str| a[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
            super::report(&state.db, acct, &super::ReportQuery { campaign_id: opt("campaign_id"), flow_id: opt("flow_id"), days: a["days"].as_i64() }).await
        }
        _ => return None,
    })
}

//! Two-way texting: an SMS inbox for conversations, replies from the dashboard, API and MCP,
//! keywords ("text JOIN to subscribe"), automatic replies, email digests of new texts, and the
//! "Received SMS Reply" event flows use (including the "wait for a reply" step).
//!
//! The provider webhooks in `public.rs` check the provider's signature ([`verify`]) and hand
//! every inbound text to [`inbound`]: it finds the one account the receiving number belongs to,
//! stores the text and applies its effect on consent in one transaction ([`store::receive`]:
//! opt-outs first and always), then runs what follows ([`process`]): events, HELP, double
//! opt-in, keywords, automatic replies and waiting flows. Every text that goes out passes
//! the claim in `sending::claim_text` just before the provider call (under the lock an inbound
//! opt-out takes, with [`store::reply_allowed`] for replies), so an opt-out that committed first wins.

mod api;
pub mod conversations;
pub mod keywords;
mod notify;
pub mod optout;
pub mod replies;
pub mod settings;
pub mod store;
mod tools;
pub mod verify;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

pub use store::{record_sent, replied_between, Inbound, Received};
pub use verify::Proof;

use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::features::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use crate::audience::{self, NewEvent};
use crate::error::AppResult;
use crate::models::normalize_phone;
use crate::state::AppState;

pub struct Module;

/// An inbound text from a provider webhook, whose signature was already checked with the key of
/// the route's own provider and connection ([`verify`]). `scope` is the account whose webhook URL
/// was used (None for the platform's).
///
/// The receiving number must be assigned to exactly one account, and to it through this
/// connection: the platform's (an account on the platform's numbers) or, for an account's own
/// URL, that account (its own provider when it connected one). Otherwise nothing is stored and it's
/// logged. One exception: an opt-out is still recorded when the number belongs to exactly one
/// account and the URL used is the platform's or that account's, even without a message id, a
/// signing key or a matching connection; it is never written into several accounts. A text
/// without the provider's message id is otherwise refused.
pub async fn inbound(state: &AppState, scope: Option<Uuid>, msg: &Inbound, proof: Proof) -> AppResult<()> {
    let (Some(from), Some(to)) = (normalize_phone(&msg.from), normalize_phone(&msg.to)) else {
        tracing::warn!(provider = msg.provider, "inbound text with an unreadable number; not stored");
        return Ok(());
    };
    let stop = optout::classify(&msg.text, msg.provider_signal.as_deref()) == optout::Class::Stop;
    // Every account the number is assigned to (whatever the URL), and how it texts.
    let claims: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT a.id, a.sms_mode, a.sms_byo_provider FROM accounts a LEFT JOIN phone_numbers n ON n.account_id = a.id AND n.status <> 'released'
         WHERE a.sms_from = $1 OR n.phone_number = $1",
    )
    .bind(&to)
    .fetch_all(&state.db)
    .await?;
    let message = msg.provider_message_id.as_deref().unwrap_or("none");
    let owner = match claims.as_slice() {
        [one] => one.clone(),
        [] => {
            tracing::warn!(provider = msg.provider, message, to = %to, "inbound text for a number no account uses; not stored");
            return Ok(());
        }
        many => {
            tracing::error!(provider = msg.provider, message, to = %to, accounts = many.len(), "inbound text for a number several accounts claim; nothing stored in any");
            if stop {
                let ids: Vec<Uuid> = many.iter().map(|(id, _, _)| *id).collect();
                claim_conflict_alert(state, &to, &ids).await;
            }
            return Ok(());
        }
    };
    let (account_id, mode, byo_provider) = owner;
    // The URL used is the platform's or this account's own: only then may it write anything here.
    let url_fits = scope.is_none_or(|s| s == account_id);
    // And the connection that verified it is the one the number belongs to.
    let connection_fits = url_fits
        && match (scope, mode.as_str()) {
            (_, "byo") => scope == Some(account_id) && byo_provider.as_deref() == Some(msg.provider),
            _ => true,
        };
    let Some(provider_message_id) = msg.provider_message_id.clone().filter(|_| connection_fits) else {
        if !url_fits {
            tracing::warn!(provider = msg.provider, message, to = %to, "inbound text on another account's webhook address; not stored");
            return Ok(());
        }
        if stop {
            // Opting out is always the safe direction.
            tracing::warn!(provider = msg.provider, message, to = %to, "opt-out without a message id or through another connection; recorded, text not stored");
            return store::opt_out_only(&state.db, account_id, &from, &to).await;
        }
        if !connection_fits {
            tracing::warn!(provider = msg.provider, message, to = %to, "inbound text through a connection the number doesn't use; not stored");
            return Ok(());
        }
        tracing::warn!(provider = msg.provider, to = %to, "inbound text without a message id; not stored");
        return Err(crate::error::AppError::bad("an inbound text needs the provider's message id"));
    };
    let rx = store::receive(state, account_id, &from, &to, msg, &provider_message_id, proof.verified()).await?;
    if rx.processed {
        return Ok(());
    }
    process(state, &rx).await
}

/// An opt-out was dropped because several accounts claim the number it was sent to: tell the
/// operator, at most once per number per day. The claim and the job that emails are written
/// together; anything that goes wrong is only logged, so the webhook still answers 200.
async fn claim_conflict_alert(state: &AppState, our_number: &str, accounts: &[Uuid]) {
    let run = async {
        let mut tx = state.db.begin().await?;
        let fresh = sqlx::query(
            "INSERT INTO sms_claim_conflict_alerts (our_number, day) VALUES ($1, (now() AT TIME ZONE 'America/Los_Angeles')::date) ON CONFLICT DO NOTHING",
        )
        .bind(our_number)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if fresh {
            let ids: Vec<Uuid> = accounts.iter().take(20).copied().collect();
            crate::jobs::enqueue(&mut *tx, "inbox_claim_conflict_alert", json!({"our_number": our_number, "accounts": ids, "count": accounts.len()}), None).await?;
        }
        tx.commit().await?;
        Ok::<(), sqlx::Error>(())
    };
    if let Err(e) = run.await {
        tracing::error!(to = %our_number, error = %e, "couldn't queue the operator alert for a number several accounts claim");
    }
}

/// The job behind [`claim_conflict_alert`]: the number and the account ids, nothing else.
async fn send_claim_conflict_alert(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let number = payload["our_number"].as_str().unwrap_or_default();
    let ids: Vec<Uuid> = payload["accounts"].as_array().into_iter().flatten().filter_map(|v| v.as_str()?.parse().ok()).collect();
    let count = payload["count"].as_u64().unwrap_or(ids.len() as u64);
    let list = ids.iter().map(|id| format!("  {id}")).collect::<Vec<_>>().join("\n");
    let more = if count > ids.len() as u64 { format!("\n  … and {} more", count - ids.len() as u64) } else { String::new() };
    let text = format!(
        "Someone texted STOP (or another opt-out) to {number}, but {count} accounts claim that number, so the opt-out wasn't recorded in any of them.\n\
         They may still get texts from this number until it belongs to one account. Release it from the accounts that shouldn't have it (their phone numbers, or the number set as their SMS sender).\n\n\
         Number: {number}\nAccounts:\n{list}{more}\n\n\
         This alert is sent at most once a day for each number."
    );
    let first = ids.first().copied().unwrap_or_default();
    crate::deliverability::email_operator(state, first, &format!("Opt-out not recorded: {number} is claimed by several accounts"), text).await;
    Ok(())
}

/// Everything after a text is stored. Each step is idempotent, so a provider retry of a text that
/// didn't finish runs them again safely. Unverified texts get only their consent events: no
/// keyword, reply, flow or subscription.
pub async fn process(state: &AppState, rx: &Received) -> AppResult<()> {
    match rx.kind.as_str() {
        "stop" => {
            let props = json!({"keyword": rx.word, "method": "text", "text": rx.text, "provider_signal": rx.provider_signal, "verified": rx.verified, "inbound_message_id": rx.id});
            consent_event(state, rx, "Unsubscribed from SMS", props).await?;
        }
        "start" => {
            let props = json!({"keyword": rx.word, "method": "start", "text": rx.text, "to": rx.to, "inbound_message_id": rx.id});
            consent_event(state, rx, "Subscribed to SMS", props).await?;
        }
        _ if !rx.verified => {}
        // HELP never starts a keyword, an automatic reply or a flow.
        "help" => replies::help_reply(state, rx).await?,
        "confirm" => {
            if keywords::confirm_pending(state, rx).await? {
                store::record_reply_event(&state.db, rx, Some("Y")).await?;
            } else {
                store::set_kind(&state.db, rx.id, "message", None).await?;
                store::record_reply_event(&state.db, rx, None).await?;
            }
            store::wake_waiting_flows(&state.db, rx.profile_id, rx.received_at).await?;
        }
        "keyword" => {
            let k = match rx.keyword.as_deref() {
                Some(w) => keywords::by_word(&state.db, rx.account_id, w).await?,
                None => None,
            };
            store::record_reply_event(&state.db, rx, rx.keyword.as_deref().map(str::to_uppercase).as_deref()).await?;
            if let Some(k) = k {
                keywords::run(state, rx, &k).await?;
            }
            store::wake_waiting_flows(&state.db, rx.profile_id, rx.received_at).await?;
        }
        "message" => {
            store::record_reply_event(&state.db, rx, None).await?;
            replies::default_reply(state, rx).await?;
            store::wake_waiting_flows(&state.db, rx.profile_id, rx.received_at).await?;
        }
        // "blocked": texted while opted out. Nothing happens and nothing is sent.
        _ => {}
    }
    store::mark_processed(&state.db, rx.id).await
}

async fn consent_event(state: &AppState, rx: &Received, metric: &str, properties: Value) -> AppResult<()> {
    audience::record_event(
        &state.db,
        NewEvent {
            account_id: rx.account_id,
            profile_id: rx.profile_id,
            metric,
            properties,
            value: None,
            occurred_at: None,
            unique_id: Some(format!("sms-in:{}", rx.id)),
            message_id: None,
        },
    )
    .await?;
    Ok(())
}

// Reading conversations and inbound texts is reading people's data. Replying (and closing a
// conversation, which is part of answering it) needs the send permission; saved replies are the
// replier's, so changing them does too. Keywords and inbox settings (automatic replies, HELP,
// budgets) are Managers and up; the Telnyx signing key in the settings also needs the SMS
// connection's permission (checked in `settings::update_all`).
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/inbox/conversations", Perm::ProfilesRead),
    rule("GET", "/api/inbox/unread", Perm::ProfilesRead),
    rule("GET", "/api/inbox/conversations/{id}", Perm::ProfilesRead),
    rule("POST", "/api/inbox/conversations/{id}/read", Perm::ProfilesRead),
    rule("POST", "/api/inbox/conversations/{id}/status", Perm::CampaignSend),
    rule("POST", "/api/inbox/conversations/{id}/reply", Perm::CampaignSend),
    rule("GET", "/api/inbox/profiles/{profile_id}", Perm::ProfilesRead),
    rule("GET", "/api/inbox/media/{id}", Perm::ProfilesRead),
    rule("GET", "/api/inbox/keywords", Perm::AccountRead),
    rule("POST", "/api/inbox/keywords", Perm::SettingsWrite),
    rule("PATCH", "/api/inbox/keywords/{id}", Perm::SettingsWrite),
    rule("DELETE", "/api/inbox/keywords/{id}", Perm::SettingsWrite),
    rule("GET", "/api/inbox/saved-replies", Perm::MessagingRead),
    rule("POST", "/api/inbox/saved-replies", Perm::CampaignSend),
    rule("PATCH", "/api/inbox/saved-replies/{id}", Perm::CampaignSend),
    rule("DELETE", "/api/inbox/saved-replies/{id}", Perm::CampaignManage),
    rule("GET", "/api/inbox/settings", Perm::AccountRead),
    rule("PATCH", "/api/inbox/settings", Perm::SettingsWrite),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "list_conversations", perm: Perm::ProfilesRead },
    ToolRule { name: "get_conversation", perm: Perm::ProfilesRead },
    ToolRule { name: "mark_conversation_read", perm: Perm::ProfilesRead },
    ToolRule { name: "send_sms_reply", perm: Perm::CampaignSend },
    ToolRule { name: "close_conversation", perm: Perm::CampaignSend },
    ToolRule { name: "list_sms_keywords", perm: Perm::AccountRead },
    ToolRule { name: "upsert_sms_keyword", perm: Perm::SettingsWrite },
    ToolRule { name: "delete_sms_keyword", perm: Perm::SettingsWrite },
    ToolRule { name: "list_saved_replies", perm: Perm::MessagingRead },
    ToolRule { name: "upsert_saved_reply", perm: Perm::CampaignSend },
    ToolRule { name: "delete_saved_reply", perm: Perm::CampaignManage },
    ToolRule { name: "get_sms_inbox_settings", perm: Perm::AccountRead },
    ToolRule { name: "update_sms_inbox_settings", perm: Perm::SettingsWrite },
];

/// Audit entries, shared by the REST routes and the MCP tools. Fixed fields only: never the text
/// of a message or a reply.
pub(crate) mod audit {
    use serde_json::{json, Value};
    use uuid::Uuid;

    use super::keywords::Keyword;
    use crate::auth::Ctx;
    use crate::features::team;
    use crate::state::AppState;

    /// Which settings changed (their names, already checked against the known list).
    pub async fn settings(state: &AppState, ctx: &Ctx, patch: &Value) {
        let mut fields: Vec<&str> = patch.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        fields.sort_unstable();
        team::audit(state, ctx, "sms_inbox.settings_change", None, json!({"fields": fields})).await;
    }

    pub async fn keyword(state: &AppState, ctx: &Ctx, action: &str, k: &Keyword) {
        team::audit(state, ctx, action, Some(&k.id.to_string()), json!({"word": k.keyword, "enabled": k.enabled, "subscribe": k.subscribe})).await;
    }

    pub async fn keyword_deleted(state: &AppState, ctx: &Ctx, id: Uuid) {
        team::audit(state, ctx, "sms_keyword.delete", Some(&id.to_string()), json!({})).await;
    }

    /// A reply that went out (not dry runs).
    pub async fn reply(state: &AppState, ctx: &Ctx, conversation_id: Uuid, out: &Value) {
        if out["sent"] == true {
            team::audit(state, ctx, "sms_reply.send", Some(&conversation_id.to_string()), json!({"message_id": out["message_id"]})).await;
        }
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "inbox"
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools::definitions()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        tools::call(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "sms-inbox",
            name: "SMS inbox, replies and keywords",
            description: "Two-way texting: conversations, reply rules, opt-outs, keywords and sign-up disclosures, HELP, automatic replies and their budgets, webhook verification, and the reply flow trigger and step",
            text: include_str!("../../../docs/SMS_INBOX.md"),
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            "inbox_media" => Some(notify::copy_media(state, payload).await),
            "inbox_auto_send" => Some(replies::run_auto_send(state, payload).await),
            "inbox_digest" => Some(notify::send_digest(state, payload).await),
            "inbox_claim_conflict_alert" => Some(send_claim_conflict_alert(state, payload).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            notify::queue_digests(state).await?;
        }
        if tick.is_multiple_of(60) {
            sqlx::query("DELETE FROM sms_auto_budget WHERE hour < now() - interval '2 days'").execute(&state.db).await?;
            sqlx::query("DELETE FROM sms_claim_conflict_alerts WHERE created_at < now() - interval '7 days'").execute(&state.db).await?;
        }
        Ok(())
    }
}

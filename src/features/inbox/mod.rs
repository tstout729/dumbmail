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
            store::wake_waiting_flows(&state.db, rx.profile_id).await?;
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
            store::wake_waiting_flows(&state.db, rx.profile_id).await?;
        }
        "message" => {
            store::record_reply_event(&state.db, rx, None).await?;
            replies::default_reply(state, rx).await?;
            store::wake_waiting_flows(&state.db, rx.profile_id).await?;
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

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "inbox"
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
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            notify::queue_digests(state).await?;
        }
        if tick.is_multiple_of(60) {
            sqlx::query("DELETE FROM sms_auto_budget WHERE hour < now() - interval '2 days'").execute(&state.db).await?;
        }
        Ok(())
    }
}

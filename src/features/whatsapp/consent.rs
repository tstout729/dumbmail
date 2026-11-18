//! WhatsApp consent, separate from texts: kept per number (`whatsapp_contacts`), changed only by
//! [`set`], with every change in `whatsapp_consent_log`.
//!
//! - **Subscribed** only by an explicit WhatsApp opt-in: a sign-up keyword sent on WhatsApp, or an
//!   API or import call that says the person agreed to WhatsApp marketing.
//! - **Unsubscribed** by STOP on WhatsApp, Meta's own "stop marketing messages" signal (the
//!   `user_preferences` webhook or error 131050), or an API call. It wins over everything: queued
//!   WhatsApp messages to the number are skipped in the same transaction, and the final claim before
//!   every hand-off reads it under the same lock.
//! - Consent belongs to the number: a profile whose number changes has no WhatsApp consent for the
//!   new one until that number opts in.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// SQL true when profile `p` would get a WhatsApp marketing message now: a number, subscribed to
/// WhatsApp on that number, not known to be off WhatsApp, and WhatsApp connected.
pub const REACHABLE_SQL: &str = "(p.phone_number IS NOT NULL
    AND EXISTS (SELECT 1 FROM whatsapp_contacts wc WHERE wc.account_id = p.account_id AND wc.phone_number = p.phone_number AND wc.consent = 'subscribed' AND wc.unreachable_at IS NULL)
    AND EXISTS (SELECT 1 FROM whatsapp_settings ws WHERE ws.account_id = p.account_id AND ws.access_token_enc IS NOT NULL AND ws.phone_number_id IS NOT NULL))";

/// SQL for profile `p`'s WhatsApp consent status on its current number.
pub const STATUS_SQL: &str = "COALESCE((SELECT wc.consent FROM whatsapp_contacts wc WHERE wc.account_id = p.account_id AND wc.phone_number = p.phone_number), 'never_subscribed')";

/// Serializes everything that changes or checks one number's WhatsApp state in an account:
/// inbound messages and opt-outs, and the final claim before a message is handed to Meta.
pub async fn lock(conn: &mut PgConnection, account_id: Uuid, phone: &str) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('whatsapp:' || $1::text || ':' || $2, 64))").bind(account_id).bind(phone).execute(conn).await?;
    Ok(())
}

/// Make sure the number has a contact row (and point it at this profile when given).
pub async fn ensure_contact(conn: &mut PgConnection, account_id: Uuid, phone: &str, profile_id: Option<Uuid>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO whatsapp_contacts (account_id, phone_number, profile_id) VALUES ($1, $2, $3)
         ON CONFLICT (account_id, phone_number) DO UPDATE SET profile_id = COALESCE(EXCLUDED.profile_id, whatsapp_contacts.profile_id)",
    )
    .bind(account_id)
    .bind(phone)
    .bind(profile_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Where a consent change came from.
pub const SOURCES: &[&str] = &["keyword", "stop", "start", "meta_opt_out", "meta_error", "api", "import", "dashboard"];

/// The one function that changes WhatsApp consent. Call it inside a transaction holding [`lock`]
/// for the number. Records the change (with `source` and `detail`) and, for an unsubscribe, skips
/// the number's queued WhatsApp messages (reported to webhooks and exports). Returns whether the
/// status changed. `status` is `subscribed` or `unsubscribed`.
#[allow(clippy::too_many_arguments)]
pub async fn set(
    conn: &mut PgConnection,
    db: &sqlx::PgPool,
    account_id: Uuid,
    phone: &str,
    profile_id: Option<Uuid>,
    status: &str,
    source: &str,
    detail: Value,
) -> AppResult<bool> {
    if !matches!(status, "subscribed" | "unsubscribed") {
        return Err(AppError::bad("WhatsApp consent must be subscribed or unsubscribed"));
    }
    if !SOURCES.contains(&source) {
        return Err(AppError::bad("unknown consent source"));
    }
    ensure_contact(&mut *conn, account_id, phone, profile_id).await?;
    let old: Option<String> = sqlx::query_scalar(
        "UPDATE whatsapp_contacts c SET consent = $3, consent_source = $4, consent_at = now(), updated_at = now()
         FROM (SELECT consent FROM whatsapp_contacts WHERE account_id = $1 AND phone_number = $2 FOR UPDATE) old
         WHERE c.account_id = $1 AND c.phone_number = $2 RETURNING old.consent",
    )
    .bind(account_id)
    .bind(phone)
    .bind(status)
    .bind(source)
    .fetch_optional(&mut *conn)
    .await?;
    let changed = old.as_deref() != Some(status);
    sqlx::query("INSERT INTO whatsapp_consent_log (id, account_id, phone_number, profile_id, consent, source, detail) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(phone)
        .bind(profile_id)
        .bind(status)
        .bind(source)
        .bind(&detail)
        .execute(&mut *conn)
        .await?;
    if status == "unsubscribed" {
        skip_queued(conn, db, account_id, phone, "opted out of WhatsApp").await?;
    }
    Ok(changed)
}

/// Skip the number's WhatsApp messages that haven't been handed to Meta yet (campaign and flow
/// messages; replies to the opt-out itself go after it).
pub async fn skip_queued(conn: &mut PgConnection, db: &sqlx::PgPool, account_id: Uuid, phone: &str, why: &str) -> AppResult<()> {
    let skipped: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE messages m SET status = 'skipped', error = $3 FROM whatsapp_sent w
         WHERE w.message_id = m.id AND m.account_id = $1 AND m.channel = 'whatsapp' AND m.to_address = $2 AND m.status IN ('queued', 'retrying')
           AND w.kind = 'marketing' AND w.handed_at IS NULL
         RETURNING m.id",
    )
    .bind(account_id)
    .bind(phone)
    .bind(why)
    .fetch_all(&mut *conn)
    .await?;
    // Campaign messages queued before their whatsapp_sent row exists.
    let more: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE messages m SET status = 'skipped', error = $3
         WHERE m.account_id = $1 AND m.channel = 'whatsapp' AND m.to_address = $2 AND m.status = 'queued' AND m.campaign_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM whatsapp_sent w WHERE w.message_id = m.id)
         RETURNING m.id",
    )
    .bind(account_id)
    .bind(phone)
    .bind(why)
    .fetch_all(&mut *conn)
    .await?;
    let all: Vec<Uuid> = skipped.into_iter().chain(more).collect();
    crate::features::messages_unsent_in(conn, db, account_id, &all).await;
    Ok(())
}

/// Meta said this number can't get WhatsApp messages: nothing more goes to it until it writes in.
pub async fn mark_unreachable(db: &sqlx::PgPool, account_id: Uuid, phone: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO whatsapp_contacts (account_id, phone_number, unreachable_at) VALUES ($1, $2, now())
         ON CONFLICT (account_id, phone_number) DO UPDATE SET unreachable_at = now(), updated_at = now()",
    )
    .bind(account_id)
    .bind(phone)
    .execute(db)
    .await?;
    Ok(())
}

/// One number's WhatsApp state, for the dashboard and tools.
pub async fn contact_json(db: &sqlx::PgPool, account_id: Uuid, phone: &str) -> sqlx::Result<Value> {
    let row: Option<(String, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT consent, consent_source, consent_at, unreachable_at, last_inbound_at FROM whatsapp_contacts WHERE account_id = $1 AND phone_number = $2",
    )
    .bind(account_id)
    .bind(phone)
    .fetch_optional(db)
    .await?;
    let history: Vec<(String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT consent, source, created_at FROM whatsapp_consent_log WHERE account_id = $1 AND phone_number = $2 ORDER BY created_at DESC LIMIT 20")
            .bind(account_id)
            .bind(phone)
            .fetch_all(db)
            .await?;
    let history: Vec<Value> = history.into_iter().map(|(c, s, at)| json!({"consent": c, "source": s, "at": at})).collect();
    Ok(match row {
        Some((consent, source, at, unreachable, inbound)) => json!({
            "phone_number": phone, "consent": consent, "source": source, "changed_at": at,
            "on_whatsapp": unreachable.is_none(), "window_open_until": inbound.map(|t| t + chrono::Duration::hours(24)).filter(|t| *t > Utc::now()),
            "history": history,
        }),
        None => json!({"phone_number": phone, "consent": "never_subscribed", "source": null, "changed_at": null, "on_whatsapp": true, "window_open_until": null, "history": history}),
    })
}

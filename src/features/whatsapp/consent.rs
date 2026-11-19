//! WhatsApp consent, separate from texts: kept per number (`whatsapp_contacts`), changed only by
//! [`set`], with every change in `whatsapp_consent_log`.
//!
//! - **Subscribed** only by an explicit WhatsApp opt-in: one of the account's WhatsApp sign-up words
//!   or opt-in buttons sent on WhatsApp, a sign-up form's WhatsApp consent box, or an API or import
//!   call that says the person agreed to WhatsApp marketing.
//! - **Unsubscribed** by STOP on WhatsApp, Meta's own "stop marketing messages" signal (the
//!   `user_preferences` webhook or error 131050), or an API call. Queued WhatsApp messages to the
//!   number are skipped in the same transaction, and the final claim before every hand-off reads the
//!   consent under the same lock.
//! - **Ordered by when it happened.** Each change carries the time of the event behind it (Meta's
//!   timestamp, which is whole seconds, or now). A change older than the one in force does nothing.
//!   An opt-out wins anywhere in its second, and a subscribe after an opt-out needs a later second,
//!   so a delayed START or sign-up can never undo a newer STOP.
//! - Consent belongs to the number and to the connected sender: a profile whose number changes has
//!   no WhatsApp consent for the new one, and a new business number starts with nobody subscribed.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// SQL true when profile `p` would get a WhatsApp marketing message now: a number, subscribed to
/// WhatsApp on that number, not known to be off WhatsApp, and a connected sender.
pub const REACHABLE_SQL: &str = "(p.phone_number IS NOT NULL
    AND EXISTS (SELECT 1 FROM whatsapp_contacts wc WHERE wc.account_id = p.account_id AND wc.phone_number = p.phone_number AND wc.consent = 'subscribed' AND wc.unreachable_at IS NULL)
    AND EXISTS (SELECT 1 FROM whatsapp_settings ws WHERE ws.account_id = p.account_id AND ws.sender_id IS NOT NULL))";

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
pub const SOURCES: &[&str] = &["keyword", "button", "stop", "start", "meta_opt_out", "meta_error", "api", "import", "form", "dashboard"];

/// The one function that changes WhatsApp consent. Call it inside a transaction holding [`lock`]
/// for the number. `at` is when the event behind it happened. The change applies only when it's
/// newer than the consent in force (an opt-out also at the same moment); it's recorded either way.
/// An applied unsubscribe skips the number's queued WhatsApp messages (reported to webhooks and
/// exports). Returns whether the status changed. `status` is `subscribed` or `unsubscribed`.
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
    at: DateTime<Utc>,
) -> AppResult<bool> {
    if !matches!(status, "subscribed" | "unsubscribed") {
        return Err(AppError::bad("WhatsApp consent must be subscribed or unsubscribed"));
    }
    if !SOURCES.contains(&source) {
        return Err(AppError::bad("unknown consent source"));
    }
    ensure_contact(&mut *conn, account_id, phone, profile_id).await?;
    let applied: Option<String> = sqlx::query_scalar(
        "UPDATE whatsapp_contacts c SET consent = $3, consent_source = $4, consent_at = $5, updated_at = now()
         FROM (SELECT consent FROM whatsapp_contacts WHERE account_id = $1 AND phone_number = $2 FOR UPDATE) old
         WHERE c.account_id = $1 AND c.phone_number = $2
           AND (c.consent_at IS NULL
                -- An opt-out applies unless something happened in a later second.
                OR ($3 = 'unsubscribed' AND date_trunc('second', $5) >= date_trunc('second', c.consent_at))
                -- After an opt-out, a subscribe needs a later second (in the same one, the opt-out wins).
                OR ($3 = 'subscribed' AND c.consent = 'unsubscribed' AND date_trunc('second', $5) > date_trunc('second', c.consent_at))
                OR ($3 = 'subscribed' AND c.consent <> 'unsubscribed' AND $5 > c.consent_at))
         RETURNING old.consent",
    )
    .bind(account_id)
    .bind(phone)
    .bind(status)
    .bind(source)
    .bind(at)
    .fetch_optional(&mut *conn)
    .await?;
    let mut detail = detail;
    if let Some(o) = detail.as_object_mut() {
        o.insert("applied".into(), json!(applied.is_some()));
        o.insert("event_at".into(), json!(at));
    }
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
    if applied.is_some() && status == "unsubscribed" {
        skip_queued(conn, db, account_id, Some(phone), "opted out of WhatsApp").await?;
    }
    Ok(applied.is_some_and(|old| old != status))
}

/// Skip WhatsApp messages that haven't been handed to Meta yet: one number's (campaign and flow
/// messages; answers to the opt-out itself go after it), or with `phone` None every one in the
/// account (the sender changed or was disconnected).
pub async fn skip_queued(conn: &mut PgConnection, db: &sqlx::PgPool, account_id: Uuid, phone: Option<&str>, why: &str) -> AppResult<()> {
    let all = phone.is_none();
    let skipped: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE messages m SET status = 'skipped', error = $3
         WHERE m.account_id = $1 AND m.channel = 'whatsapp' AND ($2::text IS NULL OR m.to_address = $2) AND m.status IN ('queued', 'retrying')
           AND NOT EXISTS (SELECT 1 FROM whatsapp_sent w WHERE w.message_id = m.id AND w.handed_at IS NOT NULL)
           AND ($4 OR NOT EXISTS (SELECT 1 FROM whatsapp_sent w WHERE w.message_id = m.id AND w.kind <> 'marketing'))
         RETURNING m.id",
    )
    .bind(account_id)
    .bind(phone)
    .bind(why)
    .bind(all)
    .fetch_all(&mut *conn)
    .await?;
    // Their held credit goes back.
    sqlx::query(
        "WITH old AS (SELECT message_id, reserved_micros FROM whatsapp_sent WHERE message_id = ANY($2) AND reserved_micros > 0 AND charged_at IS NULL FOR UPDATE),
              r AS (UPDATE whatsapp_sent w SET reserved_micros = 0 FROM old WHERE w.message_id = old.message_id RETURNING old.reserved_micros)
         UPDATE accounts SET balance_micros = balance_micros + (SELECT COALESCE(sum(reserved_micros), 0) FROM r) WHERE id = $1",
    )
    .bind(account_id)
    .bind(&skipped)
    .execute(&mut *conn)
    .await?;
    crate::features::messages_unsent_in(conn, db, account_id, &skipped).await;
    Ok(())
}

/// The account's WhatsApp sender changed (another business account or number) or was
/// disconnected, inside the transaction that changed it: nobody stays subscribed (opt-outs stay),
/// every service window closes, business-scoped ids are forgotten, and queued sends are skipped. A
/// new business account's templates are read again.
pub async fn sender_changed(conn: &mut PgConnection, db: &sqlx::PgPool, account_id: Uuid, business_changed: bool) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO whatsapp_consent_log (id, account_id, phone_number, profile_id, consent, source, detail)
         SELECT gen_random_uuid(), account_id, phone_number, profile_id, 'never_subscribed', 'dashboard', '{\"reason\": \"WhatsApp sender changed\"}'
         FROM whatsapp_contacts WHERE account_id = $1 AND consent = 'subscribed'",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE whatsapp_contacts SET consent = CASE WHEN consent = 'subscribed' THEN 'never_subscribed' ELSE consent END,
                consent_at = CASE WHEN consent = 'subscribed' THEN now() ELSE consent_at END,
                last_inbound_at = NULL, user_id = NULL, updated_at = now()
         WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM whatsapp_bsuid_optouts WHERE account_id = $1").bind(account_id).execute(&mut *conn).await?;
    sqlx::query("DELETE FROM whatsapp_backoff WHERE account_id = $1").bind(account_id).execute(&mut *conn).await?;
    if business_changed {
        sqlx::query("UPDATE whatsapp_templates SET status = 'DELETED', updated_at = now() WHERE account_id = $1 AND status <> 'DELETED'").bind(account_id).execute(&mut *conn).await?;
    }
    skip_queued(conn, db, account_id, None, "WhatsApp sender changed").await
}

/// A signed webhook told us who a business-scoped user id belongs to: remember it, and apply any
/// opt-out that arrived with only that id. Call with the number locked.
pub async fn learn_user_id(conn: &mut PgConnection, db: &sqlx::PgPool, account_id: Uuid, phone: &str, user_id: &str, profile_id: Option<Uuid>) -> AppResult<()> {
    ensure_contact(&mut *conn, account_id, phone, profile_id).await?;
    // The id moves to this number (an id belongs to one person).
    sqlx::query("UPDATE whatsapp_contacts SET user_id = NULL WHERE account_id = $1 AND user_id = $2 AND phone_number <> $3").bind(account_id).bind(user_id).bind(phone).execute(&mut *conn).await?;
    sqlx::query("UPDATE whatsapp_contacts SET user_id = $3 WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(phone).bind(user_id).execute(&mut *conn).await?;
    let pending: Option<DateTime<Utc>> = sqlx::query_scalar("DELETE FROM whatsapp_bsuid_optouts WHERE account_id = $1 AND user_id = $2 RETURNING at")
        .bind(account_id)
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await?;
    if let Some(at) = pending {
        set(conn, db, account_id, phone, profile_id, "unsubscribed", "meta_opt_out", json!({"user_id": user_id, "pending": true}), at).await?;
    }
    Ok(())
}

/// The number a business-scoped user id belongs to, if a signed webhook told us.
pub async fn number_for_user_id(conn: &mut PgConnection, account_id: Uuid, user_id: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT phone_number FROM whatsapp_contacts WHERE account_id = $1 AND user_id = $2").bind(account_id).bind(user_id).fetch_optional(conn).await
}

/// An opt-out from someone known only by a business-scoped user id: applied to their number now if
/// we know it, otherwise kept until a signed webhook connects the two.
pub async fn opt_out_user_id(conn: &mut PgConnection, db: &sqlx::PgPool, account_id: Uuid, user_id: &str, source: &str, at: DateTime<Utc>) -> AppResult<Option<String>> {
    match number_for_user_id(&mut *conn, account_id, user_id).await? {
        Some(phone) => {
            lock(&mut *conn, account_id, &phone).await?;
            set(conn, db, account_id, &phone, None, "unsubscribed", source, json!({"user_id": user_id}), at).await?;
            Ok(Some(phone))
        }
        None => {
            sqlx::query("INSERT INTO whatsapp_bsuid_optouts (account_id, user_id, at) VALUES ($1, $2, $3) ON CONFLICT (account_id, user_id) DO UPDATE SET at = GREATEST(whatsapp_bsuid_optouts.at, EXCLUDED.at)")
                .bind(account_id)
                .bind(user_id)
                .bind(at)
                .execute(&mut *conn)
                .await?;
            Ok(None)
        }
    }
}

/// Meta said a message to this number was undeliverable (131026). That alone doesn't mean the number
/// isn't on WhatsApp (old app, terms not accepted); after `limit` in a row it counts as off WhatsApp
/// until it writes in. Returns whether it's now marked.
pub async fn undeliverable(db: &sqlx::PgPool, account_id: Uuid, phone: &str, limit: i32) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "INSERT INTO whatsapp_contacts (account_id, phone_number, undeliverable_count) VALUES ($1, $2, 1)
         ON CONFLICT (account_id, phone_number) DO UPDATE SET undeliverable_count = whatsapp_contacts.undeliverable_count + 1,
             unreachable_at = CASE WHEN whatsapp_contacts.undeliverable_count + 1 >= $3 THEN COALESCE(whatsapp_contacts.unreachable_at, now()) ELSE whatsapp_contacts.unreachable_at END,
             updated_at = now()
         RETURNING unreachable_at IS NOT NULL",
    )
    .bind(account_id)
    .bind(phone)
    .bind(limit)
    .fetch_one(db)
    .await
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

/// A sign-up form's box naming WhatsApp was ticked with this number. It subscribes the number to
/// WhatsApp marketing, unless the number opted out (a public form can't undo someone's STOP; they
/// opt back in on WhatsApp itself).
pub async fn form_opt_in(db: &sqlx::PgPool, account_id: Uuid, profile_id: Uuid, phone: &str, form_id: Uuid) -> AppResult<bool> {
    let mut tx = db.begin().await?;
    lock(&mut tx, account_id, phone).await?;
    let current: Option<String> = sqlx::query_scalar("SELECT consent FROM whatsapp_contacts WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(phone).fetch_optional(&mut *tx).await?;
    if current.as_deref() == Some("unsubscribed") {
        return Ok(false);
    }
    let changed = set(&mut tx, db, account_id, phone, Some(profile_id), "subscribed", "form", json!({"form_id": form_id}), Utc::now()).await?;
    tx.commit().await?;
    Ok(changed)
}

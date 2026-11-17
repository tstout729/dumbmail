//! The marketing hold: "we have this person's details for service reasons (a booking, an order),
//! but they haven't agreed to marketing".
//!
//! - **Who sets it:** only code that *creates* a profile from a source without an opt-in, and only
//!   on the profile it just created (an integration whose owner hasn't confirmed its contacts opted
//!   in, an import of people who never agreed). Nothing sets it on a profile that already existed.
//! - **What it does:** campaigns never email a held person ([`push_campaign_filter`]); flows email
//!   them only when the flow is marked as service messages (`settings.service_messages`), checked
//!   at the final send gate ([`blocks_email`]). Segments' `can_receive` email rule is false for
//!   them, and `{"type": "property", "field": "marketing_hold"}` selects them.
//! - **What it never does:** let anyone who is unsubscribed or suppressed get anything. Those
//!   checks run first and win, with or without service messages.
//! - **How it clears:** the moment the person becomes subscribed to email, by any route (a form,
//!   double opt-in, the API, an integration's explicit opt-in). A database trigger
//!   (`profiles_clear_marketing_hold`, migration 0022) enforces that a subscribed profile is never
//!   held, so no code path can forget.

use sqlx::{PgExecutor, Postgres, QueryBuilder};
use uuid::Uuid;

/// Why a send to a held person was skipped.
pub const SKIP_REASON: &str = "no marketing consent";

/// The SQL condition (on `profiles p`) for "held back from marketing".
pub const HELD_SQL: &str = "p.marketing_hold";

/// Campaign audiences: leave held people out of email campaigns.
pub fn push_campaign_filter(qb: &mut QueryBuilder<'_, Postgres>, channel: &str) {
    if channel != "sms" {
        qb.push(" AND NOT p.marketing_hold");
    }
}

/// Hold a profile that was just created by a source without an opt-in. Never call this for a
/// profile that already existed. A subscribed profile is never held (the trigger clears it).
pub async fn hold_new_profile<'e>(db: impl PgExecutor<'e>, account_id: Uuid, profile_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE profiles SET marketing_hold = true WHERE id = $1 AND account_id = $2 AND email_consent <> 'subscribed'")
        .bind(profile_id)
        .bind(account_id)
        .execute(db)
        .await?;
    Ok(())
}

/// The final email gate: true when this profile is held and the send isn't from a flow marked as
/// service messages. Callers check unsubscribes and suppressions first; those always win.
pub async fn blocks_email<'e>(db: impl PgExecutor<'e>, profile_id: Uuid, flow_id: Option<Uuid>) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT p.marketing_hold AND NOT COALESCE((SELECT f.definition->'settings'->>'service_messages' = 'true' FROM flows f WHERE f.id = $2 AND f.account_id = p.account_id), false)
         FROM profiles p WHERE p.id = $1",
    )
    .bind(profile_id)
    .bind(flow_id)
    .fetch_optional(db)
    .await
    .map(|v| v.unwrap_or(false))
}

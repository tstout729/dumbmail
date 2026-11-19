//! The marketing hold: "we have this person's details for service reasons (a booking, an order),
//! but they haven't agreed to marketing".
//!
//! - **Who sets it:** only code that *creates* a profile from a source without an opt-in, and only
//!   on the profile it just created (an integration whose owner hasn't confirmed its contacts opted
//!   in, an import of people who never agreed). Nothing sets it on a profile that already existed.
//! - **What it does:** campaigns never email a held person ([`push_campaign_filter`]); flows email
//!   them only when the flow is marked as service messages (`settings.service_messages`) and the
//!   address came from an integration, or for a back-in-stock alert they confirmed; checked in the
//!   send path's own consent queries ([`blocks_email_sql`]). Segments' `can_receive` email rule is false for
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

/// Campaign audiences: leave held people out of email campaigns. The hold is about email consent:
/// texts need their own SMS opt-in, and a push only reaches a device whose owner allowed
/// notifications there, so neither is filtered here.
pub fn push_campaign_filter(qb: &mut QueryBuilder<'_, Postgres>, channel: &str) {
    if channel == "email" {
        qb.push(" AND NOT p.marketing_hold");
    }
}

/// Hold a profile that was just created by a source without an opt-in. Never call this for a
/// profile that already existed. A subscribed profile is never held (the trigger clears it). The
/// hold is recorded in the profile's consent history (`features::people::consent::set_hold_in`).
pub async fn hold_new_profile(db: &sqlx::PgPool, account_id: Uuid, profile_id: Uuid, src: &crate::features::people::ConsentSource) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    crate::features::people::consent::set_hold_in(&mut tx, account_id, profile_id, true, src).await?;
    tx.commit().await
}

/// The SQL condition (on `profiles p`, with the sending flow's id at `flow_param`, NULL for a
/// campaign or test) for "this email to a held person must not go": the person is held, and the
/// send isn't one of the exceptions. Exceptions:
///
/// - a flow marked as service messages (`settings.service_messages`), and only to an address an
///   integration supplied (`p.email_source`); never to one a form or the website script added;
/// - the back-in-stock alert the person asked for and confirmed (a flow started by `Back in
///   Stock`, which only DumbMail records, for a confirmed request). It doesn't clear the hold.
///
/// Used inside the send path's existing checks (`sending::send`), so it adds no query per email.
/// Unsubscribes and suppressions are checked there first and always win.
pub fn blocks_email_sql(flow_param: &str) -> String {
    format!(
        "(p.marketing_hold AND NOT COALESCE((SELECT
            (f.definition->'settings'->>'service_messages' = 'true' AND COALESCE(p.email_source LIKE 'integration:%', false))
            OR (f.trigger->>'type' = 'metric' AND f.trigger->>'metric' = 'Back in Stock')
          FROM flows f WHERE f.id = {flow_param} AND f.account_id = p.account_id), false))"
    )
}

/// [`blocks_email_sql`] on its own, for callers outside the send path.
pub async fn blocks_email<'e>(db: impl PgExecutor<'e>, profile_id: Uuid, flow_id: Option<Uuid>) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!("SELECT {} FROM profiles p WHERE p.id = $1", blocks_email_sql("$2")))
        .bind(profile_id)
        .bind(flow_id)
        .fetch_optional(db)
        .await
        .map(|v| v.unwrap_or(false))
}

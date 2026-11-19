//! What a conversation's order access rests on, and whether it still holds.
//!
//! - Hub: the shopper's hub session (its id and the person's hub generation when it was issued). It
//!   holds while that session is live, the generation hasn't moved (no new address, no sign-out
//!   everywhere, no merge), the email is still the session's and came from the store, and the hub's
//!   Orders section is on.
//! - Texts and WhatsApp: the number they wrote from. It holds while it is still the profile's
//!   number and the store gave it (and any email on the profile came from the store too).
//! - The settings page's test chat: a made-up shopper (always holds; its orders are made up).

use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppResult;
use crate::models::Account;
use crate::state::AppState;

pub fn hub(session: Uuid, generation: i64) -> Value {
    json!({"kind": "hub", "session": session, "generation": generation})
}

pub fn number(number: &str) -> Value {
    json!({"kind": "number", "number": number})
}

pub fn sample() -> Value {
    json!({"kind": "sample"})
}

/// Whether `proof` still allows `profile_id`'s orders to be read in this account.
pub async fn holds(state: &AppState, account: &Account, profile_id: Option<Uuid>, proof: &Value) -> AppResult<bool> {
    match proof["kind"].as_str() {
        Some("sample") => Ok(true),
        Some("hub") => {
            let (Some(pid), Some(session), Some(generation)) = (profile_id, proof["session"].as_str().and_then(|s| s.parse::<Uuid>().ok()), proof["generation"].as_i64()) else {
                return Ok(false);
            };
            let hub = crate::features::hub::settings::load(&state.db, account.id).await?;
            if !hub.enabled || !hub.sections.orders {
                return Ok(false);
            }
            let ok: Option<bool> = sqlx::query_scalar(&format!(
                "SELECT {trusted} FROM hub_sessions s JOIN profiles p ON p.id = s.profile_id AND p.account_id = s.account_id
                 WHERE s.id = $1 AND s.profile_id = $2 AND s.account_id = $3 AND s.revoked_at IS NULL AND s.expires_at > now()
                   AND p.email = s.email AND s.generation = $4
                   AND s.generation = COALESCE((SELECT g.generation FROM hub_generations g WHERE g.profile_id = s.profile_id), 0)",
                trusted = crate::features::hub::data::TRUSTED_EMAIL_SQL
            ))
            .bind(session)
            .bind(pid)
            .bind(account.id)
            .bind(generation)
            .fetch_optional(&state.db)
            .await?;
            Ok(ok == Some(true))
        }
        Some("number") => {
            let (Some(pid), Some(n)) = (profile_id, proof["number"].as_str()) else { return Ok(false) };
            let ok: Option<bool> = sqlx::query_scalar(&format!("SELECT p.phone_number = $3 AND {} FROM profiles p WHERE p.id = $1 AND p.account_id = $2", super::chat::TRUSTED_NUMBER_SQL))
                .bind(pid)
                .bind(account.id)
                .bind(n)
                .fetch_optional(&state.db)
                .await?;
            Ok(ok == Some(true))
        }
        _ => Ok(false),
    }
}

/// The hub generation a session was issued with.
pub async fn session_generation(state: &AppState, session: Uuid) -> AppResult<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT generation FROM hub_sessions WHERE id = $1").bind(session).fetch_optional(&state.db).await?)
}

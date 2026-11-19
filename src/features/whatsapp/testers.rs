//! Test numbers: a team member's own phone, verified by sending the code DumbMail shows them from
//! that phone to the account's WhatsApp number (the message proves they hold it). WhatsApp tests go
//! only to verified numbers of current members, checked again at the hand-off.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::normalize_phone;

/// How long a code can be used.
const CODE_MINUTES: i64 = 30;
/// Numbers per member.
const PER_MEMBER: i64 = 3;

/// SQL true when `$account` (an expression) has a verified test number `$phone` belonging to a
/// current member.
pub fn member_number_sql(account: &str, phone: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM whatsapp_test_numbers tn JOIN memberships mb ON mb.user_id = tn.user_id AND mb.account_id = tn.account_id
                 WHERE tn.account_id = {account} AND tn.phone_number = {phone} AND tn.verified_at IS NOT NULL)"
    )
}

pub async fn list(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(Uuid, String, String, Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT tn.id, tn.phone_number, u.email, tn.verified_at, tn.created_at FROM whatsapp_test_numbers tn
         JOIN users u ON u.id = tn.user_id JOIN memberships mb ON mb.user_id = tn.user_id AND mb.account_id = tn.account_id
         WHERE tn.account_id = $1 ORDER BY tn.created_at",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(id, phone, email, verified, at)| json!({"id": id, "phone_number": phone, "member": email, "verified": verified.is_some(), "verified_at": verified, "created_at": at})).collect())
}

/// Start verifying a member's own number. Returns the code to send from it.
pub async fn add(db: &sqlx::PgPool, account_id: Uuid, user_id: Uuid, raw: &str) -> AppResult<Value> {
    let phone = normalize_phone(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't a phone number (use +15035550100)")))?;
    let mine: i64 = sqlx::query_scalar("SELECT count(*) FROM whatsapp_test_numbers WHERE account_id = $1 AND user_id = $2 AND phone_number <> $3")
        .bind(account_id)
        .bind(user_id)
        .bind(&phone)
        .fetch_one(db)
        .await?;
    if mine >= PER_MEMBER {
        return Err(AppError::bad(format!("Each team member can have up to {PER_MEMBER} test numbers")));
    }
    let code = format!("DM-{:06}", rand::random::<u32>() % 1_000_000);
    let row: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO whatsapp_test_numbers (id, account_id, user_id, phone_number, code) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (account_id, phone_number) DO UPDATE SET code = EXCLUDED.code, created_at = now(), verified_at = NULL, user_id = EXCLUDED.user_id
         WHERE whatsapp_test_numbers.user_id = EXCLUDED.user_id OR whatsapp_test_numbers.verified_at IS NULL
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(user_id)
    .bind(&phone)
    .bind(&code)
    .fetch_optional(db)
    .await?;
    let Some(id) = row else {
        return Err(AppError::Conflict("Another team member already verified this number".into()));
    };
    Ok(json!({"id": id, "phone_number": phone, "code": code, "note": format!("From {phone}, send {code} on WhatsApp to your business number within {CODE_MINUTES} minutes.")}))
}

pub async fn remove(db: &sqlx::PgPool, account_id: Uuid, user_id: Option<Uuid>, any: bool, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM whatsapp_test_numbers WHERE id = $1 AND account_id = $2 AND ($3 OR user_id = $4)")
        .bind(id)
        .bind(account_id)
        .bind(any)
        .bind(user_id)
        .execute(db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("test number"));
    }
    Ok(())
}

/// An inbound message from `from`: if it carries that number's pending code, the number is
/// verified. Returns whether it did (the message is then only a verification).
pub async fn verify_from(conn: &mut sqlx::PgConnection, account_id: Uuid, from: &str, text: &str) -> sqlx::Result<bool> {
    let word = text.trim().to_ascii_uppercase();
    if !word.starts_with("DM-") || word.len() > 12 {
        return Ok(false);
    }
    let n = sqlx::query(
        "UPDATE whatsapp_test_numbers SET verified_at = now() WHERE account_id = $1 AND phone_number = $2 AND code = $3 AND verified_at IS NULL
           AND created_at > now() - make_interval(mins => $4::int)",
    )
    .bind(account_id)
    .bind(from)
    .bind(&word)
    .bind(CODE_MINUTES as i32)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Whether `phone` is a verified test number of a current member.
pub async fn is_member_number(db: &sqlx::PgPool, account_id: Uuid, phone: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!("SELECT {}", member_number_sql("$1", "$2"))).bind(account_id).bind(phone).fetch_one(db).await
}

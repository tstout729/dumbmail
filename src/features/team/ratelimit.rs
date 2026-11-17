//! Fixed-window attempt counters for sign-in, codes, resets and invites, kept in Postgres so they
//! hold across restarts and server instances. Bucket names are hashed before they're stored.

use crate::auth::sha256_hex;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Count one attempt in `bucket`; refuse once more than `limit` happened in the current window.
pub async fn hit(state: &AppState, bucket: &str, limit: i32, window_secs: i64) -> AppResult<()> {
    let key = sha256_hex(&format!("{bucket}|{window_secs}"));
    let hits: i32 = sqlx::query_scalar(
        "INSERT INTO rate_limits (bucket, window_start, hits)
         VALUES ($1, to_timestamp(floor(extract(epoch FROM now()) / $2) * $2), 1)
         ON CONFLICT (bucket, window_start) DO UPDATE SET hits = rate_limits.hits + 1
         RETURNING hits",
    )
    .bind(key)
    .bind(window_secs as f64)
    .fetch_one(&state.db)
    .await?;
    if hits > limit {
        return Err(AppError::RateLimited("Too many attempts. Wait a few minutes and try again.".into()));
    }
    Ok(())
}

/// Give back an attempt counted by [`hit`] (after a success), so only failures add up. Counting
/// first and refunding keeps parallel guesses from slipping past the limit.
pub async fn refund(state: &AppState, bucket: &str, window_secs: i64) -> AppResult<()> {
    let key = sha256_hex(&format!("{bucket}|{window_secs}"));
    sqlx::query("UPDATE rate_limits SET hits = greatest(hits - 1, 0) WHERE bucket = $1 AND window_start = to_timestamp(floor(extract(epoch FROM now()) / $2) * $2)")
        .bind(key)
        .bind(window_secs as f64)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// How many attempts `bucket` has in the current window, without counting one.
pub async fn count(state: &AppState, bucket: &str, window_secs: i64) -> AppResult<i32> {
    let key = sha256_hex(&format!("{bucket}|{window_secs}"));
    Ok(sqlx::query_scalar("SELECT hits FROM rate_limits WHERE bucket = $1 AND window_start = to_timestamp(floor(extract(epoch FROM now()) / $2) * $2)")
        .bind(key)
        .bind(window_secs as f64)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(0))
}

/// Limit by the caller's address, when the proxy told us one.
pub async fn hit_ip(state: &AppState, what: &str, ip: Option<&str>, limit: i32, window_secs: i64) -> AppResult<()> {
    match ip {
        Some(ip) => hit(state, &format!("{what}:ip:{ip}"), limit, window_secs).await,
        None => hit(state, &format!("{what}:ip:unknown"), limit * 20, window_secs).await,
    }
}

/// Drop windows older than a day.
pub async fn prune(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM rate_limits WHERE window_start < now() - interval '1 day'").execute(&state.db).await?;
    Ok(())
}

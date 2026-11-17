//! Indexes on big existing tables (`messages`, `events`, `profiles`), built in the background after
//! the server starts instead of in a migration.
//!
//! `CREATE INDEX CONCURRENTLY` doesn't block writes, but it can take a long time on a large table,
//! and if it's interrupted it leaves an invalid index behind that `IF NOT EXISTS` would then skip.
//! So a module lists its indexes and calls [`ensure`] from its `Feature::after_start`: on a
//! connection of its own, one server (holding an advisory lock) checks each index, drops an
//! invalid one and builds what's missing. A failure is logged and tried again on the next start.
//! Code must work without the indexes, only slower.

use crate::state::AppState;

/// Whether an index exists in the current schema, and if so whether it's valid.
pub const STATE_SQL: &str = "SELECT i.indisvalid AND i.indisready FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
                             WHERE c.relname = $1 AND c.relnamespace = current_schema()::regnamespace";

/// Check and build every index in `indexes` (name, and the `CREATE INDEX CONCURRENTLY` statement
/// that builds it). `lock_key` is the module's own advisory lock; `what` names the indexes in logs.
/// Returns once all are in place, or another server is building them.
pub async fn ensure(state: &AppState, lock_key: i64, what: &str, indexes: &[(&str, &str)]) -> anyhow::Result<()> {
    // A connection of its own, outside the pool, so a long build never holds one live sending needs.
    let mut conn = state.db.acquire().await?.detach();
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(lock_key).fetch_one(&mut conn).await?;
    if !got {
        tracing::info!(what, "another server is building these indexes");
        return Ok(());
    }
    // No time limit on this connection: a cancelled build would only leave an invalid index.
    sqlx::query("SET statement_timeout = 0").execute(&mut conn).await?;
    let mut failed = Vec::new();
    for (name, create) in indexes {
        let res: anyhow::Result<()> = async {
            let valid: Option<bool> = sqlx::query_scalar(STATE_SQL).bind(*name).fetch_optional(&mut conn).await?;
            match valid {
                Some(true) => return Ok(()),
                Some(false) => {
                    tracing::warn!(index = name, what, "dropping an invalid index left by an interrupted build");
                    // `name` comes from a module's constant list, never from input.
                    sqlx::query(&format!("DROP INDEX CONCURRENTLY IF EXISTS {name}")).execute(&mut conn).await?;
                }
                None => {}
            }
            let started = std::time::Instant::now();
            sqlx::query(create).execute(&mut conn).await?;
            tracing::info!(index = name, what, seconds = started.elapsed().as_secs(), "built an index");
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::error!(index = name, what, error = %format!("{e:#}"), "couldn't build an index; trying again on the next start");
            failed.push(*name);
        }
    }
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(lock_key).execute(&mut conn).await;
    let _ = sqlx::Connection::close(conn).await;
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{what} not built: {}", failed.join(", "))
    }
}

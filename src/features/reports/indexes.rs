//! Indexes that make reports fast on big accounts, built in the background after the server starts.
//!
//! `CREATE INDEX CONCURRENTLY` doesn't block sending, but it can take a long time on a large
//! `messages` table, and if it's interrupted it leaves an invalid index behind that `IF NOT EXISTS`
//! would then skip. So these aren't migrations: once at startup, on its own task and connection, one
//! server (holding an advisory lock) checks each index, drops an invalid one and builds what's
//! missing. A failure is logged and tried again on the next start. Reports work without them, slower.

use crate::state::AppState;

/// The indexes this module wants: name and the statement that builds it.
pub const INDEXES: &[(&str, &str)] = &[
    // Opens and clicks by time, for the analytics overview and reports.
    ("messages_account_opened_idx", "CREATE INDEX CONCURRENTLY messages_account_opened_idx ON messages (account_id, opened_at) WHERE opened_at IS NOT NULL"),
    ("messages_account_clicked_idx", "CREATE INDEX CONCURRENTLY messages_account_clicked_idx ON messages (account_id, clicked_at) WHERE clicked_at IS NOT NULL"),
];

/// Advisory lock held while building, so only one server builds at a time.
pub const LOCK_KEY: i64 = 0x0d4d_7265_706f_7274;

/// Whether an index exists in the current schema, and if so whether it's valid.
pub const STATE_SQL: &str = "SELECT i.indisvalid AND i.indisready FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
                             WHERE c.relname = $1 AND c.relnamespace = current_schema()::regnamespace";

/// Check and build every index. Returns once all are in place, or another server is building them.
pub async fn ensure(state: &AppState) -> anyhow::Result<()> {
    build(state, LOCK_KEY, "report", INDEXES).await
}

/// The same for another module's `indexes` (name, `CREATE INDEX CONCURRENTLY ...`), under its own
/// advisory lock `lock_key`; `what` names them in the logs ("report").
pub async fn build(state: &AppState, lock_key: i64, what: &str, indexes: &[(&str, &str)]) -> anyhow::Result<()> {
    // A connection of its own, outside the pool, so a long build never holds one live sending needs.
    let mut conn = state.db.acquire().await?.detach();
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(lock_key).fetch_one(&mut conn).await?;
    if !got {
        tracing::info!("another server is building the {what} indexes");
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
                    tracing::warn!(index = name, "dropping an invalid {what} index left by an interrupted build");
                    // `name` is one of the constants given (here or by another module), never input.
                    sqlx::query(&format!("DROP INDEX CONCURRENTLY IF EXISTS {name}")).execute(&mut conn).await?;
                }
                None => {}
            }
            let started = std::time::Instant::now();
            sqlx::query(create).execute(&mut conn).await?;
            tracing::info!(index = name, seconds = started.elapsed().as_secs(), "built a {what} index");
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::error!(index = name, error = %format!("{e:#}"), "couldn't build a {what} index; trying again on the next start");
            failed.push(*name);
        }
    }
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(lock_key).execute(&mut conn).await;
    let _ = sqlx::Connection::close(conn).await;
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{what} indexes not built: {}", failed.join(", "))
    }
}

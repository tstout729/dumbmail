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
pub const STATE_SQL: &str = crate::features::indexes::STATE_SQL;

/// Check and build every index. Returns once all are in place, or another server is building them.
pub async fn ensure(state: &AppState) -> anyhow::Result<()> {
    crate::features::indexes::ensure(state, LOCK_KEY, "report indexes", INDEXES).await
}

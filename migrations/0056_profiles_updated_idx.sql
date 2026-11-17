-- no-transaction
-- Profiles by last change, for incremental warehouse exports (built without locking writes to profiles).
CREATE INDEX CONCURRENTLY IF NOT EXISTS profiles_account_updated_idx ON profiles (account_id, updated_at, id);

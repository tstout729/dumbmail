-- no-transaction
-- Events in the order they were recorded, for incremental warehouse exports (built without locking writes to events).
CREATE INDEX CONCURRENTLY IF NOT EXISTS events_account_created_idx ON events (account_id, created_at, id);

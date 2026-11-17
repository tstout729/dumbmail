-- no-transaction
-- Opens by time for the analytics overview and reports (built without locking writes to messages).
CREATE INDEX CONCURRENTLY IF NOT EXISTS messages_account_opened_idx ON messages (account_id, opened_at) WHERE opened_at IS NOT NULL;

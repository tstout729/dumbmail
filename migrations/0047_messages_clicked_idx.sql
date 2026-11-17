-- no-transaction
-- Clicks by time for the analytics overview and reports (built without locking writes to messages).
CREATE INDEX CONCURRENTLY IF NOT EXISTS messages_account_clicked_idx ON messages (account_id, clicked_at) WHERE clicked_at IS NOT NULL;

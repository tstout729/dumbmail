-- A failed release (e.g. the provider API is down) is retried at most hourly, so stuck rows
-- can't crowd newly due numbers out of the rent job's batch.
ALTER TABLE phone_numbers ADD COLUMN release_failed_at timestamptz;

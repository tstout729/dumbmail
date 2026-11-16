-- Campaign batches claim each message (status 'sending') before sending it, so a duplicate or retried
-- batch can't send it twice. claimed_at lets the scheduler fail claims left behind by a crash.
ALTER TABLE messages ADD COLUMN claimed_at timestamptz;

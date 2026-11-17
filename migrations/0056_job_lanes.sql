-- A background lane for the job queue. The main workers (sending, flows, segments) take only
-- 'main' jobs; a few background workers take only 'background' jobs (webhooks, exports and other
-- integration work), so that work can never hold up sending. The column's default is a constant,
-- so adding it doesn't rewrite the table; the jobs table only holds waiting work, so the index
-- builds quickly.
ALTER TABLE jobs ADD COLUMN lane text NOT NULL DEFAULT 'main';
CREATE INDEX jobs_lane_ready_idx ON jobs (lane, run_at, id) WHERE locked_at IS NULL;
-- Finding whether an account already has a webhook fan-out job waiting (see webhooks/mod.rs).
CREATE INDEX jobs_background_account_idx ON jobs (kind, (payload->>'account_id')) WHERE locked_at IS NULL AND lane = 'background';

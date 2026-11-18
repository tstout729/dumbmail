-- Machine-translated drafts (src/features/translations/draft.rs).

-- One "Draft with AI" run: the strings to translate are copied in when it starts (never profile
-- data), so a retry works on the same set. At most one queued or running per account.
CREATE TABLE translation_jobs (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_kind text NOT NULL,
    target_id uuid NOT NULL,
    target_part text NOT NULL DEFAULT '',
    language text NOT NULL,
    strings jsonb NOT NULL DEFAULT '[]',
    status text NOT NULL DEFAULT 'queued',
    chars integer NOT NULL,
    translated integer NOT NULL DEFAULT 0,
    skipped integer NOT NULL DEFAULT 0,
    error text,
    requested_by uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE UNIQUE INDEX translation_jobs_one_running ON translation_jobs (account_id) WHERE status IN ('queued', 'running');
CREATE INDEX translation_jobs_account ON translation_jobs (account_id, created_at DESC);

-- Characters sent for machine translation per account and day (the account's time zone), for the
-- daily limit. Reserved when a run starts, in the same statement that checks the limit.
CREATE TABLE translation_usage (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    day date NOT NULL,
    chars bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, day)
);

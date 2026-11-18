-- Scheduled exports to S3-compatible object storage, for loading into a data warehouse.

CREATE TABLE warehouse_destinations (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            text NOT NULL,
    endpoint        text,                               -- NULL: AWS S3 in `region`
    region          text NOT NULL,
    bucket          text NOT NULL,
    prefix          text NOT NULL DEFAULT '',
    path_style      boolean NOT NULL DEFAULT false,
    access_key_enc  text NOT NULL,
    secret_key_enc  text NOT NULL,
    access_key_hint text NOT NULL,
    format          text NOT NULL DEFAULT 'jsonl',      -- jsonl | csv (both gzip'd)
    datasets        text[] NOT NULL,
    schedule        text NOT NULL DEFAULT 'daily',      -- hourly | daily | manual
    enabled         boolean NOT NULL DEFAULT true,
    revision        integer NOT NULL DEFAULT 1,         -- bumped when where or how files are written changes
    baseline_after  timestamptz NOT NULL DEFAULT now(), -- the next run starts no earlier (see run.rs)
    next_run_at     timestamptz,
    last_run_at     timestamptz,
    last_test_at    timestamptz,
    last_test_error text,                               -- a status and a short class, never a response body
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX warehouse_destinations_account_idx ON warehouse_destinations (account_id, created_at);
CREATE INDEX warehouse_destinations_due_idx ON warehouse_destinations (next_run_at) WHERE enabled AND schedule <> 'manual';

CREATE TABLE warehouse_runs (
    id             uuid PRIMARY KEY,
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    destination_id uuid NOT NULL REFERENCES warehouse_destinations(id) ON DELETE CASCADE,
    revision       integer NOT NULL,                 -- the destination revision it was started under
    settings       jsonb NOT NULL,                   -- the destination as it was when the run started (keys encrypted)
    kind           text NOT NULL,                    -- incremental | full
    status         text NOT NULL DEFAULT 'queued',   -- queued | running | succeeded | failed | cancelled
    progress       jsonb NOT NULL DEFAULT '{}',      -- per dataset: window, cursor, files, rows
    window_end     timestamptz NOT NULL,
    rows           bigint NOT NULL DEFAULT 0,
    files          integer NOT NULL DEFAULT 0,
    bytes          bigint NOT NULL DEFAULT 0,
    attempts       integer NOT NULL DEFAULT 0,
    error          text,                             -- a status and a short class, never a response body
    manifest_key   text,
    lease_until    timestamptz,                      -- one worker per run at a time
    created_at     timestamptz NOT NULL DEFAULT now(),
    started_at     timestamptz,
    finished_at    timestamptz
);
CREATE INDEX warehouse_runs_destination_idx ON warehouse_runs (destination_id, created_at DESC);
CREATE UNIQUE INDEX warehouse_runs_one_active_uq ON warehouse_runs (destination_id) WHERE status IN ('queued', 'running');

-- How far each incremental dataset has been exported for a destination.
CREATE TABLE warehouse_watermarks (
    destination_id uuid NOT NULL REFERENCES warehouse_destinations(id) ON DELETE CASCADE,
    dataset        text NOT NULL,
    exported_until timestamptz NOT NULL,
    PRIMARY KEY (destination_id, dataset)
);

-- Rows of mutable datasets that changed without a change column to find them by (a message's
-- delivery, open or bounce; a profile's last activity). Written only for accounts with an export
-- set up. Append-only (a row that changes twice is logged twice; exports dedupe on the row's key),
-- with no foreign key, so writing one never waits on another writer. Kept 7 days.
CREATE TABLE warehouse_changes (
    id         bigserial PRIMARY KEY,
    account_id uuid NOT NULL,
    dataset    text NOT NULL,
    row_id     uuid NOT NULL,
    changed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX warehouse_changes_time_idx ON warehouse_changes (account_id, dataset, changed_at, row_id);
CREATE INDEX warehouse_changes_prune_idx ON warehouse_changes (changed_at);

-- The keys a run exports for a full read of a dataset, taken in one statement (one snapshot) when
-- the dataset starts, so rows added behind a finished file's cursor can't be skipped. Files are
-- then cut from here in `seq` order. Removed when the run ends.
CREATE TABLE warehouse_stage (
    run_id  uuid NOT NULL REFERENCES warehouse_runs(id) ON DELETE CASCADE,
    dataset text NOT NULL,
    seq     bigint NOT NULL,
    key_a   uuid NOT NULL,
    key_b   uuid,
    PRIMARY KEY (run_id, dataset, seq)
);

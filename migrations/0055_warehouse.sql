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
    next_run_at     timestamptz,
    last_run_at     timestamptz,
    last_test_at    timestamptz,
    last_test_error text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX warehouse_destinations_account_idx ON warehouse_destinations (account_id, created_at);
CREATE INDEX warehouse_destinations_due_idx ON warehouse_destinations (next_run_at) WHERE enabled AND schedule <> 'manual';

CREATE TABLE warehouse_runs (
    id             uuid PRIMARY KEY,
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    destination_id uuid NOT NULL REFERENCES warehouse_destinations(id) ON DELETE CASCADE,
    kind           text NOT NULL,                    -- incremental | full
    status         text NOT NULL DEFAULT 'queued',   -- queued | running | succeeded | failed
    progress       jsonb NOT NULL DEFAULT '{}',      -- per dataset: window, cursor, files, rows
    window_end     timestamptz NOT NULL,
    rows           bigint NOT NULL DEFAULT 0,
    files          integer NOT NULL DEFAULT 0,
    bytes          bigint NOT NULL DEFAULT 0,
    attempts       integer NOT NULL DEFAULT 0,
    error          text,
    manifest_key   text,
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

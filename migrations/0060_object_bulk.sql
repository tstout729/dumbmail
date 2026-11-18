-- Bulk object upserts: the batch is kept until a background job has written it, a chunk at a
-- time; `done` says how far it got, so a job that dies part-way carries on from there.
CREATE TABLE object_bulk_upserts (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status      text NOT NULL DEFAULT 'queued', -- queued | running | done | failed
    source      text NOT NULL,
    items       jsonb,
    total       integer NOT NULL,
    done        integer NOT NULL DEFAULT 0,
    created     integer NOT NULL DEFAULT 0,
    updated     integer NOT NULL DEFAULT 0,
    unchanged   integer NOT NULL DEFAULT 0,
    failed      integer NOT NULL DEFAULT 0,
    -- The first errors: [{index, external_id, error}].
    errors      jsonb NOT NULL DEFAULT '[]',
    created_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE INDEX object_bulk_upserts_account_idx ON object_bulk_upserts(account_id, created_at DESC);
CREATE INDEX object_bulk_upserts_open_idx ON object_bulk_upserts(account_id) WHERE status IN ('queued', 'running');

-- Adding people who already qualify to a flow (back-populate). The people are fixed when it starts
-- (flow_backfill_members); each is marked done in the same transaction that starts their run, so a
-- retried batch adds nobody twice.

CREATE TABLE flow_backfills (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    flow_id      uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    status       text NOT NULL DEFAULT 'running', -- running | done | cancelled | failed
    max_people   integer NOT NULL,
    total        integer,
    added        integer NOT NULL DEFAULT 0,
    skipped      integer NOT NULL DEFAULT 0,
    error        text,
    created_by   uuid,
    created_at   timestamptz NOT NULL DEFAULT now(),
    snapshot_at  timestamptz,
    finished_at  timestamptz
);
-- One back-populate per flow at a time, enforced by the insert.
CREATE UNIQUE INDEX flow_backfills_one_running_uq ON flow_backfills (flow_id) WHERE status = 'running';
CREATE INDEX flow_backfills_flow_idx ON flow_backfills (flow_id, created_at DESC);

CREATE TABLE flow_backfill_members (
    backfill_id uuid NOT NULL REFERENCES flow_backfills(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    done_at     timestamptz,
    outcome     text, -- added | skipped: <why>
    run_id      uuid,
    PRIMARY KEY (backfill_id, profile_id)
);
CREATE INDEX flow_backfill_members_todo_idx ON flow_backfill_members (backfill_id, profile_id) WHERE done_at IS NULL;

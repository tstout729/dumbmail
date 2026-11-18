-- Flows off custom objects.

-- Object-date triggers (N days or hours before or after a date field of each object): one row per
-- (flow, object, moment it is due), the same shape as flow_event_date_fires and run by the same
-- scheduler. A date that moves drops the pending row and adds one for the new moment; a moment
-- that already fired never fires again.
CREATE TABLE flow_object_date_fires (
    flow_id    uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    object_id  uuid NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    due_at     timestamptz NOT NULL,
    -- pending | started | missed | filtered | not_started | not_live
    outcome    text NOT NULL DEFAULT 'pending',
    run_id     uuid,
    runs       integer NOT NULL DEFAULT 0,
    fired_at   timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (flow_id, object_id, due_at)
);
CREATE INDEX flow_object_date_fires_due_idx ON flow_object_date_fires(due_at) WHERE outcome = 'pending';
CREATE INDEX flow_object_date_fires_object_idx ON flow_object_date_fires(object_id);

-- Which trigger an object-date flow's existing objects were scheduled for, and how far that got.
CREATE TABLE flow_object_date_seeds (
    flow_id        uuid PRIMARY KEY REFERENCES flows(id) ON DELETE CASCADE,
    trigger        jsonb NOT NULL,
    last_object_id uuid,
    done           boolean NOT NULL DEFAULT false
);

-- Object created/updated triggers: one row per (change, flow), written in the same transaction as
-- the runs it starts, so a retried job never starts them twice.
CREATE TABLE object_flow_starts (
    change_id  uuid NOT NULL REFERENCES object_changes(id) ON DELETE CASCADE,
    flow_id    uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    runs       integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (change_id, flow_id)
);

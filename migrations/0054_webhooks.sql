-- Outgoing webhooks: endpoints an account registers (in the dashboard, or as REST hooks from
-- Zapier and Make), an outbox of changes waiting to be matched to endpoints, one delivery row per
-- event per endpoint, and the per-account secret that signs flow webhook steps.

CREATE TABLE webhook_endpoints (
    id                     uuid PRIMARY KEY,
    account_id             uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    url                    text NOT NULL,
    description            text NOT NULL DEFAULT '',
    topics                 text[] NOT NULL,
    metrics                text[] NOT NULL DEFAULT '{}',     -- event.tracked: only these metrics (empty: all)
    filter                 jsonb,                            -- [{field, op, value}] on the event's data; all must match
    trusted_only           boolean NOT NULL DEFAULT false,   -- skip events a browser sent (source = browser)
    enabled                boolean NOT NULL DEFAULT true,
    source                 text NOT NULL DEFAULT 'dashboard', -- dashboard | api | zapier | make
    api_key_id             uuid REFERENCES api_keys(id) ON DELETE CASCADE, -- REST hooks: the key that owns it
    batch_size             integer NOT NULL DEFAULT 50,
    rate_per_second        integer NOT NULL DEFAULT 10,
    revision               integer NOT NULL DEFAULT 1,       -- bumped when the URL changes; older deliveries are cancelled
    revision_started_at    timestamptz NOT NULL DEFAULT now(), -- only changes captured since then go to this revision's URL
    secret_enc             text NOT NULL,
    secret_hint            text NOT NULL,
    previous_secret_enc    text,                             -- still signs for a day after a rotation
    previous_secret_until  timestamptz,
    pending                integer NOT NULL DEFAULT 0,       -- undelivered rows (kept by the code, recounted hourly)
    dropped_total          bigint NOT NULL DEFAULT 0,        -- rows dropped because the backlog was full
    last_dropped_at        timestamptz,
    drop_alerted           boolean NOT NULL DEFAULT false,   -- the owner was told about dropping
    consecutive_failures   integer NOT NULL DEFAULT 0,
    failing_since          timestamptz,
    retry_at               timestamptz,                      -- back off the whole endpoint after a failure
    disabled_reason        text,
    lease_until            timestamptz,                      -- one sender per endpoint at a time
    wake_queued            boolean NOT NULL DEFAULT false,   -- a delivery job is already queued
    last_resend_at         timestamptz,
    last_success_at        timestamptz,
    last_failure_at        timestamptz,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_endpoints_account_idx ON webhook_endpoints (account_id, created_at);
CREATE INDEX webhook_endpoints_key_idx ON webhook_endpoints (api_key_id) WHERE api_key_id IS NOT NULL;

-- Changes waiting to be matched to endpoints. Written with one cheap insert where the change
-- happens (in the same transaction when there is one); a background job per account moves them
-- into webhook_deliveries. Append-only, with no foreign key, so writing a row never waits on a
-- lock someone else holds (an account row locked for billing, say). Rows of a deleted account
-- match no endpoint and are dropped when they fan out.
CREATE TABLE webhook_outbox (
    id         bigserial PRIMARY KEY,
    account_id uuid NOT NULL,
    event_id   uuid NOT NULL,                     -- the envelope id receivers dedupe on
    topic      text NOT NULL,
    metric     text,                              -- event.tracked: the metric's name
    source     text NOT NULL DEFAULT 'system',    -- browser | api | integration | system
    data       jsonb NOT NULL,                    -- the envelope's data; {"$ref": ...} parts are filled in when it fans out
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_outbox_account_idx ON webhook_outbox (account_id, id);

-- How many rows each account has waiting in the outbox, counted by the background sweep. Servers
-- read it with their cached subscriptions and stop adding rows for an account that is full.
CREATE TABLE webhook_outbox_counts (
    account_id uuid PRIMARY KEY,
    rows       bigint NOT NULL,
    counted_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE webhook_deliveries (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    endpoint_id     uuid NOT NULL REFERENCES webhook_endpoints(id) ON DELETE CASCADE,
    revision        integer NOT NULL DEFAULT 1,        -- the endpoint revision (URL) it was queued for
    event_id        uuid NOT NULL,                     -- the envelope id, the same for every endpoint
    topic           text NOT NULL,
    payload         jsonb NOT NULL,                    -- the envelope as sent
    is_test         boolean NOT NULL DEFAULT false,
    status          text NOT NULL DEFAULT 'pending',   -- pending | sending (a test in flight) | delivered | failed | dropped | cancelled
    attempts        integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL DEFAULT now() + interval '24 hours', -- stop retrying after this
    last_attempt_at timestamptz,
    response_code   integer,
    error           text,                              -- a short class from a fixed list, never a response body
    duration_ms     integer,
    delivered_at    timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (endpoint_id, created_at, id) WHERE status = 'pending';
CREATE INDEX webhook_deliveries_expiry_idx ON webhook_deliveries (expires_at) WHERE status = 'pending';
CREATE INDEX webhook_deliveries_log_idx ON webhook_deliveries (endpoint_id, created_at DESC);
CREATE INDEX webhook_deliveries_created_idx ON webhook_deliveries (created_at);
-- One delivery per event per endpoint: a change reported twice goes out once.
CREATE UNIQUE INDEX webhook_deliveries_event_uq ON webhook_deliveries (endpoint_id, event_id) WHERE NOT is_test;

CREATE TABLE webhook_signing_secrets (
    account_id  uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    secret_enc  text NOT NULL,
    secret_hint text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

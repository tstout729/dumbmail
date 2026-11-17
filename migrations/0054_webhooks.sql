-- Outgoing webhooks: endpoints an account registers (in the dashboard, or as REST hooks from
-- Zapier and Make), one delivery row per event per endpoint, and the per-account secret that signs
-- flow webhook steps.

CREATE TABLE webhook_endpoints (
    id                     uuid PRIMARY KEY,
    account_id             uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    url                    text NOT NULL,
    description            text NOT NULL DEFAULT '',
    topics                 text[] NOT NULL,
    metrics                text[] NOT NULL DEFAULT '{}',     -- event.tracked: only these metrics (empty: all)
    filter                 jsonb,                            -- [{field, op, value}] on the event's data; all must match
    enabled                boolean NOT NULL DEFAULT true,
    source                 text NOT NULL DEFAULT 'dashboard', -- dashboard | api | zapier | make
    batch_size             integer NOT NULL DEFAULT 50,
    rate_per_second        integer NOT NULL DEFAULT 10,
    secret_enc             text NOT NULL,
    secret_hint            text NOT NULL,
    previous_secret_enc    text,                             -- still signs for a day after a rotation
    previous_secret_until  timestamptz,
    consecutive_failures   integer NOT NULL DEFAULT 0,
    failing_since          timestamptz,
    retry_at               timestamptz,                      -- back off the whole endpoint after a failure
    disabled_reason        text,
    lease_until            timestamptz,                      -- one sender per endpoint at a time
    wake_queued            boolean NOT NULL DEFAULT false,   -- a delivery job is already queued
    last_success_at        timestamptz,
    last_failure_at        timestamptz,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_endpoints_account_idx ON webhook_endpoints (account_id, created_at);

CREATE TABLE webhook_deliveries (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    endpoint_id     uuid NOT NULL REFERENCES webhook_endpoints(id) ON DELETE CASCADE,
    event_id        uuid NOT NULL,                     -- the envelope id, the same for every endpoint
    topic           text NOT NULL,
    payload         jsonb NOT NULL,                    -- the envelope as sent
    is_test         boolean NOT NULL DEFAULT false,
    status          text NOT NULL DEFAULT 'pending',   -- pending | delivered | failed
    attempts        integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL DEFAULT now() + interval '24 hours', -- stop retrying after this
    last_attempt_at timestamptz,
    response_code   integer,
    response_body   text,
    error           text,
    duration_ms     integer,
    delivered_at    timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (endpoint_id, next_attempt_at) WHERE status = 'pending';
CREATE INDEX webhook_deliveries_log_idx ON webhook_deliveries (endpoint_id, created_at DESC);
CREATE INDEX webhook_deliveries_created_idx ON webhook_deliveries (created_at);

CREATE TABLE webhook_signing_secrets (
    account_id  uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    secret_enc  text NOT NULL,
    secret_hint text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

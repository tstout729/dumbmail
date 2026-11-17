-- Custom integrations: incoming webhooks and scheduled pulls translated by a sandboxed script.
CREATE TABLE integrations (
    id                     uuid PRIMARY KEY,
    account_id             uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name                   text NOT NULL,
    recipe                 text,
    status                 text NOT NULL DEFAULT 'enabled', -- enabled | disabled
    -- The secret inbound URL token: looked up by its SHA-256, kept encrypted so the owner can copy the URL again.
    token_hash             text NOT NULL UNIQUE,
    token_enc              text NOT NULL,
    signature              jsonb NOT NULL DEFAULT '{"type": "none"}',
    signature_secret_enc   text,
    signature_secret_hint  text,
    script                 text NOT NULL DEFAULT '',
    settings               jsonb NOT NULL DEFAULT '{}',
    pull                   jsonb,
    pull_next_at           timestamptz,
    credentials_enc        text,
    credential_hints       jsonb NOT NULL DEFAULT '{}',
    oauth_token_enc        text,
    oauth_token_expires_at timestamptz,
    oauth_token_requests   timestamptz[] NOT NULL DEFAULT '{}',
    state                  jsonb NOT NULL DEFAULT '{}',
    backfill               jsonb,
    deliveries_ok          bigint NOT NULL DEFAULT 0,
    deliveries_failed      bigint NOT NULL DEFAULT 0,
    last_delivery_at       timestamptz,
    last_error             text,
    last_error_at          timestamptz,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX integrations_account_idx ON integrations(account_id, created_at);
CREATE INDEX integrations_pull_due_idx ON integrations(pull_next_at) WHERE pull_next_at IS NOT NULL;

CREATE TABLE integration_deliveries (
    id              uuid PRIMARY KEY,
    integration_id  uuid NOT NULL REFERENCES integrations(id) ON DELETE CASCADE,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source          text NOT NULL,                   -- webhook | pull | test | replay
    status          text NOT NULL DEFAULT 'pending', -- pending | ok | error | ignored
    content_type    text,
    headers         jsonb NOT NULL DEFAULT '{}',
    query           jsonb NOT NULL DEFAULT '{}',
    body            text,
    request         jsonb,
    attempts        integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz,
    error           text,
    result          jsonb,
    duration_ms     integer,
    replay_of       uuid,
    created_at      timestamptz NOT NULL DEFAULT now(),
    processed_at    timestamptz
);
CREATE INDEX integration_deliveries_list_idx ON integration_deliveries(integration_id, created_at DESC);
CREATE INDEX integration_deliveries_open_idx ON integration_deliveries(integration_id, status) WHERE status IN ('pending', 'error');
CREATE INDEX integration_deliveries_created_idx ON integration_deliveries(created_at);

-- Where a profile came from (e.g. 'integration:<id>'), set when an integration creates it.
ALTER TABLE profiles ADD COLUMN source text;

-- Event-date flow triggers: each event starts a flow at most once.
CREATE TABLE flow_event_date_fires (
    flow_id    uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    event_id   uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    outcome    text NOT NULL,
    fired_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (flow_id, event_id)
);
CREATE INDEX flow_event_date_fires_event_idx ON flow_event_date_fires(event_id);

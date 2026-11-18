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
    -- What the cached token was issued for (a hash of token URL, client, scope, audience and pull origin).
    oauth_token_key        text,
    -- A token refresh in flight: whoever holds the lease asks the app; others wait for its result.
    oauth_lease_id         uuid,
    oauth_lease_until      timestamptz,
    -- Bumped whenever pull settings, credentials or settings change; a run that started on an older
    -- version never writes its token into the cache.
    auth_version           bigint NOT NULL DEFAULT 0,
    -- Everyone this integration adds has agreed to get marketing email (set by the owner).
    contacts_opted_in      boolean NOT NULL DEFAULT false,
    -- Where a webhook's own message id is (resends with the same id are ignored): {"header": "svix-id"} or {"field": "id"}.
    message_id             jsonb,
    -- The metrics the script may record. They are marked server-only when the integration is
    -- saved, so a browser can never send them, even before the first real one arrives.
    metrics                text[] NOT NULL DEFAULT '{}',
    -- Paths removed from every payload before it is stored or shown (e.g. "results[].notes",
    -- "**.keyCode"); the script still sees the whole payload in memory.
    redact                 text[] NOT NULL DEFAULT '{}',
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
    -- The app's own id for this message (resends with the same id are acknowledged and ignored).
    message_id      text,
    -- Actions already written, by index, with a hash of each: a retry or replay skips them.
    applied         jsonb NOT NULL DEFAULT '{}',
    -- The action list the script returned the first time it ran successfully for this delivery
    -- (missing ids and times filled in). Retries apply this list; they never run the script again.
    actions         jsonb,
    body_bytes      integer NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    processed_at    timestamptz
);
CREATE INDEX integration_deliveries_list_idx ON integration_deliveries(integration_id, created_at DESC);
CREATE INDEX integration_deliveries_open_idx ON integration_deliveries(integration_id, status) WHERE status IN ('pending', 'error');
CREATE INDEX integration_deliveries_created_idx ON integration_deliveries(created_at);
CREATE UNIQUE INDEX integration_deliveries_message_uq ON integration_deliveries(integration_id, message_id) WHERE message_id IS NOT NULL;
CREATE INDEX integration_deliveries_account_open_idx ON integration_deliveries(account_id) WHERE status = 'pending' OR (status = 'error' AND next_attempt_at IS NOT NULL);
CREATE INDEX integration_deliveries_account_body_idx ON integration_deliveries(account_id, created_at) WHERE body IS NOT NULL;

-- Stored payload bytes per account, kept by a trigger, so admission can check the budget without
-- adding up every delivery. (No foreign key: rows are tidied with the account's deliveries.)
CREATE TABLE integration_payload_bytes (
    account_id uuid PRIMARY KEY,
    bytes      bigint NOT NULL DEFAULT 0
);
CREATE FUNCTION integration_payload_bytes_track() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    delta bigint := 0;
    acct uuid;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        delta := delta - OLD.body_bytes;
        acct := OLD.account_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        delta := delta + NEW.body_bytes;
        acct := NEW.account_id;
    END IF;
    IF delta <> 0 THEN
        INSERT INTO integration_payload_bytes (account_id, bytes) VALUES (acct, delta)
        ON CONFLICT (account_id) DO UPDATE SET bytes = integration_payload_bytes.bytes + EXCLUDED.bytes;
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER integration_payload_bytes_track AFTER INSERT OR DELETE OR UPDATE OF body_bytes ON integration_deliveries
    FOR EACH ROW EXECUTE FUNCTION integration_payload_bytes_track();

-- Where a profile came from (e.g. 'integration:<id>'), set when an integration creates it.
ALTER TABLE profiles ADD COLUMN source text;

-- Event-date flow triggers: one row per (flow, event), written when the event is recorded (or the
-- flow goes live), with the moment it is due. The scheduler claims due rows and starts the flow.
CREATE TABLE flow_event_date_fires (
    flow_id    uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    event_id   uuid NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    due_at     timestamptz NOT NULL,
    -- pending | queued | started | missed | filtered | superseded | cancelled | not_started
    outcome    text NOT NULL DEFAULT 'pending',
    run_id     uuid,
    fired_at   timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (flow_id, event_id)
);
CREATE INDEX flow_event_date_fires_due_idx ON flow_event_date_fires(due_at) WHERE outcome = 'pending';
CREATE INDEX flow_event_date_fires_event_idx ON flow_event_date_fires(event_id);

-- Which trigger an event-date flow's existing events were scheduled for, and how far that got.
CREATE TABLE flow_event_date_seeds (
    flow_id       uuid PRIMARY KEY REFERENCES flows(id) ON DELETE CASCADE,
    trigger       jsonb NOT NULL,
    last_event_id uuid,
    done          boolean NOT NULL DEFAULT false
);

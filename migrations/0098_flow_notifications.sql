-- Notification steps in flows (see docs/FLOWS_PLUS.md): addresses outside the team confirm before
-- they get anything, each notification is claimed before it is sent, and a per-flow daily counter
-- caps how many go out.

CREATE TABLE flow_notify_addresses (
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email         text NOT NULL,
    token_hash    text,
    requested_at  timestamptz NOT NULL DEFAULT now(),
    confirmed_at  timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, email)
);
CREATE UNIQUE INDEX flow_notify_addresses_token_uq ON flow_notify_addresses (token_hash) WHERE token_hash IS NOT NULL;

-- One row per run, step and recipient: the claim, written before the provider call.
CREATE TABLE flow_notifications (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    flow_id             uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    run_id              uuid NOT NULL,
    node_id             text NOT NULL,
    to_email            text NOT NULL,
    status              text NOT NULL DEFAULT 'sending', -- sending | sent | skipped | failed | unknown
    error               text,
    subject             text,
    provider_message_id text,
    cost_micros         bigint,
    created_at          timestamptz NOT NULL DEFAULT now(),
    sent_at             timestamptz,
    UNIQUE (run_id, node_id, to_email)
);
CREATE INDEX flow_notifications_flow_idx ON flow_notifications (flow_id, created_at);
CREATE INDEX flow_notifications_sending_idx ON flow_notifications (created_at) WHERE status = 'sending';

-- Notifications a flow sent (or claimed) on each local day, for the daily cap.
CREATE TABLE flow_notify_usage (
    flow_id uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    day     date NOT NULL,
    sent    integer NOT NULL DEFAULT 0,
    PRIMARY KEY (flow_id, day)
);

-- Notification steps in flows (see docs/FLOWS_PLUS.md): addresses outside the team confirm before
-- they get anything, each notification is claimed before it is sent, and a per-flow daily counter
-- caps how many go out.

CREATE TABLE flow_notify_addresses (
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email         text NOT NULL,
    token_hash    text,
    requested_at  timestamptz NOT NULL DEFAULT now(),
    confirmed_at  timestamptz,
    -- Removed addresses are kept (without their confirmation), so removing and adding one again
    -- can't be used to send it link after link.
    removed_at    timestamptz,
    -- A hard bounce or complaint from a notification: it gets no more (an Admin can clear a bounce).
    blocked_reason text,
    blocked_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, email)
);

-- Every confirmation email sent, for the hourly per-address and daily per-account limits.
CREATE TABLE flow_notify_confirm_sends (
    id         bigserial PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email      text NOT NULL,
    sent_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX flow_notify_confirm_sends_idx ON flow_notify_confirm_sends (account_id, sent_at);
CREATE INDEX flow_notify_confirm_sends_email_idx ON flow_notify_confirm_sends (email, sent_at);
CREATE UNIQUE INDEX flow_notify_addresses_token_uq ON flow_notify_addresses (token_hash) WHERE token_hash IS NOT NULL;

-- One row per run, step and recipient: who the step emails for that run and what it says, fixed
-- the first time it runs, then `handing` just before the provider call.
CREATE TABLE flow_notifications (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    flow_id             uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    run_id              uuid NOT NULL,
    node_id             text NOT NULL,
    to_email            text NOT NULL,
    status              text NOT NULL DEFAULT 'pending', -- pending | handing | sent | skipped | failed | unconfirmed
    error               text,
    subject             text,
    body                text,
    provider_message_id text,
    cost_micros         bigint,
    created_at          timestamptz NOT NULL DEFAULT now(),
    handed_at           timestamptz,
    sent_at             timestamptz,
    UNIQUE (run_id, node_id, to_email)
);
CREATE INDEX flow_notifications_flow_idx ON flow_notifications (flow_id, created_at);
CREATE INDEX flow_notifications_handing_idx ON flow_notifications (handed_at) WHERE status = 'handing';
CREATE INDEX flow_notifications_provider_idx ON flow_notifications (provider_message_id) WHERE provider_message_id IS NOT NULL;

-- Notifications a flow sent (or claimed) on each local day, for the daily cap.
CREATE TABLE flow_notify_usage (
    flow_id uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    day     date NOT NULL,
    sent    integer NOT NULL DEFAULT 0,
    PRIMARY KEY (flow_id, day)
);

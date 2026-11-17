-- Transactional email API (receipts, password resets, shipping notices). See src/features/transactional.

-- Transactional mail goes out without marketing consent, so it never counts as a marketing message:
-- smart sending and attribution skip it.
ALTER TABLE messages ADD COLUMN transactional boolean NOT NULL DEFAULT false;

-- Templates the transactional API may send. Set only when a template is created as transactional.
ALTER TABLE templates ADD COLUMN transactional boolean NOT NULL DEFAULT false;

-- One row per API send (and resend). The message row carries delivery and engagement; this keeps the
-- request, so a retry with the same idempotency key returns the first result and a send can be resent.
CREATE TABLE transactional_sends (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id      uuid REFERENCES messages(id) ON DELETE SET NULL,
    profile_id      uuid REFERENCES profiles(id) ON DELETE SET NULL,
    to_email        text NOT NULL,
    template_id     uuid REFERENCES templates(id) ON DELETE SET NULL,
    subject         text,
    -- queued | sent | skipped | failed (delivery after 'sent' is read from the message)
    status          text NOT NULL DEFAULT 'queued',
    error           text,
    idempotency_key text,
    request_hash    text NOT NULL,
    -- What was asked for (recipient, content or template, variables, options), for resends.
    request         jsonb NOT NULL,
    metadata        jsonb NOT NULL DEFAULT '{}',
    track_clicks    boolean NOT NULL DEFAULT false,
    resent_from     uuid REFERENCES transactional_sends(id) ON DELETE SET NULL,
    via             text NOT NULL DEFAULT 'api',
    created_at      timestamptz NOT NULL DEFAULT now(),
    sent_at         timestamptz
);
CREATE INDEX transactional_sends_account_idx ON transactional_sends (account_id, created_at DESC);
CREATE INDEX transactional_sends_to_idx ON transactional_sends (account_id, to_email, created_at DESC);
CREATE INDEX transactional_sends_message_idx ON transactional_sends (message_id) WHERE message_id IS NOT NULL;
CREATE UNIQUE INDEX transactional_sends_idem_uq ON transactional_sends (account_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

-- Per-account caps and the automatic transactional pause. Accounts without a row use the defaults.
CREATE TABLE transactional_settings (
    account_id       uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    per_minute_limit integer,
    daily_limit      integer,
    paused_at        timestamptz,
    paused_reason    text,
    updated_at       timestamptz NOT NULL DEFAULT now()
);

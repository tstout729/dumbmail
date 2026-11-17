-- Sending guardrails for the shared SES account.

-- Warm-up: new accounts start with a small 24-hour email limit that grows while their bounces and
-- complaints stay low (see src/deliverability.rs). An operator can set a fixed limit instead.
ALTER TABLE accounts
    ADD COLUMN email_daily_limit_override integer,
    -- Skip campaign email to people who got many emails in the last 180 days and did nothing at all.
    ADD COLUMN email_sunset_enabled boolean NOT NULL DEFAULT true;

-- Confirmation emails count toward bounce and complaint rates but not toward the warm-up limit, so
-- sign-up forms (or a bot filling them in) can't raise it.
ALTER TABLE email_sends ADD COLUMN counts_toward_limit boolean NOT NULL DEFAULT true;

-- Double opt-in: a sign-up to a double opt-in list waits here until the person confirms by email.
CREATE TABLE optin_confirmations (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    list_id      uuid NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
    form_id      uuid,
    email        text NOT NULL,
    -- The sign-up event's properties (form, version), recorded when the person confirms.
    properties   jsonb NOT NULL DEFAULT '{}',
    created_at   timestamptz NOT NULL DEFAULT now(),
    confirmed_at timestamptz
);
CREATE INDEX optin_confirmations_email_idx ON optin_confirmations (account_id, email, created_at DESC);
CREATE INDEX optin_confirmations_recent_idx ON optin_confirmations (account_id, created_at DESC);

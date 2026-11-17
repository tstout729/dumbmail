-- Texting consent safety: carrier-level opt-outs, the evidence kept for each keyword sign-up,
-- automatic texts written down before they're sent, hourly budgets for them, and double opt-ins
-- tied to a prompt that actually went out.

-- A number that texted STOP (or a provider said opted out) can't be texted at all from this
-- account. Each row is one pair: their number and the number of ours that got the opt-out. Only
-- START or UNSTOP (or a re-opt-in word the provider documents) sent to that same number of ours
-- lifts that row; the number stays blocked while any row for it remains. This is separate from
-- the profile's marketing consent (`profiles.sms_consent`).
--
-- It starts empty: people who texted STOP before this table existed are still protected as they
-- were, by their profile's SMS consent being `unsubscribed`. No history is scanned, so this
-- migration holds its locks only for an instant.
CREATE TABLE sms_opt_outs (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    phone_number text NOT NULL,
    our_number text NOT NULL,
    source text NOT NULL, -- keyword | phrase | provider
    inbound_id uuid REFERENCES sms_inbound(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, phone_number, our_number)
);

-- HELP answers: at most one per number per hour, enforced by the write itself.
CREATE TABLE sms_help_answers (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    phone_number text NOT NULL,
    last_at timestamptz NOT NULL,
    PRIMARY KEY (account_id, phone_number)
);

-- Every automatic text, written down before it's sent. Its id is also the id of the message it
-- goes out as, so a job that runs twice can't send it twice.
CREATE TABLE sms_auto_sends (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    conversation_id uuid NOT NULL REFERENCES sms_conversations(id) ON DELETE CASCADE,
    -- The text it answers.
    inbound_id uuid NOT NULL REFERENCES sms_inbound(id) ON DELETE CASCADE,
    -- auto_reply | keyword_reply | confirmation | optin_request | help
    purpose text NOT NULL,
    body text NOT NULL,
    -- Their number, and ours (the one they texted).
    to_number text NOT NULL,
    from_number text NOT NULL,
    keyword_id uuid REFERENCES sms_keywords(id) ON DELETE SET NULL,
    keyword_version integer,
    -- Evidence carried to the consent record (the sign-up text, the prompt).
    context jsonb NOT NULL DEFAULT '{}',
    status text NOT NULL DEFAULT 'queued', -- queued | sending | sent | skipped | failed | cancelled
    error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE (inbound_id, purpose)
);

CREATE INDEX sms_auto_sends_queued ON sms_auto_sends (account_id, to_number) WHERE status = 'queued';

-- Automatic texts counted per hour, per account ('account') and per receiving number.
CREATE TABLE sms_auto_budget (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    scope text NOT NULL,
    hour timestamptz NOT NULL,
    count integer NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, scope, hour)
);

-- A keyword sign-up with double opt-in, waiting for "Y". It counts only once the prompt was
-- accepted by the provider (prompt_sent_at), for 7 days from then, from the number it went to.
CREATE TABLE sms_pending_optins (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    phone_number text NOT NULL,
    our_number text NOT NULL,
    keyword_id uuid NOT NULL REFERENCES sms_keywords(id) ON DELETE CASCADE,
    keyword_version integer NOT NULL,
    inbound_id uuid REFERENCES sms_inbound(id) ON DELETE SET NULL,
    prompt_id uuid REFERENCES sms_auto_sends(id) ON DELETE SET NULL,
    prompt_sent_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, phone_number)
);

-- How and when each keyword sign-up agreed: the exact disclosure text sent, the keyword version,
-- both numbers and the provider's message ids.
CREATE TABLE sms_consents (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    method text NOT NULL, -- keyword | double_opt_in
    phone_number text NOT NULL,
    our_number text NOT NULL,
    keyword_id uuid REFERENCES sms_keywords(id) ON DELETE SET NULL,
    keyword text NOT NULL,
    keyword_version integer NOT NULL,
    disclosure text NOT NULL,
    signup_inbound_id uuid REFERENCES sms_inbound(id) ON DELETE SET NULL,
    signup_provider_message_id text,
    confirm_inbound_id uuid REFERENCES sms_inbound(id) ON DELETE SET NULL,
    confirm_provider_message_id text,
    prompt_message_id uuid,
    prompt_provider_message_id text,
    confirmation_message_id uuid NOT NULL,
    confirmation_provider_message_id text,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX sms_consents_profile ON sms_consents (profile_id, created_at DESC);
CREATE UNIQUE INDEX sms_consents_confirmation ON sms_consents (confirmation_message_id);

-- Operator alerts for an opt-out that couldn't be recorded because several accounts claim the
-- number it was sent to: at most one per number per day (Pacific), enforced by this key.
CREATE TABLE sms_claim_conflict_alerts (
    our_number text NOT NULL,
    day date NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (our_number, day)
);

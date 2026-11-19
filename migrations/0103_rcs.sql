-- RCS: texts upgraded to rich messages from a Telnyx RCS agent, with SMS for everyone else
-- (see src/features/rcs and docs/RCS.md). All new tables; nothing existing changes.

-- One RCS agent per account. Off until a Check confirms the agent with Telnyx.
CREATE TABLE rcs_settings (
    account_id           uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    agent_id             text,
    -- From the last successful Check: the agent's name and the messaging profile Telnyx keeps it on.
    agent_name           text,
    messaging_profile_id text,
    -- The agent id the last successful Check confirmed (a changed agent id needs a new Check).
    checked_agent_id     text,
    -- Which Telnyx credentials that Check used (a short hash, never the key): sends from other
    -- credentials (the account switched texting providers) don't use RCS until it's checked again.
    checked_key          text,
    checked_at           timestamptz,
    -- Why the last Check failed (our own short wording, never Telnyx's text).
    check_problem        text,
    enabled              boolean NOT NULL DEFAULT false,
    updated_at           timestamptz NOT NULL DEFAULT now()
);

-- An agent belongs to one account: the Check proves it (the agent's webhook URL carries this
-- account's private webhook address), and no second account can confirm the same agent.
CREATE UNIQUE INDEX rcs_settings_agent_uq ON rcs_settings (checked_agent_id) WHERE checked_agent_id IS NOT NULL;

-- What Telnyx said a number can receive from an agent, kept for a bounded time.
CREATE TABLE rcs_capabilities (
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    agent_id     text NOT NULL,
    phone_number text NOT NULL,
    capable      boolean NOT NULL,
    features     text[] NOT NULL DEFAULT '{}',
    checked_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, agent_id, phone_number)
);
CREATE INDEX rcs_capabilities_checked_idx ON rcs_capabilities (checked_at);

-- Each text that went to Telnyx as RCS, with exactly what was claimed (so a fallback sends the
-- same text, to the same number, from the same number), and which route it ended on.
CREATE TABLE rcs_messages (
    message_id      uuid PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    agent_id        text NOT NULL,
    to_number       text NOT NULL,
    from_number     text NOT NULL,
    sms_body        text NOT NULL,
    payload         jsonb NOT NULL,
    -- The marketing rules it was sent under (the fallback's last check applies the same ones).
    strict          boolean NOT NULL DEFAULT false,
    is_test         boolean NOT NULL DEFAULT false,
    -- sending → sent → delivered → read; sent → fallback_queued → fallback_sending → fallback_sent;
    -- failed (nothing delivered), unknown (Telnyx didn't say), fallback_skipped.
    state           text NOT NULL DEFAULT 'sending',
    -- rcs until a fallback text goes out, then sms.
    route           text NOT NULL DEFAULT 'rcs',
    rcs_provider_id text,
    sms_provider_id text,
    rcs_charge_micros bigint NOT NULL DEFAULT 0,
    fallback_reason text,
    sent_at         timestamptz,
    delivered_at    timestamptz,
    read_at         timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX rcs_messages_provider_idx ON rcs_messages (rcs_provider_id) WHERE rcs_provider_id IS NOT NULL;
CREATE INDEX rcs_messages_account_idx ON rcs_messages (account_id, created_at DESC);

-- Campaign texts whose numbers' RCS support is being looked up in bulk ahead of the send.
CREATE TABLE rcs_prefetches (
    campaign_id uuid PRIMARY KEY REFERENCES campaigns(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    done_at     timestamptz
);

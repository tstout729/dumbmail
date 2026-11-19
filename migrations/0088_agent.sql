-- AI shopping agent: answers shoppers in the customer hub, in a chat bubble on the store's site and,
-- when the store turns it on, as the first answer to inbound texts (see src/features/agent,
-- docs/AGENT.md). All new tables; nothing existing changes.

-- One row per account that saved agent settings. No row means everything is off.
CREATE TABLE agent_settings (
    account_id  uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    -- The checked shape is in agent::settings::Settings.
    config      jsonb NOT NULL DEFAULT '{}',
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Pages from the store's own website the agent may answer from. Text only, re-read weekly.
CREATE TABLE agent_pages (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    url           text NOT NULL,
    title         text,
    body          text NOT NULL DEFAULT '',
    -- pending | ready | failed
    status        text NOT NULL DEFAULT 'pending',
    error         text,
    fetched_at    timestamptz,
    next_fetch_at timestamptz NOT NULL DEFAULT now(),
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, url)
);
CREATE INDEX agent_pages_due_idx ON agent_pages (next_fetch_at);

-- Conversations. `profile_id` is set only for a verified shopper (signed in to the hub, or texting
-- from their own number); anonymous chats have none. The system prompt and tool list are fixed
-- when a conversation starts and `history` is only ever appended to (the API checks that earlier
-- turns are unchanged). Deleted 90 days after the last message, and with the person.
CREATE TABLE agent_conversations (
    id               uuid PRIMARY KEY,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id       uuid REFERENCES profiles(id) ON DELETE CASCADE,
    -- hub | site | sms | test
    place            text NOT NULL,
    verified         boolean NOT NULL DEFAULT false,
    -- A hash of the browser's conversation token (hub and site chats).
    token_hash       text UNIQUE,
    sms_conversation_id uuid REFERENCES sms_conversations(id) ON DELETE CASCADE,
    system_prompt    text NOT NULL,
    tools            jsonb NOT NULL DEFAULT '[]',
    history          jsonb NOT NULL DEFAULT '[]',
    turns            integer NOT NULL DEFAULT 0,
    tokens           integer NOT NULL DEFAULT 0,
    -- open | handed_off | ended
    status           text NOT NULL DEFAULT 'open',
    handoff_reason   text,
    handoff_summary  text,
    handed_off_at    timestamptz,
    -- One turn at a time: set while a turn runs (a crashed turn frees itself when it passes).
    busy_until       timestamptz,
    ip_hash          text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    last_message_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX agent_conversations_recent_idx ON agent_conversations (account_id, last_message_at DESC, id DESC);
CREATE INDEX agent_conversations_profile_idx ON agent_conversations (profile_id) WHERE profile_id IS NOT NULL;
CREATE INDEX agent_conversations_sms_idx ON agent_conversations (sms_conversation_id, last_message_at DESC) WHERE sms_conversation_id IS NOT NULL;
CREATE INDEX agent_conversations_age_idx ON agent_conversations (last_message_at);

-- What was said, for the team to read (shopper | agent | note).
CREATE TABLE agent_messages (
    id               uuid PRIMARY KEY,
    conversation_id  uuid NOT NULL REFERENCES agent_conversations(id) ON DELETE CASCADE,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    role             text NOT NULL,
    body             text NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX agent_messages_conversation_idx ON agent_messages (conversation_id, created_at, id);

-- Conversations started per account per month (the account's time zone), counted by the write
-- that starts one, so the monthly cap holds across processes.
CREATE TABLE agent_usage (
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    month          text NOT NULL,
    conversations  integer NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, month)
);

-- Conversations started per visitor (a keyed hash of the IP) per hour, across stores.
CREATE TABLE agent_visitor_usage (
    ip_hash        text NOT NULL,
    hour           timestamptz NOT NULL,
    conversations  integer NOT NULL DEFAULT 0,
    PRIMARY KEY (ip_hash, hour)
);

-- Inbound texts the agent took (at most once each, whatever retries happen).
CREATE TABLE agent_sms_claims (
    inbound_id       uuid PRIMARY KEY REFERENCES sms_inbound(id) ON DELETE CASCADE,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    conversation_id  uuid REFERENCES agent_conversations(id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now()
);

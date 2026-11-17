-- Two-way texting: a conversation per person who texted in, every inbound text, MMS media we
-- copied, and the text of each SMS as it went out (campaigns, flows, replies), so a thread shows
-- the whole history.

CREATE TABLE sms_conversations (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'open', -- open | closed
    unread_count integer NOT NULL DEFAULT 0,
    last_message_at timestamptz NOT NULL DEFAULT now(),
    last_inbound_at timestamptz,
    last_direction text NOT NULL DEFAULT 'inbound', -- inbound | outbound
    last_preview text NOT NULL DEFAULT '',
    closed_at timestamptz,
    -- Automatic replies: at most one default/away reply per person every 12 hours, and at most 5
    -- automatic texts of any kind per person per hour, so two bots can't text each other forever.
    last_default_reply_at timestamptz,
    auto_window_start timestamptz,
    auto_window_count integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (profile_id)
);

CREATE INDEX sms_conversations_recent ON sms_conversations (account_id, last_message_at DESC, id DESC);
CREATE INDEX sms_conversations_unread ON sms_conversations (account_id) WHERE unread_count > 0;

CREATE TABLE sms_inbound (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    conversation_id uuid NOT NULL REFERENCES sms_conversations(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    provider text NOT NULL, -- telnyx | twilio
    provider_message_id text,
    from_number text NOT NULL,
    to_number text NOT NULL,
    body text NOT NULL DEFAULT '',
    -- message | stop | start | help | keyword | confirm (a "Y" that finished a double opt-in)
    kind text NOT NULL DEFAULT 'message',
    keyword text,
    -- [{url, content_type, media_id?}]; media_id is set once the file is copied into sms_media.
    media jsonb NOT NULL DEFAULT '[]',
    received_at timestamptz NOT NULL DEFAULT now()
);

-- A provider retrying its webhook must not store (or act on) the same text twice.
CREATE UNIQUE INDEX sms_inbound_provider_message ON sms_inbound (account_id, provider, provider_message_id) WHERE provider_message_id IS NOT NULL;
CREATE INDEX sms_inbound_profile ON sms_inbound (profile_id, received_at DESC);
CREATE INDEX sms_inbound_account ON sms_inbound (account_id, received_at DESC);

-- Images people texted us, copied from the provider (its links expire). Served only to signed-in users.
CREATE TABLE sms_media (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    inbound_id uuid NOT NULL REFERENCES sms_inbound(id) ON DELETE CASCADE,
    content_type text NOT NULL,
    size integer NOT NULL,
    bytes bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX sms_media_inbound ON sms_media (inbound_id);

CREATE TABLE sms_sent_texts (
    message_id uuid PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    body text NOT NULL,
    media_url text,
    kind text NOT NULL DEFAULT 'marketing', -- marketing | reply | auto_reply
    sent_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- WhatsApp messages: what each outgoing message sends (fixed before the hand-off, so a retry sends
-- the same thing) and its WhatsApp status, the conversations in the inbox, and every inbound message.

CREATE TABLE whatsapp_sent (
    message_id     uuid PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- marketing (campaigns and flows) | reply (typed by a team member) | keyword_reply | confirmation
    kind           text NOT NULL DEFAULT 'marketing',
    sent_by        uuid REFERENCES users(id) ON DELETE SET NULL,
    -- The sender it was prepared for (`whatsapp_settings.sender_id`): another sender never sends it.
    sender_id      uuid,
    -- The number the final claim handed it to (what the worker sent to).
    sent_to        text,
    -- Credit held at the claim (when a price is set), settled once Meta says it was sent, released
    -- if it fails.
    reserved_micros bigint NOT NULL DEFAULT 0,
    charged_at     timestamptz,
    -- Meta's pacing answer to the send (accepted | held_for_quality_assessment | paused).
    pacing         text,
    template_id    uuid REFERENCES whatsapp_templates(id) ON DELETE SET NULL,
    template_name  text,
    category       text,
    -- What the conversation view shows (the template with its variables filled, or the text).
    body           text NOT NULL DEFAULT '',
    -- The Cloud API request body, without the recipient.
    payload        jsonb NOT NULL,
    -- Set in the final claim, right before the provider call. A row with this set and no wamid
    -- was handed over without an answer: it's never sent again.
    handed_at      timestamptz,
    attempts       integer NOT NULL DEFAULT 0,
    wamid          text,
    -- sent | delivered | read | failed, from Meta's status webhooks (they only move forward).
    wa_status      text,
    wa_status_at   timestamptz,
    error_code     integer,
    pricing_category text,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX whatsapp_sent_wamid ON whatsapp_sent (wamid) WHERE wamid IS NOT NULL;
CREATE INDEX whatsapp_sent_account ON whatsapp_sent (account_id, created_at DESC);

CREATE TABLE whatsapp_conversations (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id      uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    phone_number    text NOT NULL,
    name            text,
    status          text NOT NULL DEFAULT 'open', -- open | closed
    unread_count    integer NOT NULL DEFAULT 0,
    last_message_at timestamptz NOT NULL DEFAULT now(),
    last_inbound_at timestamptz,
    last_direction  text NOT NULL DEFAULT 'inbound',
    last_preview    text NOT NULL DEFAULT '',
    closed_at       timestamptz,
    -- Automatic answers (keyword replies) per person per hour, so two bots can't loop forever.
    auto_window_start timestamptz,
    auto_window_count integer NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, phone_number)
);
CREATE INDEX whatsapp_conversations_recent ON whatsapp_conversations (account_id, last_message_at DESC, id DESC);
CREATE INDEX whatsapp_conversations_profile ON whatsapp_conversations (profile_id);

CREATE TABLE whatsapp_inbound (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    conversation_id uuid NOT NULL REFERENCES whatsapp_conversations(id) ON DELETE CASCADE,
    profile_id      uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    wamid           text NOT NULL,
    from_number     text NOT NULL,
    -- text | button | interactive | image | ... (Meta's message type)
    msg_type        text NOT NULL,
    body            text NOT NULL DEFAULT '',
    -- message | stop | start | keyword | blocked (a message while unsubscribed does nothing)
    kind            text NOT NULL DEFAULT 'message',
    keyword         text,
    -- The message it replied to (a template's button), when Meta said.
    context_wamid   text,
    received_at     timestamptz NOT NULL,
    processed_at    timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, wamid)
);
CREATE INDEX whatsapp_inbound_conversation ON whatsapp_inbound (conversation_id, received_at DESC);
CREATE INDEX whatsapp_inbound_profile ON whatsapp_inbound (profile_id, received_at DESC);

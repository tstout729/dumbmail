-- Shopping agent: first answers to inbound WhatsApp messages (see src/features/agent/whatsapp.rs).
-- Additive: a nullable column on the agent's own (new) table, and a new table.

ALTER TABLE agent_conversations ADD COLUMN whatsapp_conversation_id uuid REFERENCES whatsapp_conversations(id) ON DELETE CASCADE;
CREATE INDEX agent_conversations_whatsapp_idx ON agent_conversations (whatsapp_conversation_id, last_message_at DESC) WHERE whatsapp_conversation_id IS NOT NULL;

-- Inbound WhatsApp messages the agent took (at most once each, whatever retries happen).
CREATE TABLE agent_whatsapp_claims (
    inbound_id       uuid PRIMARY KEY REFERENCES whatsapp_inbound(id) ON DELETE CASCADE,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    conversation_id  uuid REFERENCES agent_conversations(id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now()
);

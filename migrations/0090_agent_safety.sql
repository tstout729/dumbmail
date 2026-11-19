-- Shopping agent safety (see src/features/agent): what each conversation's order access rests on,
-- who owns the running turn, the conversation fee held until an answer is delivered, and which
-- outgoing message carries which conversation's answer. Additive: new nullable or defaulted
-- columns on the agent's own table, a new table, and plain indexes on small new tables.

-- The proof that allowed order access: {"kind": "hub", "session": .., "generation": ..},
-- {"kind": "number", "number": ..}, {"kind": "sample"}, or null (no orders).
ALTER TABLE agent_conversations ADD COLUMN proof jsonb;
-- The running turn's owner (with busy_until): only the owner's writes and unlock succeed.
ALTER TABLE agent_conversations ADD COLUMN turn_owner uuid;
-- none | reserved | captured | released. Reserved at admission, captured once when an answer is
-- delivered, released when nothing went out.
ALTER TABLE agent_conversations ADD COLUMN fee_state text NOT NULL DEFAULT 'none';
ALTER TABLE agent_conversations ADD COLUMN fee_micros bigint NOT NULL DEFAULT 0;
ALTER TABLE agent_conversations ADD COLUMN fee_reserved_at timestamptz;
CREATE INDEX agent_conversations_held_idx ON agent_conversations (account_id) WHERE fee_state = 'reserved';

-- An outgoing text or WhatsApp message that carries an agent answer.
CREATE TABLE agent_deliveries (
    message_id       uuid PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id  uuid NOT NULL REFERENCES agent_conversations(id) ON DELETE CASCADE,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX agent_deliveries_conversation_idx ON agent_deliveries (conversation_id);

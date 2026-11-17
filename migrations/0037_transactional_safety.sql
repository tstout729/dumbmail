-- Transactional email: off until the owner turns it on, each attempt claimed once, charged once, and
-- no variables or rendered bodies kept in the clear. See src/features/transactional.

-- Off by default. enabled_at: the owner turned it on and accepted what it may be used for.
-- full_mode: the operator allowed inline content and the normal caps on the hosted service
-- (self-hosted installs always have it). Without it, only saved transactional templates go out.
ALTER TABLE transactional_settings ADD COLUMN enabled_at timestamptz;
ALTER TABLE transactional_settings ADD COLUMN enabled_by uuid REFERENCES users(id) ON DELETE SET NULL;
ALTER TABLE transactional_settings ADD COLUMN full_mode boolean NOT NULL DEFAULT false;

-- Send status is now queued (waiting for an attempt) | sending (one attempt holds it) | sent | skipped
-- | failed | unknown (an attempt was cut off after the hand-off to the provider may have happened, so it
-- is never sent again). `request` is no longer filled in ('{}'): the log keeps no variables or content.
ALTER TABLE transactional_sends ADD COLUMN attempts integer NOT NULL DEFAULT 0;
ALTER TABLE transactional_sends ADD COLUMN claimed_at timestamptz;
-- What a retry needs to render the email again, encrypted with SECRET_KEY. Cleared as soon as the
-- send is final, and after 24 hours at most.
ALTER TABLE transactional_sends ADD COLUMN payload_enc text;
-- Set in the same transaction as the charge, so a send is billed at most once.
ALTER TABLE transactional_sends ADD COLUMN charged_at timestamptz;
ALTER TABLE transactional_sends ADD COLUMN cost_micros bigint;
CREATE INDEX transactional_sends_claimed_idx ON transactional_sends (claimed_at) WHERE status = 'sending';
CREATE INDEX transactional_sends_payload_idx ON transactional_sends (created_at) WHERE payload_enc IS NOT NULL;

-- Hard bounces and complaints by address, so a mailbox stays suppressed even if the profile that
-- bounced later changed its email.
CREATE INDEX messages_bad_address_idx ON messages (account_id, to_address) WHERE status IN ('bounced', 'complained');

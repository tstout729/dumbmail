-- Bounces and complaints are matched to their send by the provider's message id, so they still count
-- after the message itself was deleted along with its profile.
ALTER TABLE email_sends ADD COLUMN provider_message_id text;
UPDATE email_sends s SET provider_message_id = m.provider_message_id FROM messages m WHERE m.id = s.message_id;
CREATE INDEX email_sends_provider_idx ON email_sends (provider_message_id) WHERE provider_message_id IS NOT NULL;

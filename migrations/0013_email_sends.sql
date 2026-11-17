-- One row per real email the provider accepted, with its bounce/complaint outcome, for the automatic
-- sending pause. Kept apart from messages on purpose: deleting a profile deletes its messages, and must
-- not erase the sends and bounces the pause is measured on. Rows are pruned after 30 days.
CREATE TABLE email_sends (
    message_id  uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    campaign_id uuid,
    sent_at     timestamptz NOT NULL DEFAULT now(),
    hard_bounce boolean NOT NULL DEFAULT false,
    complaint   boolean NOT NULL DEFAULT false
);
CREATE INDEX email_sends_account_idx ON email_sends (account_id, sent_at);
CREATE INDEX email_sends_campaign_idx ON email_sends (campaign_id) WHERE campaign_id IS NOT NULL;

-- The last 7 days of email already sent, so accounts are judged on recent history from the start.
INSERT INTO email_sends (message_id, account_id, campaign_id, sent_at, hard_bounce, complaint)
SELECT id, account_id, campaign_id, COALESCE(sent_at, created_at),
       status = 'bounced' AND bounce_type = 'Permanent' AND error IS DISTINCT FROM 'OnAccountSuppressionList',
       status = 'complained'
FROM messages
WHERE channel = 'email' AND NOT is_test AND status IN ('sent','delivered','bounced','complained')
  AND COALESCE(sent_at, created_at) > now() - interval '7 days';

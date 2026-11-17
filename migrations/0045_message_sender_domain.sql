-- The domain each email was sent from, for deliverability by sending domain. Older messages stay NULL.
ALTER TABLE messages ADD COLUMN from_domain text;

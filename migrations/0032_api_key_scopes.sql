-- API key limits. Existing keys keep full access (a constant default: no rewrite).
ALTER TABLE api_keys ADD COLUMN scopes text[] NOT NULL DEFAULT '{full}';
ALTER TABLE api_keys ADD COLUMN expires_at timestamptz;
ALTER TABLE api_keys ADD COLUMN created_by uuid;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_created_by_fk FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL NOT VALID;
-- The key that made this key, if a key made it. A key stops working when any key above it is
-- revoked or expired, and never expires later than the key that made it.
ALTER TABLE api_keys ADD COLUMN parent_key_id uuid;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_parent_fk FOREIGN KEY (parent_key_id) REFERENCES api_keys(id) ON DELETE CASCADE NOT VALID;
CREATE INDEX api_keys_parent_idx ON api_keys (parent_key_id) WHERE parent_key_id IS NOT NULL;

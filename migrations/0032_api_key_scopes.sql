-- API key limits. Existing keys keep full access.
ALTER TABLE api_keys ADD COLUMN scopes text[] NOT NULL DEFAULT '{full}';
ALTER TABLE api_keys ADD COLUMN expires_at timestamptz;
ALTER TABLE api_keys ADD COLUMN created_by uuid REFERENCES users(id) ON DELETE SET NULL;

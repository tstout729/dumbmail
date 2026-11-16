-- "Test your setup" sessions: the website script reports what it detects on the customer's site.
CREATE TABLE site_tests (
    code text PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    url text,
    checks jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    last_seen_at timestamptz,
    last_url text
);
CREATE INDEX site_tests_account_idx ON site_tests (account_id, created_at DESC);

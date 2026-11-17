-- Team members and invites. Existing memberships keep their 'owner' role.
CREATE INDEX IF NOT EXISTS memberships_account_idx ON memberships (account_id);

-- Which account a login opens by default (the one used last).
ALTER TABLE users ADD COLUMN last_account_id uuid REFERENCES accounts(id) ON DELETE SET NULL;

CREATE TABLE team_invites (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email        text NOT NULL,
    role         text NOT NULL,
    token_hash   text NOT NULL UNIQUE,
    invited_by   uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    sent_at      timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    accepted_at  timestamptz,
    accepted_by  uuid REFERENCES users(id) ON DELETE SET NULL,
    revoked_at   timestamptz
);
CREATE INDEX team_invites_account_idx ON team_invites (account_id, created_at DESC);
-- One open invite per address per account.
CREATE UNIQUE INDEX team_invites_open_idx ON team_invites (account_id, email) WHERE accepted_at IS NULL AND revoked_at IS NULL;

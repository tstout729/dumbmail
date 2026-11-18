-- Team members and invites. Existing memberships keep their 'owner' role.
-- Additive only, and nothing here rewrites or long-locks users, sessions, api_keys or accounts:
-- new columns are nullable or have a constant default, and foreign keys on existing tables are
-- added NOT VALID (checked for new rows; old rows are all NULL anyway).
CREATE INDEX IF NOT EXISTS memberships_account_idx ON memberships (account_id);

-- Which account a login opens by default (the one used last).
ALTER TABLE users ADD COLUMN last_account_id uuid;
ALTER TABLE users ADD CONSTRAINT users_last_account_fk FOREIGN KEY (last_account_id) REFERENCES accounts(id) ON DELETE SET NULL NOT VALID;

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

-- Someone taken off an account. Single sign-on won't add them back on its own (just-in-time
-- joining refuses a match on the email or the provider identity) until an owner or admin invites
-- them again and they accept.
CREATE TABLE team_removals (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email      text NOT NULL,
    issuer     text,
    subject    text,
    removed_by uuid REFERENCES users(id) ON DELETE SET NULL,
    removed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX team_removals_email_idx ON team_removals (account_id, email);
CREATE INDEX team_removals_identity_idx ON team_removals (account_id, issuer, subject);

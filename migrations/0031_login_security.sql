-- Sessions: an id to list and revoke them, how they signed in, two-factor state and the device.
ALTER TABLE sessions ADD COLUMN id uuid NOT NULL DEFAULT gen_random_uuid();
ALTER TABLE sessions ADD COLUMN auth_method text NOT NULL DEFAULT 'password';
ALTER TABLE sessions ADD COLUMN mfa_ok boolean NOT NULL DEFAULT false;
ALTER TABLE sessions ADD COLUMN sso_account_id uuid REFERENCES accounts(id) ON DELETE CASCADE;
ALTER TABLE sessions ADD COLUMN last_seen_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE sessions ADD COLUMN ip text;
ALTER TABLE sessions ADD COLUMN user_agent text;
CREATE UNIQUE INDEX sessions_id_idx ON sessions (id);

-- Two-factor login (TOTP). Secrets are encrypted with SECRET_KEY.
ALTER TABLE users ADD COLUMN totp_secret_enc text;
ALTER TABLE users ADD COLUMN totp_pending_enc text;
ALTER TABLE users ADD COLUMN totp_enabled_at timestamptz;
-- The last 30-second step a code was accepted for, so a code can't be used twice.
ALTER TABLE users ADD COLUMN totp_last_step bigint;

CREATE TABLE recovery_codes (
    id         uuid PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash  text NOT NULL,
    used_at    timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX recovery_codes_user_idx ON recovery_codes (user_id);

CREATE TABLE password_resets (
    token_hash text PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL,
    used_at    timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX password_resets_user_idx ON password_resets (user_id);

-- "Require two-factor login for all members".
ALTER TABLE accounts ADD COLUMN require_two_factor boolean NOT NULL DEFAULT false;

-- Fixed-window counters for sign-in, code and reset attempts. Bucket names are hashed.
CREATE TABLE rate_limits (
    bucket       text NOT NULL,
    window_start timestamptz NOT NULL,
    hits         integer NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, window_start)
);
CREATE INDEX rate_limits_window_idx ON rate_limits (window_start);

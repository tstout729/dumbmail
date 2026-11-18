-- Customer hub: a shopper account area on the store's site (see src/features/hub, docs/HUB.md).

-- One row per account that has opened the hub settings. No row means the hub is off.
CREATE TABLE hub_settings (
    account_id  uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    enabled     boolean NOT NULL DEFAULT false,
    -- off | button (a floating "My account" button) | link (only from the store's own link or button)
    launcher    text NOT NULL DEFAULT 'off',
    -- The checked shape is in hub::settings::Settings.
    config      jsonb NOT NULL DEFAULT '{}',
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Sign-in codes. Only a hash of the code is kept, and the address only as a hash too (so a row
-- says nothing about who asked). A row is written for every request, known address or not, so the
-- rate limits count both the same way. profile_id is set when the address belongs to someone.
CREATE TABLE hub_codes (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id  uuid REFERENCES profiles(id) ON DELETE SET NULL,
    email_hash  text NOT NULL,
    ip_hash     text,
    code_hash   text,
    attempts    integer NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    -- Set once the email is handed to the provider's queue (claimed before sending).
    sent_at     timestamptz,
    used_at     timestamptz
);
CREATE INDEX hub_codes_email_idx ON hub_codes(account_id, email_hash, created_at DESC);
CREATE INDEX hub_codes_account_idx ON hub_codes(account_id, created_at DESC);
CREATE INDEX hub_codes_ip_idx ON hub_codes(ip_hash, created_at DESC) WHERE ip_hash IS NOT NULL;
CREATE INDEX hub_codes_profile_idx ON hub_codes(profile_id) WHERE profile_id IS NOT NULL;

-- Signed-in shoppers. The cookie holds a random token; only its hash is kept. A session is good
-- only while the person's email is still the one they signed in with, so it ends when the email
-- changes; erasing the person deletes it.
CREATE TABLE hub_sessions (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    token_hash   text NOT NULL UNIQUE,
    email        text NOT NULL,
    -- code | link
    method       text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    revoked_at   timestamptz
);
CREATE INDEX hub_sessions_profile_idx ON hub_sessions(profile_id);
CREATE INDEX hub_sessions_expires_idx ON hub_sessions(expires_at);

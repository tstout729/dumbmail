-- Single sign-on with OpenID Connect, one identity provider per account.
CREATE TABLE account_sso (
    account_id           uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    enabled              boolean NOT NULL DEFAULT true,
    issuer               text NOT NULL,
    client_id            text NOT NULL,
    client_secret_enc    text NOT NULL,
    secret_hint          text NOT NULL,
    domains              text[] NOT NULL DEFAULT '{}',
    enforce              boolean NOT NULL DEFAULT false,
    auto_join            boolean NOT NULL DEFAULT true,
    default_role         text NOT NULL DEFAULT 'analyst',
    -- The owner says this provider only issues verified emails (for providers that never send
    -- the email_verified claim). Off: a missing claim counts as unverified.
    verified_emails_only boolean NOT NULL DEFAULT false,
    -- The owner says the provider makes everyone use two-step verification, so its sign-ins
    -- satisfy "require two-factor" even when the ID token's amr claim doesn't say so.
    idp_enforces_mfa     boolean NOT NULL DEFAULT false,
    -- Goes up whenever a setting that decides who gets in changes (provider, client, secret,
    -- domains, on/off, the verified-email and two-step promises). A sign-in started under an older
    -- revision gets no session, and sessions it started stop working.
    revision             integer NOT NULL DEFAULT 1,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);

-- A person at an account's identity provider (issuer + subject) is one DumbMail login there, and
-- a login has at most one subject per provider in each account: it's never moved to another
-- subject by an email match. Bindings belong to the account, so resetting one in one account
-- leaves another account that uses the same provider alone.
CREATE TABLE sso_identities (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    issuer     text NOT NULL,
    subject    text NOT NULL,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, issuer, subject),
    UNIQUE (account_id, issuer, user_id)
);
CREATE INDEX sso_identities_user_idx ON sso_identities (user_id);

-- The provider settings revision a single sign-on session was started under.
ALTER TABLE sessions ADD COLUMN sso_revision integer;

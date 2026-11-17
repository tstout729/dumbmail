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
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);

-- A person at an identity provider (issuer + subject) is one DumbMail login, and a login has at
-- most one subject per provider: it's never moved to another subject by an email match.
CREATE TABLE sso_identities (
    issuer     text NOT NULL,
    subject    text NOT NULL,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issuer, subject),
    UNIQUE (issuer, user_id)
);
CREATE INDEX sso_identities_user_idx ON sso_identities (user_id);

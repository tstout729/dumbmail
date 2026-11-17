-- Single sign-on with OpenID Connect, one identity provider per account.
CREATE TABLE account_sso (
    account_id        uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    enabled           boolean NOT NULL DEFAULT true,
    issuer            text NOT NULL,
    client_id         text NOT NULL,
    client_secret_enc text NOT NULL,
    secret_hint       text NOT NULL,
    domains           text[] NOT NULL DEFAULT '{}',
    enforce           boolean NOT NULL DEFAULT false,
    auto_join         boolean NOT NULL DEFAULT true,
    default_role      text NOT NULL DEFAULT 'analyst',
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- A person at an identity provider (issuer + subject) is one DumbMail login.
CREATE TABLE sso_identities (
    issuer     text NOT NULL,
    subject    text NOT NULL,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issuer, subject)
);
CREATE INDEX sso_identities_user_idx ON sso_identities (user_id);

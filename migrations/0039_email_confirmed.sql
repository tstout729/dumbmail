-- Whether a login's email address is proven to belong to it, so "Continue with Google" only links
-- to a password login its address's owner already had (see src/api/google.rs).
ALTER TABLE users ADD COLUMN email_confirmed_at timestamptz;
-- Google proved these addresses when they were linked.
UPDATE users SET email_confirmed_at = now() WHERE google_sub IS NOT NULL AND email_confirmed_at IS NULL;
-- A login given up by "start fresh": it can't sign in and its address is freed.
ALTER TABLE users ADD COLUMN disabled_at timestamptz;

-- An account left with no way in ("start fresh"): it sends and runs nothing.
ALTER TABLE accounts ADD COLUMN closed_at timestamptz;

-- Links that confirm a login's email address. Only hashes are kept; each works once.
CREATE TABLE email_confirmations (
    token_hash text PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email      text NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at    timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX email_confirmations_user_idx ON email_confirmations (user_id);

-- A Google sign-in that may link Google to an existing login, waiting either for the person to
-- choose what happens to an unconfirmed password login (the browser holds only a random token in
-- an HttpOnly cookie, whose hash is kept here), or for the sign-in that links it to finish its
-- two-factor step (`session_id`). Everything needed to finish comes from this row, including the
-- login's state when it was checked; the link is made only if that state still holds. Single use.
CREATE TABLE google_link_pending (
    token_hash         text PRIMARY KEY,
    user_id            uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email              text NOT NULL,
    google_sub         text NOT NULL,
    google_name        text,
    google_given_name  text,
    -- Google is the authority for this address (Gmail, or a Workspace domain that matches it).
    google_authoritative boolean NOT NULL DEFAULT false,
    next               text NOT NULL DEFAULT '/',
    session_id         uuid UNIQUE,
    credential_version integer,
    was_confirmed      boolean,
    expires_at         timestamptz NOT NULL,
    used_at            timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX google_link_pending_user_idx ON google_link_pending (user_id);

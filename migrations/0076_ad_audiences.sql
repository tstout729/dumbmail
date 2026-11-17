-- Ad audience sync: lists and segments kept in step with Meta, Google Ads and TikTok audiences.

-- One sign-in per ad platform per account. Tokens are encrypted with crate::crypto and never
-- returned. Disconnecting clears the tokens but keeps the row, so syncs pick up again on reconnect.
CREATE TABLE ad_connections (
    id                 uuid PRIMARY KEY,
    account_id         uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    platform           text NOT NULL CHECK (platform IN ('meta', 'google', 'tiktok')),
    status             text NOT NULL DEFAULT 'connected', -- connected | needs_reconnect | disconnected
    connected_as       text,
    access_token_enc   text,
    refresh_token_enc  text,
    token_hint         text,
    token_expires_at   timestamptz,
    error              text,
    connected_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    connected_at       timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, platform)
);

-- Pending OAuth sign-ins: the random state (stored hashed) ties the platform's callback to the
-- account and person who started it. Single use; rows older than 15 minutes are ignored and pruned.
CREATE TABLE ad_oauth_states (
    state_hash   text PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id      uuid REFERENCES users(id) ON DELETE CASCADE,
    platform     text NOT NULL,
    verifier_enc text,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE ad_audience_syncs (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    connection_id       uuid NOT NULL REFERENCES ad_connections(id) ON DELETE CASCADE,
    platform            text NOT NULL,
    ad_account_id       text NOT NULL,
    ad_account_name     text,
    -- Google Ads manager account the signed-in Google user reaches the ad account through.
    login_account_id    text,
    -- No foreign key: a deleted list or segment shows as an error instead of silently removing the sync.
    source_type         text NOT NULL CHECK (source_type IN ('list', 'segment')),
    source_id           uuid NOT NULL,
    name                text NOT NULL,
    external_audience_id text,
    include_unsubscribed boolean NOT NULL DEFAULT false,
    match_fields        text NOT NULL DEFAULT 'contact' CHECK (match_fields IN ('contact', 'contact_and_location')),
    interval_hours      integer NOT NULL DEFAULT 6 CHECK (interval_hours BETWEEN 1 AND 168),
    status              text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'paused')),
    terms_accepted_at   timestamptz,
    current_run_id      uuid,
    next_run_at         timestamptz NOT NULL DEFAULT now(),
    last_run_at         timestamptz,
    last_success_at     timestamptz,
    last_error          text,
    members_sent        integer NOT NULL DEFAULT 0,
    last_added          integer NOT NULL DEFAULT 0,
    last_removed        integer NOT NULL DEFAULT 0,
    last_invalid        integer NOT NULL DEFAULT 0,
    platform_size       bigint,
    platform_size_upper bigint,
    match_rate          real,
    platform_status     text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ad_audience_syncs_account_idx ON ad_audience_syncs(account_id, created_at DESC);
CREATE INDEX ad_audience_syncs_due_idx ON ad_audience_syncs(next_run_at) WHERE status = 'active' AND current_run_id IS NULL;

-- What DumbMail last sent to the platform, per person: the hashed identifiers exactly as sent (so a
-- removal sends the same values) and a digest of them (so a changed email is a remove plus an add).
-- profile_id has no foreign key on purpose: a deleted profile must stay here until the next sync
-- removes it from the platform audience.
CREATE TABLE ad_audience_members (
    sync_id    uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL,
    digest     text NOT NULL,
    ids        jsonb NOT NULL,
    sent_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (sync_id, profile_id)
);

-- Who should be in the audience, built at the start of each run and diffed against ad_audience_members.
CREATE TABLE ad_audience_desired (
    sync_id    uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL,
    digest     text NOT NULL,
    ids        jsonb NOT NULL,
    PRIMARY KEY (sync_id, profile_id)
);

CREATE TABLE ad_audience_runs (
    id            uuid PRIMARY KEY,
    sync_id       uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    trigger       text NOT NULL, -- created | scheduled | manual
    status        text NOT NULL DEFAULT 'running', -- running | done | failed
    phase         text NOT NULL DEFAULT 'scan', -- scan | remove | add | finish
    cursor_id     uuid,
    -- Platform batching state (Meta upload sessions) and retry counts.
    state         jsonb NOT NULL DEFAULT '{}',
    added         integer NOT NULL DEFAULT 0,
    removed       integer NOT NULL DEFAULT 0,
    invalid       integer NOT NULL DEFAULT 0,
    skipped       integer NOT NULL DEFAULT 0,
    error         text,
    started_at    timestamptz NOT NULL DEFAULT now(),
    heartbeat_at  timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz
);
CREATE INDEX ad_audience_runs_sync_idx ON ad_audience_runs(sync_id, started_at DESC);

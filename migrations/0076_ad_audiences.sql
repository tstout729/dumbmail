-- Ad audience sync: lists and segments kept in step with Meta, Google Ads and TikTok audiences.

-- One sign-in per ad platform per account. Tokens are encrypted with crate::crypto and never
-- returned. `generation` goes up on every connect and disconnect; a token refresh or revoke that
-- started under an older generation can't write. Disconnecting clears the tokens but keeps the row,
-- so syncs pick up again on reconnect.
CREATE TABLE ad_connections (
    id                 uuid PRIMARY KEY,
    account_id         uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    platform           text NOT NULL CHECK (platform IN ('meta', 'google', 'tiktok')),
    status             text NOT NULL DEFAULT 'connected' CHECK (status IN ('connected', 'needs_reconnect', 'disconnected')),
    generation         bigint NOT NULL DEFAULT 1,
    connected_as       text,
    access_token_enc   text,
    refresh_token_enc  text,
    token_expires_at   timestamptz,
    -- A code from a fixed list (see store::ConnError), never a platform's own words.
    error_code         text,
    -- Whether the platform confirmed the last disconnect's revoke: confirmed | not_confirmed.
    revoke_status      text,
    connected_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    connected_at       timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, platform)
);

-- Pending OAuth sign-ins: the random state (stored hashed) ties the platform's callback to the
-- account, person and platform that started it. Consumed atomically for exactly that triple;
-- rows older than 15 minutes don't work and are pruned.
CREATE TABLE ad_oauth_states (
    state_hash   text PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    platform     text NOT NULL,
    verifier_enc text,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Account-wide settings. Google syncs need an owner or admin to confirm that the account's
-- marketing consent covers advertising use (Google's EU user consent policy); who and when is kept.
CREATE TABLE ad_audience_settings (
    account_id        uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    google_consent_at timestamptz,
    google_consent_by uuid REFERENCES users(id) ON DELETE SET NULL,
    google_consent_by_email text,
    updated_at        timestamptz NOT NULL DEFAULT now()
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
    -- Set only from the platform's answer to DumbMail's own create call (audience_created_at is when).
    -- DumbMail never adopts an audience by name or description.
    external_audience_id text,
    audience_created_at timestamptz,
    match_fields        text NOT NULL DEFAULT 'contact' CHECK (match_fields IN ('contact', 'contact_and_location')),
    interval_hours      integer NOT NULL DEFAULT 6 CHECK (interval_hours BETWEEN 1 AND 168),
    status              text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'paused')),
    -- Why it's paused: user | google_consent | tiktok_minimum | audience_missing | create_unclear
    pause_reason        text,
    terms_accepted_at   timestamptz,
    current_run_id      uuid,
    -- One worker (or one API delete) at a time: every write a run makes checks this token.
    lease_token         uuid,
    lease_until         timestamptz,
    next_run_at         timestamptz NOT NULL DEFAULT now(),
    last_run_at         timestamptz,
    last_success_at     timestamptz,
    -- A code from a fixed list (see sync::ErrorCode); the words are DumbMail's.
    last_error_code     text,
    -- Identifiers (hashed emails and phone numbers) by what the platform said about them.
    sent_count          integer NOT NULL DEFAULT 0,
    confirmed_count     integer NOT NULL DEFAULT 0,
    pending_removal_count integer NOT NULL DEFAULT 0,
    last_added          integer NOT NULL DEFAULT 0,
    last_removed        integer NOT NULL DEFAULT 0,
    platform_size       bigint,
    platform_size_upper bigint,
    match_rate          real,
    platform_status     text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ad_audience_syncs_account_idx ON ad_audience_syncs(account_id, created_at DESC);
-- One sync per list or segment and ad account.
CREATE UNIQUE INDEX ad_audience_syncs_one_idx ON ad_audience_syncs(account_id, platform, ad_account_id, source_type, source_id);
CREATE INDEX ad_audience_syncs_due_idx ON ad_audience_syncs(next_run_at) WHERE current_run_id IS NULL;

-- Every hashed identifier DumbMail may have put in the platform audience, one row per identifier
-- (not per profile: two profiles that normalize to the same hash share one row). There is no
-- profile id here on purpose: when a profile is deleted or merged, the hash stays until the
-- platform confirms its removal.
--   adding      in a batch that was sent; the platform hasn't confirmed it yet
--   confirmed   the platform confirmed it
--   unconfirmed sent before, never confirmed (rejected, or an answer DumbMail couldn't read):
--               treated as possibly in the audience, so it's removed if it stops qualifying
--   removing    must be removed; stays until the platform confirms the removal
CREATE TABLE ad_audience_members (
    sync_id      uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    key_type     text NOT NULL CHECK (key_type IN ('email', 'phone')),
    key_hash     text NOT NULL,
    state        text NOT NULL CHECK (state IN ('adding', 'confirmed', 'unconfirmed', 'removing')),
    batch_id     uuid,
    -- The run that last sent it, so a refused identifier isn't retried within the same run.
    tried_run_id uuid,
    first_sent_at timestamptz NOT NULL DEFAULT now(),
    confirmed_at timestamptz,
    PRIMARY KEY (sync_id, key_type, key_hash)
);
CREATE INDEX ad_audience_members_state_idx ON ad_audience_members(sync_id, state);
CREATE INDEX ad_audience_members_batch_idx ON ad_audience_members(batch_id) WHERE batch_id IS NOT NULL;

-- Who qualifies, built at the start of each run: one row per profile and identifier. Rows with
-- veto=true come from profiles anywhere in the account that opted out, unsubscribed or are
-- suppressed: any identifier they map to is removed and never added. src_check is a hash of the
-- stored email or phone, so a batch can see at dispatch that the profile's address changed.
CREATE TABLE ad_audience_desired (
    sync_id    uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL,
    key_type   text NOT NULL,
    key_hash   text NOT NULL,
    src_check  text NOT NULL,
    veto       boolean NOT NULL,
    -- Meta only: hashed name and location fields sent alongside the identifier when adding.
    hints      jsonb,
    -- Already on the platform, or taken by a batch this run: not a candidate for adding.
    claimed    boolean NOT NULL DEFAULT false,
    PRIMARY KEY (sync_id, profile_id, key_type)
);
CREATE INDEX ad_audience_desired_key_idx ON ad_audience_desired(sync_id, key_type, key_hash);
CREATE INDEX ad_audience_desired_open_idx ON ad_audience_desired(sync_id, key_type, key_hash) WHERE NOT claimed AND NOT veto;

-- Exactly what was (or is about to be) sent: written before the platform call, settled after it in
-- one transaction with the identifiers' new states. An unsettled batch is resent with the same id
-- and rows; a Meta resend uses a fresh session id, never a reused sequence.
--   pending   written; not sent yet, or sent with no readable answer
--   accepted  Google took it and is processing it (platform_ref is the requestId)
--   confirmed / partial / rejected / abandoned (gave up after repeated unreadable answers)
CREATE TABLE ad_audience_batches (
    id              uuid PRIMARY KEY,
    sync_id         uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    run_id          uuid NOT NULL,
    op              text NOT NULL CHECK (op IN ('add', 'remove')),
    -- [{"t": "email", "h": "<sha256>", "x": {hints}}], in the order sent.
    rows            jsonb NOT NULL,
    row_count       integer NOT NULL,
    status          text NOT NULL DEFAULT 'pending',
    attempts        integer NOT NULL DEFAULT 0,
    meta_session_id bigint,
    platform_ref    text,
    result_code     text,
    confirmed_count integer NOT NULL DEFAULT 0,
    rejected_count  integer NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    sent_at         timestamptz,
    settled_at      timestamptz
);
CREATE INDEX ad_audience_batches_open_idx ON ad_audience_batches(sync_id, created_at) WHERE status IN ('pending', 'accepted');
CREATE INDEX ad_audience_batches_sync_idx ON ad_audience_batches(sync_id, created_at DESC);

CREATE TABLE ad_audience_runs (
    id            uuid PRIMARY KEY,
    sync_id       uuid NOT NULL REFERENCES ad_audience_syncs(id) ON DELETE CASCADE,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    trigger       text NOT NULL, -- created | scheduled | manual
    -- full: add and remove. remove_only: a paused sync still removes people who left or opted out.
    -- veto_only: the list or segment is gone; only people who opted out are removed.
    mode          text NOT NULL DEFAULT 'full',
    status        text NOT NULL DEFAULT 'running', -- running | done | failed | stopped
    phase         text NOT NULL DEFAULT 'scan', -- scan | veto | plan | tiktok_min | remove | add | sweep | finish
    cursor_id     uuid,
    sweeps        integer NOT NULL DEFAULT 0,
    retries       integer NOT NULL DEFAULT 0,
    added         integer NOT NULL DEFAULT 0,
    removed       integer NOT NULL DEFAULT 0,
    rejected      integer NOT NULL DEFAULT 0,
    skipped       integer NOT NULL DEFAULT 0,
    error_code    text,
    started_at    timestamptz NOT NULL DEFAULT now(),
    heartbeat_at  timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz
);
CREATE INDEX ad_audience_runs_sync_idx ON ad_audience_runs(sync_id, started_at DESC);
CREATE INDEX ad_audience_runs_running_idx ON ad_audience_runs(heartbeat_at) WHERE status = 'running';

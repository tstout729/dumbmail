-- Profile tools: internal notes, possible-duplicate signals, bulk actions, downloads, privacy
-- requests, and the minimal suppression record kept after a person is deleted.

CREATE TABLE profile_notes (
    id             uuid PRIMARY KEY,
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id     uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    author_user_id uuid,
    author_label   text NOT NULL,
    body           text NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX profile_notes_profile_idx ON profile_notes (profile_id, created_at DESC);

-- An upsert named identifiers that belong to two different profiles (say, an email on one and a
-- phone number on another). The upsert itself is unchanged; the duplicates finder lists these.
CREATE TABLE identity_conflicts (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_a   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    profile_b   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    identifiers jsonb NOT NULL DEFAULT '{}',
    times_seen  integer NOT NULL DEFAULT 1,
    first_seen  timestamptz NOT NULL DEFAULT now(),
    last_seen   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, profile_a, profile_b),
    CHECK (profile_a < profile_b)
);
CREATE INDEX identity_conflicts_b_idx ON identity_conflicts (profile_b);

-- A random value per account, mixed into the one-way codes below.
CREATE TABLE privacy_salts (
    account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    salt       text NOT NULL
);

-- What's left of a deleted person who couldn't be contacted: a salted one-way code of their
-- address and the reason. If the address comes back (an import, an API call) it stays suppressed.
CREATE TABLE suppression_tombstones (
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    channel      text NOT NULL CHECK (channel IN ('email', 'sms')),
    address_hash text NOT NULL,
    -- unsubscribed | hard_bounce | complaint | manual
    reason       text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, channel, address_hash, reason)
);

-- Privacy requests (export or delete one person's data): who asked, when and what was done. The
-- person is kept only as a one-way code, never their data.
CREATE TABLE privacy_requests (
    id                uuid PRIMARY KEY,
    account_id        uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind              text NOT NULL CHECK (kind IN ('export', 'delete')),
    subject_hash      text NOT NULL,
    -- email | phone_number
    identified_by     text NOT NULL,
    -- exports are built in the background: building | ready | failed (deletes are done at once)
    status            text NOT NULL DEFAULT 'done',
    requested_by_user uuid,
    via               text NOT NULL,
    profiles_found    integer NOT NULL DEFAULT 0,
    result            jsonb NOT NULL DEFAULT '{}',
    created_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX privacy_requests_account_idx ON privacy_requests (account_id, created_at DESC);

-- Files people download after signing in (privacy exports, bulk exports). Deleted when they expire.
-- The content is in people_file_chunks, written in bounded pieces by a background job; a file can be
-- downloaded only once it's ready.
CREATE TABLE people_files (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- privacy_export (one person) | bulk_export (many people)
    kind         text NOT NULL,
    filename     text NOT NULL,
    content_type text NOT NULL,
    ready        boolean NOT NULL DEFAULT false,
    -- privacy exports point at their request
    request_id   uuid,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL
);
CREATE INDEX people_files_account_idx ON people_files (account_id, created_at DESC);
CREATE INDEX people_files_expiry_idx ON people_files (expires_at);
CREATE INDEX people_files_request_idx ON people_files (request_id) WHERE request_id IS NOT NULL;

CREATE TABLE people_file_chunks (
    file_id uuid NOT NULL REFERENCES people_files(id) ON DELETE CASCADE,
    seq     integer NOT NULL,
    bytes   bytea NOT NULL,
    PRIMARY KEY (file_id, seq)
);

-- Whose data a personal export holds. Merges move these to the kept profile, so erasing someone
-- finds every export of them, whichever profile it was made from. (No foreign key: erasure removes
-- the files itself before the profile goes.)
CREATE TABLE people_file_subjects (
    file_id    uuid NOT NULL REFERENCES people_files(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL,
    PRIMARY KEY (file_id, profile_id)
);
CREATE INDEX people_file_subjects_profile_idx ON people_file_subjects (profile_id);

-- When someone in the account was last erased. Export jobs compare it with when they started and
-- throw away a file that could still hold the erased person.
CREATE TABLE privacy_erasures (
    account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    erased_at  timestamptz NOT NULL
);

-- Bulk actions on profiles. Previewing saves the exact set of profiles counted ('preview', kept 15
-- minutes); confirming that set, with the same action and settings, queues it, and a background job
-- works through exactly those people in batches.
CREATE TABLE bulk_actions (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    action       text NOT NULL,
    params       jsonb NOT NULL DEFAULT '{}',
    selection    jsonb NOT NULL DEFAULT '{}',
    status       text NOT NULL DEFAULT 'queued' CHECK (status IN ('preview', 'queued', 'running', 'done', 'failed')),
    total        integer NOT NULL DEFAULT 0,
    processed    integer NOT NULL DEFAULT 0,
    succeeded    integer NOT NULL DEFAULT 0,
    skipped      integer NOT NULL DEFAULT 0,
    cursor_seq   integer NOT NULL DEFAULT 0,
    result       jsonb NOT NULL DEFAULT '{}',
    error        text,
    file_id      uuid,
    created_by   uuid,
    via          text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- an unconfirmed preview stops being usable at this time
    expires_at   timestamptz,
    started_at   timestamptz,
    finished_at  timestamptz
);
CREATE INDEX bulk_actions_account_idx ON bulk_actions (account_id, created_at DESC);
-- One bulk action per account at a time.
CREATE UNIQUE INDEX bulk_actions_one_active_uq ON bulk_actions (account_id) WHERE status IN ('queued', 'running');

CREATE TABLE bulk_action_targets (
    bulk_id    uuid NOT NULL REFERENCES bulk_actions(id) ON DELETE CASCADE,
    seq        integer NOT NULL,
    profile_id uuid,
    address    text,
    PRIMARY KEY (bulk_id, seq)
);
CREATE INDEX bulk_action_targets_profile_idx ON bulk_action_targets (profile_id) WHERE profile_id IS NOT NULL;

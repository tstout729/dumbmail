-- Custom objects: an account's own record types (reservations, subscriptions, pets ...), with
-- typed fields, linked to one or more profiles, and every change kept for a while.

CREATE TABLE object_types (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Lower-case key used by the API, flows and segments, e.g. 'reservation'.
    key         text NOT NULL,
    name        text NOT NULL,
    plural      text NOT NULL,
    -- [{key, label, type, options?, currency?, sensitive?}]; type is text | number | boolean |
    -- date | datetime | enum | list | money.
    fields      jsonb NOT NULL DEFAULT '[]',
    title_field text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, key)
);

CREATE TABLE objects (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    type_id     uuid NOT NULL REFERENCES object_types(id) ON DELETE CASCADE,
    -- The id the object has in the customer's own system (a reservation id).
    external_id text NOT NULL,
    fields      jsonb NOT NULL DEFAULT '{}',
    -- The title field's value, kept for lists and search.
    title       text,
    -- Who last wrote it: api | mcp | integration:<id>.
    source      text,
    version     bigint NOT NULL DEFAULT 1,
    -- The newest `as_of` (the change's time or version in the customer's system) written; an
    -- upsert older than it is ignored, so a late, older message never undoes a newer one.
    as_of       text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (type_id, external_id)
);
CREATE INDEX objects_type_updated_idx ON objects(type_id, updated_at DESC, id);
CREATE INDEX objects_type_id_idx ON objects(type_id, id);
CREATE INDEX objects_account_idx ON objects(account_id);

-- Which people an object belongs to, and how (guest, owner ...; '' when not said).
CREATE TABLE object_links (
    object_id    uuid NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
    profile_id   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    relationship text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (object_id, profile_id, relationship)
);
CREATE INDEX object_links_profile_idx ON object_links(profile_id, object_id);

-- What changed, when: field names with their values before and after (only the changed ones).
-- Kept per object (the newest 100) and for a year; trimmed in batches by a background job.
CREATE TABLE object_changes (
    id         uuid PRIMARY KEY,
    object_id  uuid NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind       text NOT NULL, -- created | updated
    changed    text[] NOT NULL DEFAULT '{}',
    before     jsonb NOT NULL DEFAULT '{}',
    after      jsonb NOT NULL DEFAULT '{}',
    source     text,
    version    bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX object_changes_object_idx ON object_changes(object_id, created_at DESC);
CREATE INDEX object_changes_created_idx ON object_changes(created_at);

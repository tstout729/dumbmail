-- Klaviyo import, second pass: marketing holds, the consent the import itself set, one runner per
-- import, the window while event history is written, list memberships the import must not re-add, and
-- imported items that need a person's review before they can send.

-- Held people are left out of campaigns and get only service-message flows until they opt in. The custom
-- integrations work adds the same column (and the enforcement); IF NOT EXISTS lets either land first.
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS marketing_hold boolean NOT NULL DEFAULT false;

ALTER TABLE klaviyo_imports
    -- The run that owns this import right now, so two workers never run it at once.
    ADD COLUMN IF NOT EXISTS lease_owner uuid,
    ADD COLUMN IF NOT EXISTS lease_until timestamptz,
    -- True while a run is writing event history for the account (set with every page written, cleared
    -- when the run pauses, finishes, fails or is cancelled).
    ADD COLUMN IF NOT EXISTS history_open boolean NOT NULL DEFAULT false,
    -- The comparison report, kept for a few minutes so the status page doesn't rebuild it on every poll.
    ADD COLUMN IF NOT EXISTS report_cache jsonb,
    ADD COLUMN IF NOT EXISTS report_cached_at timestamptz;

-- Set on an account's segments while past activity is being imported: a refresh takes new members in
-- without starting segment-triggered flows (imported orders aren't new orders). Cleared once each
-- segment has been refreshed again after the import stops writing history.
ALTER TABLE segments ADD COLUMN IF NOT EXISTS absorb_entries boolean NOT NULL DEFAULT false;

-- What the import itself last set, per profile: the email address and phone number it subscribed
-- (Klaviyo's opt-in). A later run lets Klaviyo govern those subscriptions; any other subscription is
-- the person's own DumbMail opt-in, which the import leaves alone.
CREATE TABLE IF NOT EXISTS klaviyo_consent_writes (
    profile_id  uuid PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email_sub   text,
    sms_sub     text,
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- One row per (list, profile) the import has dealt with, or that left an imported list in DumbMail.
-- `left` rows are people who left the list: in DumbMail (preference page, dashboard, API) they're never
-- added back by an import; a leave Klaviyo recorded is lifted when Klaviyo has them on the list again.
-- `scan` is the full membership scan that last saw the person in Klaviyo, so removals can be reconciled
-- once a scan completes; snapshot segments stage their new member set here before it's swapped in.
CREATE TABLE IF NOT EXISTS klaviyo_memberships (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    list_id     uuid NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    -- member | left
    state       text NOT NULL,
    -- klaviyo (removed in Klaviyo) | klaviyo_list_unsubscribe | dumbmail
    left_via    text,
    scan        bigint NOT NULL DEFAULT 0,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (list_id, profile_id)
);
CREATE INDEX IF NOT EXISTS klaviyo_memberships_scan_idx ON klaviyo_memberships (list_id, scan);

-- Imported items the importer couldn't translate with certainty: the notes, until a person clears them.
-- Edits to the item don't touch it. Nullable, no default: adding it rewrites nothing.
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS import_review jsonb;
ALTER TABLE flows ADD COLUMN IF NOT EXISTS import_review jsonb;
ALTER TABLE templates ADD COLUMN IF NOT EXISTS import_review jsonb;
ALTER TABLE forms ADD COLUMN IF NOT EXISTS import_review jsonb;

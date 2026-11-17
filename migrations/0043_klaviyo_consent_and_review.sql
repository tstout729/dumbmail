-- Klaviyo import, second pass: marketing holds, one runner per import, history windows for segment-triggered flows, list memberships the import must never re-add, and
-- imported items that need a person's review before they can send.

-- Held people are left out of campaigns and get only service-message flows until they opt in. The custom
-- integrations work adds the same column (and the enforcement); IF NOT EXISTS lets either land first.
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS marketing_hold boolean NOT NULL DEFAULT false;

ALTER TABLE klaviyo_imports
    -- The run that owns this import right now, so two workers never run it at once.
    ADD COLUMN IF NOT EXISTS lease_owner uuid,
    ADD COLUMN IF NOT EXISTS lease_until timestamptz,
    -- The last time event history writing started and finished. While it runs (or was interrupted), and
    -- for each segment's first refresh after it, segment-triggered flows don't start for new members.
    ADD COLUMN IF NOT EXISTS history_started_at timestamptz,
    ADD COLUMN IF NOT EXISTS history_done_at timestamptz,
    -- The comparison report, kept for a few minutes so the status page doesn't rebuild it on every poll.
    ADD COLUMN IF NOT EXISTS report_cache jsonb,
    ADD COLUMN IF NOT EXISTS report_cached_at timestamptz;

-- One row per (list, profile) the import has dealt with. `left` rows are people who left the list, in
-- Klaviyo or in DumbMail (preference page, removed by hand): the import never adds them back.
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

-- Imported items the importer couldn't translate with certainty. Until a person clears the review, a
-- campaign can't send and a flow can't go live.
CREATE TABLE IF NOT EXISTS klaviyo_reviews (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- campaign | flow | template | form
    kind        text NOT NULL,
    local_id    uuid NOT NULL,
    notes       jsonb NOT NULL DEFAULT '[]',
    created_at  timestamptz NOT NULL DEFAULT now(),
    cleared_at  timestamptz,
    PRIMARY KEY (kind, local_id)
);
CREATE INDEX IF NOT EXISTS klaviyo_reviews_account_idx ON klaviyo_reviews (account_id) WHERE cleared_at IS NULL;

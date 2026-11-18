-- What each imported record was in Klaviyo, so a second run updates instead of duplicating, and so the
-- comparison report can count both sides. `detail` keeps Klaviyo's own view (a profile's consent and
-- suppressions exactly as Klaviyo reported them, a campaign's headline numbers, a segment's translation).
CREATE TABLE klaviyo_refs (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- profile | list | segment | metric | template | campaign | campaign_message | flow | form
    kind        text NOT NULL,
    klaviyo_id  text NOT NULL,
    -- The DumbMail row (profile, list, segment, metric, template, campaign, flow or form).
    local_id    uuid,
    name        text,
    detail      jsonb NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, kind, klaviyo_id)
);
CREATE INDEX klaviyo_refs_local_idx ON klaviyo_refs (account_id, kind, local_id);

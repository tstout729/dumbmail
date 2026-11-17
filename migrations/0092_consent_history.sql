-- Consent history: one row for every change to a person's email or SMS consent, their suppression,
-- or their "do not sell or share" choice, with how it happened. Rows are only ever added: an update
-- is refused by the trigger below, and the app has no delete path (rows go only when the profile is
-- deleted). See src/features/people/consent.rs.
CREATE TABLE consent_events (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id    uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    channel       text NOT NULL CHECK (channel IN ('email', 'sms', 'ads')),
    field         text NOT NULL CHECK (field IN ('consent', 'suppression', 'opt_out')),
    old_value     text,
    new_value     text,
    -- form, import, api, mcp, dashboard, unsubscribe_link, preference_page, one_click, sms_keyword,
    -- double_opt_in, hard_bounce, spam_complaint, integration, klaviyo_import, merge, bulk_action,
    -- suppression_list, privacy_request, deleted_profile, gpc, other
    method        text NOT NULL,
    -- How the change arrived: dashboard, api, mcp, public (forms, links, replies, webhooks) or system.
    via           text,
    -- The email address or phone number the change was about, as it was then.
    address       text,
    -- The team member, for changes made in the dashboard (no foreign key: the row outlives the member).
    actor_user_id uuid,
    form_id       uuid,
    message_id    uuid,
    ip            text,
    user_agent    text,
    detail        jsonb NOT NULL DEFAULT '{}',
    occurred_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX consent_events_profile_idx ON consent_events (profile_id, occurred_at DESC);
CREATE INDEX consent_events_account_idx ON consent_events (account_id, occurred_at DESC);

CREATE FUNCTION consent_events_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'consent history rows cannot be changed';
END $$;
CREATE TRIGGER consent_events_no_update BEFORE UPDATE ON consent_events
    FOR EACH ROW EXECUTE FUNCTION consent_events_append_only();

-- When recording started, so the profile page can say "no history before <date>".
CREATE TABLE people_settings (
    key   text PRIMARY KEY,
    value timestamptz NOT NULL
);
INSERT INTO people_settings (key, value) VALUES ('consent_history_started_at', now());

-- Texting suppression set by the business (email's is email_suppressed_reason), and the
-- per-person "do not sell or share" flag other features can read.
ALTER TABLE profiles
    ADD COLUMN sms_suppressed_reason text,
    ADD COLUMN ads_opt_out boolean NOT NULL DEFAULT false,
    ADD COLUMN ads_opt_out_at timestamptz,
    ADD COLUMN ads_opt_out_source text;

-- Campaign sends: the audience is fixed when the send starts, release times are computed from a
-- persisted start, and SMS notes when the phone's and the profile's time zones disagree.
-- See src/campaigns.rs and src/features/sendtime.

-- Set when everyone who gets the campaign has a message row (or, during an A/B test, a holdout row),
-- so a retried send job adds nobody who joined the audience later.
ALTER TABLE campaigns ADD COLUMN audience_fixed_at timestamptz;

-- A/B tests: the rest of the audience, fixed when the test starts. They get the winner (if they can
-- still be messaged then), even if they leave the list in between.
CREATE TABLE campaign_holdouts (
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    PRIMARY KEY (campaign_id, profile_id)
);

-- When the A/B winner was picked: the winner send's release times are computed from it.
ALTER TABLE campaigns ADD COLUMN ab_winner_at timestamptz;

-- When this part of the send was asked for (the send, or the A/B winner). Release times and gradual
-- positions are computed from it, so a restarted planning step gives everyone the same time.
ALTER TABLE messages ADD COLUMN planned_from timestamptz;
-- How it was sent, when that's worth telling (SMS: the phone's and profile's time zones disagreed).
ALTER TABLE messages ADD COLUMN send_note text;

-- SMS when a profile's time zone and its phone number's never share sending hours:
-- 'phone' uses the phone number's hours; 'skip' doesn't text them.
ALTER TABLE send_settings ADD COLUMN sms_zone_conflict text NOT NULL DEFAULT 'phone';

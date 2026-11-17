-- Campaign send options (each person's local time, best time per person, gradual sends, smart sending)
-- and per-account SMS quiet hours. See src/features/sendtime.

-- How a campaign goes out; NULL sends to everyone at once, as before (see sendtime::SendOptions).
ALTER TABLE campaigns ADD COLUMN send_options jsonb;

-- When a queued campaign message is released, and the recipient's time zone it was planned in.
-- NULL send_after: send as soon as its batch runs.
ALTER TABLE messages ADD COLUMN send_after timestamptz;
ALTER TABLE messages ADD COLUMN send_tz text;

-- SMS sending window in each recipient's time zone, as minutes after local midnight. Defaults are
-- 9:00 to 20:00; accounts can only narrow it, never beyond 8:00 to 21:00.
CREATE TABLE send_settings (
    account_id       uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    sms_start_minute integer NOT NULL DEFAULT 540,
    sms_end_minute   integer NOT NULL DEFAULT 1200,
    updated_at       timestamptz NOT NULL DEFAULT now()
);

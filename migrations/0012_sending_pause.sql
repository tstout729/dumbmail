-- Automatic email pause: all customers share one Amazon SES account, so an account whose recent email
-- bounces or draws complaints at a dangerous rate stops sending email until an operator lifts it.
ALTER TABLE accounts
    ADD COLUMN sending_paused_at     timestamptz,
    ADD COLUMN sending_paused_reason text,
    -- Only messages created after this count toward the bounce/complaint rates (reset on unpause).
    ADD COLUMN sending_window_start  timestamptz;

-- SES bounce type (Permanent | Transient | Undetermined); only permanent (hard) bounces count.
ALTER TABLE messages ADD COLUMN bounce_type text;

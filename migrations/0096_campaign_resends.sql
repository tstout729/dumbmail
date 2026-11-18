-- "Resend to people who didn't open (or click)": a resend is an ordinary campaign whose audience is
-- the original's recipients who haven't opened (or clicked) it, fixed when the resend starts sending.
--
-- One row per resend slot of an original (at most two). The row is the record that a campaign is a
-- resend. Deleting a draft resend leaves its row with no campaign and no start, which frees the slot;
-- a resend that started sending keeps its slot for good (and can't be deleted).
CREATE TABLE campaign_resends (
    id           uuid PRIMARY KEY,
    campaign_id  uuid UNIQUE REFERENCES campaigns(id) ON DELETE SET NULL,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    original_id  uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    -- no_open: nobody who opened or clicked; no_click: nobody who clicked.
    target       text NOT NULL CHECK (target IN ('no_open', 'no_click')),
    number       smallint NOT NULL CHECK (number IN (1, 2)),
    -- Set in the transaction that fixes the resend's audience.
    started_at   timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (original_id, number)
);
CREATE INDEX campaign_resends_account_idx ON campaign_resends(account_id);

-- When a campaign finished sending: its last message was handled (after an A/B test, when the
-- winner send finished). Nullable with no default: no table rewrite.
ALTER TABLE campaigns ADD COLUMN finished_at timestamptz;

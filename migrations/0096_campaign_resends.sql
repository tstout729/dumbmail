-- "Resend to people who didn't open (or click)": a resend is an ordinary campaign whose audience is
-- the original's recipients who haven't opened (or clicked) it, fixed when the resend starts sending.
CREATE TABLE campaign_resends (
    campaign_id  uuid PRIMARY KEY REFERENCES campaigns(id) ON DELETE CASCADE,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    original_id  uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    -- no_open: nobody who opened or clicked; no_click: nobody who clicked.
    target       text NOT NULL CHECK (target IN ('no_open', 'no_click')),
    -- At most two resends per original, enforced by the unique index below.
    number       smallint NOT NULL CHECK (number IN (1, 2)),
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (original_id, number)
);
CREATE INDEX campaign_resends_account_idx ON campaign_resends(account_id);

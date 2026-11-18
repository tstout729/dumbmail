-- Short links in text messages (see features::short_links and docs/SHORT_LINKS.md).

-- On for every account, new and existing: it only makes texts shorter. A constant default, so adding
-- it doesn't rewrite the table.
ALTER TABLE accounts ADD COLUMN sms_short_links boolean NOT NULL DEFAULT true;

-- A text's short reference: 6 letters and digits that, with the account, find the message a short
-- link came from (the link's tag tells two messages with the same reference apart). Nullable with no
-- default, so adding it doesn't rewrite the table. Its index is built in the background after the
-- server starts (features::short_links::INDEXES); short links aren't used until it's in place.
ALTER TABLE messages ADD COLUMN short_ref text;

-- One row per destination an account's texts link to, not per recipient.
CREATE TABLE short_link_targets (
    -- 6 letters and digits, random, the start of every short link to this destination.
    code         text PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Where the link goes: the link as the sender wrote it, with any link tags added.
    url          text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- Last put in a text (updated at most once a day); unused for 400 days, the row is deleted.
    last_used_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, url)
);
CREATE INDEX short_link_targets_last_used_idx ON short_link_targets (last_used_at);

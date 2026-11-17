-- One sync per feed at a time, and no results from a feed setup that has since changed.
ALTER TABLE catalog_feeds
    ADD COLUMN config_version   integer NOT NULL DEFAULT 0,
    ADD COLUMN sync_queued_at   timestamptz,
    ADD COLUMN sync_full        boolean NOT NULL DEFAULT false,
    ADD COLUMN sync_lease       uuid,
    ADD COLUMN sync_lease_until timestamptz,
    -- The address the saved ETag / Last-Modified came from (after redirects).
    ADD COLUMN validator_url    text;

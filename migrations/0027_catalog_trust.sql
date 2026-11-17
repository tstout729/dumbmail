-- Telling server-confirmed data apart from what a browser said.

-- Events recorded through the public browser endpoint (/client/events). New events default to false
-- (recorded by the server, the API or MCP); events from before this column are NULL (unknown).
ALTER TABLE events ADD COLUMN from_browser boolean;
ALTER TABLE events ALTER COLUMN from_browser SET DEFAULT false;

-- Products learned from browser events start unverified: they show only to the person whose own
-- event carried them until the server reads the product page and confirms them.
ALTER TABLE catalog_products
    ADD COLUMN verified        boolean NOT NULL DEFAULT true,
    ADD COLUMN verified_at     timestamptz,
    ADD COLUMN verify_after    timestamptz,
    ADD COLUMN verify_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN verify_error    text,
    -- What the store itself says about publication; `published` is this and still being in its feed.
    ADD COLUMN source_published boolean NOT NULL DEFAULT true,
    ADD COLUMN missing_since   timestamptz;
-- Learned products saved before this migration (only on unreleased builds) were never checked.
UPDATE catalog_products SET verified = false, verify_after = now() WHERE source = 'events';
UPDATE catalog_products SET source_published = published;
CREATE INDEX catalog_products_verify_idx ON catalog_products(verify_after) WHERE verify_after IS NOT NULL;

-- Orders recorded by a trusted source (the API, MCP or an integration). Only these count toward
-- best sellers and bought together.
ALTER TABLE catalog_interactions ADD COLUMN trusted boolean NOT NULL DEFAULT false;
CREATE INDEX catalog_interactions_trusted_idx ON catalog_interactions(account_id, occurred_at DESC) WHERE kind = 'ordered' AND trusted;

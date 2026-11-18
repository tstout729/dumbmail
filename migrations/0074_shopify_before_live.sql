-- Shopify app, fixes before it is switched on. Additive only, on the app's own tables.

-- When an order or checkout last moved a customer's link to its own contact: a customer snapshot
-- no newer than this can't move the link back (it still applies its consent, which has its own times).
ALTER TABLE shopify_customers ADD COLUMN linked_at timestamptz;

-- The install (generation) that last wrote each piece of a store's data, so a shop redaction for an
-- old install removes that install's data even after the same store has connected again.
ALTER TABLE shopify_customers ADD COLUMN generation integer;
ALTER TABLE shopify_customer_profiles ADD COLUMN generation integer;
ALTER TABLE shopify_created_profiles ADD COLUMN generation integer;
ALTER TABLE shopify_products ADD COLUMN generation integer;

-- The install a discount code was made for: a code is reused only for that store and install.
ALTER TABLE shopify_discount_codes ADD COLUMN generation integer;

-- People a Shopify history import wrote about. Segment-triggered flows don't start for them when they
-- enter a segment (that entry came from history), checked after each refresh, so it holds whenever
-- the history was written; real entries by anyone else start flows as usual. Cleared once every
-- segment of the account has been refreshed since (see `features::shopify::events`).
CREATE TABLE shopify_history_touched (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    touched_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, profile_id)
);
CREATE INDEX shopify_history_touched_at_idx ON shopify_history_touched(account_id, touched_at);

-- Shopify app, fitted to the platform: the history window (segments absorb entries only while a
-- page of history is being written) and coupons whose codes are made in the store.
-- Additive only, on the app's own tables.

ALTER TABLE shopify_imports ADD COLUMN history_open boolean NOT NULL DEFAULT false;
CREATE INDEX shopify_imports_history_open_idx ON shopify_imports(account_id) WHERE history_open;

-- A coupon whose codes are made in the connected store, and the discount each code gives.
CREATE TABLE shopify_coupon_templates (
    coupon_id   uuid PRIMARY KEY REFERENCES coupons(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    template    jsonb NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX shopify_coupon_templates_account_idx ON shopify_coupon_templates(account_id);

-- The app's metrics are server-only for accounts that connected a store before connecting started
-- marking them (see `features::shopify::SERVER_METRICS`). Small: one row per metric per such account.
INSERT INTO metrics (id, account_id, name, server_only)
SELECT gen_random_uuid(), s.account_id, n, true
FROM shopify_shops s CROSS JOIN unnest(ARRAY['Placed Order', 'Started Checkout', 'Ordered Product', 'Fulfilled Order', 'Fulfilled Partial Order', 'Cancelled Order', 'Refunded Order']) AS n
ON CONFLICT (account_id, name) DO UPDATE SET server_only = true;

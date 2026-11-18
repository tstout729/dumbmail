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

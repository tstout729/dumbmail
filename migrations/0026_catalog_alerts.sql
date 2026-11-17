-- Back-in-stock and price-drop alerts, and the catalog's per-account settings.

CREATE TABLE catalog_settings (
    account_id               uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    learn_from_events        boolean NOT NULL DEFAULT true,
    back_in_stock_enabled    boolean NOT NULL DEFAULT true,
    back_in_stock_limit      integer,               -- most people told per restock; NULL = everyone
    price_drop_enabled       boolean NOT NULL DEFAULT true,
    price_drop_percent       double precision DEFAULT 10,
    price_drop_amount        double precision,
    price_drop_lookback_days integer NOT NULL DEFAULT 14,
    price_drop_cooldown_days integer NOT NULL DEFAULT 30,
    widget                   jsonb NOT NULL DEFAULT '{}',
    updated_at               timestamptz NOT NULL DEFAULT now()
);

-- People waiting for a product (or one variant of it) to come back. notified_at is set once they're told.
CREATE TABLE catalog_stock_subscriptions (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    product_id  uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    variant_id  uuid REFERENCES catalog_variants(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    notified_at timestamptz
);
CREATE UNIQUE INDEX catalog_stock_subscriptions_waiting_uq
    ON catalog_stock_subscriptions(profile_id, product_id, COALESCE(variant_id, '00000000-0000-0000-0000-000000000000'::uuid))
    WHERE notified_at IS NULL;
CREATE INDEX catalog_stock_subscriptions_product_idx ON catalog_stock_subscriptions(product_id, created_at) WHERE notified_at IS NULL;

-- When each person was last told about a price drop on a product (for the cooldown).
CREATE TABLE catalog_price_drop_notices (
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    product_id  uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    notified_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (profile_id, product_id)
);

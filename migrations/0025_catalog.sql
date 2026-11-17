-- Product catalog: products, variants, price and stock history, feeds, and what people did with products.

CREATE TABLE catalog_feeds (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            text NOT NULL,
    url             text NOT NULL,
    format          text NOT NULL,                -- json | google_xml
    field_map       jsonb NOT NULL DEFAULT '{}',  -- our field -> the feed's field
    interval_hours  integer NOT NULL DEFAULT 6,
    enabled         boolean NOT NULL DEFAULT true,
    unpublish_missing boolean NOT NULL DEFAULT true,
    etag            text,
    last_modified   text,
    next_sync_at    timestamptz NOT NULL DEFAULT now(),
    last_synced_at  timestamptz,
    last_status     text,                          -- ok | not_modified | error
    last_error      text,
    last_counts     jsonb NOT NULL DEFAULT '{}',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX catalog_feeds_account_idx ON catalog_feeds(account_id);
CREATE INDEX catalog_feeds_due_idx ON catalog_feeds(next_sync_at) WHERE enabled;

CREATE TABLE catalog_products (
    id               uuid PRIMARY KEY,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    external_id      text NOT NULL,
    title            text NOT NULL,
    description      text,
    url              text,
    image_url        text,
    images           jsonb NOT NULL DEFAULT '[]',
    price            double precision,
    compare_at_price double precision,
    currency         text,
    available        boolean NOT NULL DEFAULT true,
    inventory        integer,                      -- NULL: not tracked
    in_stock         boolean NOT NULL DEFAULT true, -- derived from the variants (or the product) on every write
    categories       text[] NOT NULL DEFAULT '{}',
    tags             text[] NOT NULL DEFAULT '{}',
    brand            text,
    vendor           text,
    custom           jsonb NOT NULL DEFAULT '{}',
    published        boolean NOT NULL DEFAULT true,
    published_at     timestamptz NOT NULL DEFAULT now(),
    source           text NOT NULL DEFAULT 'api',  -- api | csv | feed | events
    feed_id          uuid REFERENCES catalog_feeds(id) ON DELETE SET NULL,
    feed_seen_at     timestamptz,
    content_hash     text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, external_id)
);
CREATE INDEX catalog_products_account_idx ON catalog_products(account_id, published_at DESC);
CREATE INDEX catalog_products_url_idx ON catalog_products(account_id, url) WHERE url IS NOT NULL;
CREATE INDEX catalog_products_categories_idx ON catalog_products USING gin(categories);
CREATE INDEX catalog_products_tags_idx ON catalog_products USING gin(tags);
CREATE INDEX catalog_products_feed_idx ON catalog_products(feed_id) WHERE feed_id IS NOT NULL;

CREATE TABLE catalog_variants (
    id               uuid PRIMARY KEY,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    product_id       uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    external_id      text NOT NULL,
    title            text,
    options          jsonb NOT NULL DEFAULT '{}',
    sku              text,
    price            double precision,
    compare_at_price double precision,
    available        boolean NOT NULL DEFAULT true,
    inventory        integer,
    in_stock         boolean NOT NULL DEFAULT true,
    image_url        text,
    position         integer NOT NULL DEFAULT 0,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (product_id, external_id)
);
CREATE INDEX catalog_variants_external_idx ON catalog_variants(account_id, external_id);
CREATE INDEX catalog_variants_sku_idx ON catalog_variants(account_id, sku) WHERE sku IS NOT NULL;

-- One row each time a product's price or a variant's stock changes (variant_id NULL = the product itself).
CREATE TABLE catalog_price_history (
    id               bigserial PRIMARY KEY,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    product_id       uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    variant_id       uuid REFERENCES catalog_variants(id) ON DELETE CASCADE,
    price            double precision,
    compare_at_price double precision,
    in_stock         boolean NOT NULL,
    inventory        integer,
    recorded_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX catalog_price_history_product_idx ON catalog_price_history(product_id, recorded_at DESC);

-- What people did with products, read from Viewed Product, Added to Cart, Started Checkout and
-- Placed Order events. Feeds recommendations, best sellers and the product segment rules.
CREATE TABLE catalog_interactions (
    id          bigserial PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    product_id  uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    variant_id  uuid REFERENCES catalog_variants(id) ON DELETE SET NULL,
    kind        text NOT NULL,                     -- viewed | carted | checkout | ordered
    quantity    integer NOT NULL DEFAULT 1,
    price       double precision,
    event_id    uuid NOT NULL,
    occurred_at timestamptz NOT NULL,
    UNIQUE (event_id, product_id, kind)
);
CREATE INDEX catalog_interactions_product_idx ON catalog_interactions(product_id, kind, occurred_at DESC);
CREATE INDEX catalog_interactions_profile_idx ON catalog_interactions(profile_id, kind, occurred_at DESC);
CREATE INDEX catalog_interactions_account_idx ON catalog_interactions(account_id, kind, occurred_at DESC);
CREATE INDEX catalog_interactions_event_idx ON catalog_interactions(event_id);

-- How far the event reader has got (one row; the newest event id it has read).
CREATE TABLE catalog_learn_cursor (
    id         integer PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_event uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    updated_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO catalog_learn_cursor (id) VALUES (1);

-- Bought-together and viewed-together counts per product, recomputed when stale.
CREATE TABLE catalog_rec_cache (
    product_id  uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    kind        text NOT NULL,
    items       jsonb NOT NULL DEFAULT '[]',
    computed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, kind)
);

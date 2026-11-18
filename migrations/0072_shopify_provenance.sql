-- Shopify app, second part: install generations, where data came from (for privacy requests),
-- per-address consent times, write-back retries, erasure markers and product scans.
-- Additive only: new tables, and new nullable or constant-default columns on the app's own tables.

-- Each install of the app gets a generation; work queued for one install can't write to another.
ALTER TABLE shopify_shops ADD COLUMN generation integer NOT NULL DEFAULT 0;
-- Webhook topics Shopify refused to subscribe (shown as a problem; setup tries again).
ALTER TABLE shopify_shops ADD COLUMN missing_topics text[] NOT NULL DEFAULT '{}';
ALTER TABLE shopify_shops ADD COLUMN setup_after timestamptz;
-- Set once the store's products have been loaded in full (the first load queues no alerts).
ALTER TABLE shopify_shops ADD COLUMN catalog_loaded_at timestamptz;
-- Disconnect clean-up in the store that Shopify didn't confirm.
ALTER TABLE shopify_shops ADD COLUMN cleanup_error text;
-- Webhook bodies deleted unprocessed after 30 days.
ALTER TABLE shopify_shops ADD COLUMN dropped_payloads integer NOT NULL DEFAULT 0;

-- Every install, kept after the connection is gone or replaced, so privacy requests for a past
-- store still find the account that held its data.
CREATE TABLE shopify_installs (
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    generation     integer NOT NULL,
    shop_domain    text NOT NULL,
    installed_at   timestamptz NOT NULL DEFAULT now(),
    uninstalled_at timestamptz,
    redacted_at    timestamptz,
    PRIMARY KEY (account_id, generation)
);
CREATE INDEX shopify_installs_domain_idx ON shopify_installs(shop_domain);

ALTER TABLE shopify_webhooks ADD COLUMN generation integer;
ALTER TABLE shopify_webhooks ADD COLUMN shop_domain text;
ALTER TABLE shopify_webhooks ADD COLUMN next_at timestamptz;          -- retry after (privacy requests)
ALTER TABLE shopify_webhooks ADD COLUMN claimed_until timestamptz;    -- one processor at a time

ALTER TABLE shopify_imports ADD COLUMN generation integer;
ALTER TABLE shopify_imports ADD COLUMN shop_domain text;

-- Customer links: which store, Shopify's own update time (an older snapshot never overwrites a
-- newer one), deletion in Shopify (the link is kept for privacy requests), and write-back retries.
ALTER TABLE shopify_customers ADD COLUMN shop_domain text;
ALTER TABLE shopify_customers ADD COLUMN shopify_updated_at timestamptz;
ALTER TABLE shopify_customers ADD COLUMN deleted_at timestamptz;
ALTER TABLE shopify_customers ADD COLUMN email_wb_attempts integer NOT NULL DEFAULT 0;
ALTER TABLE shopify_customers ADD COLUMN email_wb_after timestamptz;
ALTER TABLE shopify_customers ADD COLUMN email_wb_error text;
ALTER TABLE shopify_customers ADD COLUMN sms_wb_attempts integer NOT NULL DEFAULT 0;
ALTER TABLE shopify_customers ADD COLUMN sms_wb_after timestamptz;
ALTER TABLE shopify_customers ADD COLUMN sms_wb_error text;

-- Every profile a store's customer touched (the email profile, the SMS number's profile, an order's
-- contact), and profiles the app created. Privacy requests act on these, never on a matching email.
CREATE TABLE shopify_customer_profiles (
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    shop_domain  text NOT NULL,
    customer_id  bigint NOT NULL,                      -- 0: a guest order or checkout
    profile_id   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    PRIMARY KEY (account_id, shop_domain, customer_id, profile_id)
);
CREATE INDEX shopify_customer_profiles_profile_idx ON shopify_customer_profiles(profile_id);
CREATE TABLE shopify_created_profiles (
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    shop_domain  text NOT NULL,
    profile_id   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    PRIMARY KEY (account_id, profile_id)
);

-- The Shopify consent time last applied for each address or number: older reports change nothing.
CREATE TABLE shopify_consent (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    channel     text NOT NULL,                         -- email | sms
    contact     text NOT NULL,                         -- the normalized address or number
    state       text,
    consent_at  timestamptz,
    applied_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, channel, contact)
);

-- Erased by a privacy request: imports and late webhooks skip these instead of bringing them back.
CREATE TABLE shopify_erasures (
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    shop_domain  text NOT NULL,
    kind         text NOT NULL,                        -- customer | order | shop
    shopify_id   bigint NOT NULL DEFAULT 0,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, shop_domain, kind, shopify_id)
);

-- The store each catalog product came from, Shopify's update time, the last full scan that saw it,
-- and deletion in Shopify.
CREATE TABLE shopify_products (
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    product_id          bigint NOT NULL,
    shop_domain         text NOT NULL,
    shopify_updated_at  timestamptz,
    seen_by             uuid,
    deleted_at          timestamptz,
    touched_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, product_id)
);
CREATE INDEX shopify_products_shop_idx ON shopify_products(account_id, shop_domain);

-- Discount codes: the store, and the grant they were made for.
ALTER TABLE shopify_discount_codes ADD COLUMN shop_domain text;
ALTER TABLE shopify_discount_codes ADD COLUMN grant_key text;

-- Data requests name Shopify's customer id, not an email.
ALTER TABLE shopify_data_requests ADD COLUMN shopify_customer_id bigint;
ALTER TABLE shopify_data_requests ADD COLUMN shop_domain text;

-- Shopify app: the connected store, its webhooks, customers, history import and discount codes.
-- All new tables; nothing existing changes.

-- One store per DumbMail account. A store connected to one account can't be connected to another.
CREATE TABLE shopify_shops (
    account_id          uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    shop_domain         text NOT NULL,                  -- example.myshopify.com
    status              text NOT NULL,                  -- connected | uninstalled | disconnected
    scopes              text NOT NULL DEFAULT '',
    access_token_enc    text,                           -- crypto::encrypt; NULL once disconnected
    token_expires_at    timestamptz,
    refresh_token_enc   text,
    refresh_expires_at  timestamptz,
    refreshing_until    timestamptz,                    -- one token refresh at a time
    token_hint          text,
    shop_name           text,
    currency            text,
    primary_url         text,
    shop_timezone       text,
    webhook_ids         text[] NOT NULL DEFAULT '{}',   -- subscriptions this install created
    web_pixel_id        text,
    storefront_error    text,
    connected_by        uuid,
    connected_at        timestamptz NOT NULL DEFAULT now(),
    disconnected_at     timestamptz,
    last_webhook_at     timestamptz,
    last_webhook_topic  text,
    last_order_at       timestamptz,                    -- last order event that came from Shopify's servers
    last_error          text,                           -- plain words, no secrets or vendor bodies
    last_error_at       timestamptz,
    lease_until         timestamptz,                    -- the account's one background worker
    lease_token         uuid,
    work_queued_at      timestamptz,
    reconcile_after     timestamptz NOT NULL DEFAULT now() + interval '1 day',
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX shopify_shops_domain_live_uq ON shopify_shops(shop_domain) WHERE status = 'connected';
CREATE INDEX shopify_shops_domain_idx ON shopify_shops(shop_domain);

-- Install attempts started by a signed-in person (dashboard or MCP), single use, short-lived.
CREATE TABLE shopify_oauth_states (
    state_hash   text PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id      uuid,
    shop_domain  text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX shopify_oauth_states_created_idx ON shopify_oauth_states(created_at);

-- Verified webhook deliveries waiting to be processed. The body is cleared once processed; the row
-- stays a week so a redelivery (same X-Shopify-Webhook-Id) is recognised.
CREATE TABLE shopify_webhooks (
    id           bigserial PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    webhook_id   text NOT NULL,
    topic        text NOT NULL,
    payload      jsonb,
    received_at  timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    error        text,
    done_at      timestamptz,
    UNIQUE (account_id, webhook_id)
);
CREATE INDEX shopify_webhooks_pending_idx ON shopify_webhooks(account_id, id) WHERE done_at IS NULL;
CREATE INDEX shopify_webhooks_done_idx ON shopify_webhooks(done_at) WHERE done_at IS NOT NULL;

-- Products and customers to read again from the Admin API (webhooks only say "this changed").
-- One row per item, so a burst of updates to the same product is one read.
CREATE TABLE shopify_refresh (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind        text NOT NULL,                          -- product | customer
    shopify_id  bigint NOT NULL,
    queued_at   timestamptz NOT NULL DEFAULT now(),
    attempts    integer NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, kind, shopify_id)
);
CREATE INDEX shopify_refresh_queued_idx ON shopify_refresh(account_id, queued_at);

-- Shopify customers and the profile each one is linked to, with the consent Shopify last reported
-- for that exact address and number.
CREATE TABLE shopify_customers (
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    customer_id   bigint NOT NULL,
    profile_id    uuid REFERENCES profiles(id) ON DELETE SET NULL,
    email         text,
    phone         text,
    email_state   text,                                 -- Shopify's marketing state for `email`
    sms_state     text,                                 -- Shopify's SMS consent state for `phone`
    updated_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, customer_id)
);
CREATE INDEX shopify_customers_profile_idx ON shopify_customers(profile_id) WHERE profile_id IS NOT NULL;
-- Write-back candidates: Shopify still thinks they're subscribed.
CREATE INDEX shopify_customers_subscribed_idx ON shopify_customers(account_id) WHERE email_state IN ('SUBSCRIBED', 'PENDING') OR sms_state IN ('SUBSCRIBED', 'PENDING');

-- Which product each inventory item belongs to (inventory webhooks name only the item).
CREATE TABLE shopify_inventory_items (
    account_id         uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    inventory_item_id  bigint NOT NULL,
    product_id         bigint NOT NULL,
    PRIMARY KEY (account_id, inventory_item_id)
);

-- History imports (customers, products, orders) and product reconciles (products only), read through
-- bulk operations. One running or paused at a time per account.
CREATE TABLE shopify_imports (
    id             uuid PRIMARY KEY,
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind           text NOT NULL,                       -- history | products
    status         text NOT NULL,                       -- running | paused | cancelled | done | failed
    phases         text[] NOT NULL,                     -- what this run reads, in order
    phase          text NOT NULL,                       -- the one it's on (done at the end)
    orders_since   timestamptz NOT NULL,
    bulk_id        text,
    bulk_status    text,
    result_url_enc text,                                -- signed download link, encrypted
    result_offset  bigint NOT NULL DEFAULT 0,
    counts         jsonb NOT NULL DEFAULT '{}',
    error          text,
    started_by     uuid,
    started_at     timestamptz NOT NULL DEFAULT now(),
    finished_at    timestamptz,
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX shopify_imports_active_uq ON shopify_imports(account_id) WHERE status IN ('running', 'paused');
CREATE INDEX shopify_imports_account_idx ON shopify_imports(account_id, started_at DESC);

-- Discount codes made in the store for one person. The code is derived from a stable key, so a retry
-- after a crash makes the same code rather than a second one.
CREATE TABLE shopify_discount_codes (
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    code          text NOT NULL,
    template_key  text NOT NULL,
    profile_id    uuid REFERENCES profiles(id) ON DELETE SET NULL,
    discount_id   text,
    expires_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    redeemed_at   timestamptz,
    order_id      bigint,
    PRIMARY KEY (account_id, code)
);
CREATE INDEX shopify_discount_codes_profile_idx ON shopify_discount_codes(account_id, template_key, profile_id);

-- Customer data requests from Shopify (customers/data_request): the export the store owner hands over.
CREATE TABLE shopify_data_requests (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    shopify_request_id  bigint,
    customer_email      text,
    export              jsonb NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    downloaded_at       timestamptz
);
CREATE INDEX shopify_data_requests_account_idx ON shopify_data_requests(account_id, created_at DESC);

-- At most this many accounts do Shopify background work at once (one row per slot), so a burst at
-- one store can't fill the job workers that send email.
CREATE TABLE shopify_lanes (
    slot         integer PRIMARY KEY,
    account_id   uuid,
    lease_until  timestamptz
);
INSERT INTO shopify_lanes (slot) VALUES (1), (2);

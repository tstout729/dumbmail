-- Back-in-stock and price-drop alerts that survive retries and run safely side by side.

-- One row per restock: how many people it may tell (from the settings when it happened) and how
-- many it has claimed so far.
CREATE TABLE catalog_restocks (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    product_id  uuid NOT NULL REFERENCES catalog_products(id) ON DELETE CASCADE,
    variant_id  uuid REFERENCES catalog_variants(id) ON DELETE CASCADE,
    cap         integer,
    claimed     integer NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Requests made with an email the site doesn't know wait for the person to confirm by email.
ALTER TABLE catalog_stock_subscriptions
    ADD COLUMN confirmed_at    timestamptz,
    ADD COLUMN confirm_sent_at timestamptz,
    ADD COLUMN email_key       text,
    ADD COLUMN ip_hash         text,
    ADD COLUMN restock_id      uuid REFERENCES catalog_restocks(id) ON DELETE SET NULL,
    ADD COLUMN claimed_at      timestamptz;
-- Requests saved before confirmation existed (only on unreleased builds) count as confirmed.
UPDATE catalog_stock_subscriptions SET confirmed_at = created_at;
CREATE INDEX catalog_stock_subscriptions_recent_idx ON catalog_stock_subscriptions(account_id, created_at DESC);
CREATE INDEX catalog_stock_subscriptions_ip_idx ON catalog_stock_subscriptions(ip_hash, created_at DESC) WHERE ip_hash IS NOT NULL;
CREATE INDEX catalog_stock_subscriptions_inbox_idx ON catalog_stock_subscriptions(account_id, email_key, confirm_sent_at DESC) WHERE confirm_sent_at IS NOT NULL;
CREATE INDEX catalog_stock_subscriptions_claim_idx ON catalog_stock_subscriptions(restock_id) WHERE restock_id IS NOT NULL AND notified_at IS NULL;

-- Which price drop last claimed each person, so a retried job can finish its own claims.
ALTER TABLE catalog_price_drop_notices ADD COLUMN drop_key text;

-- One row per confirmation email sent, so the hourly cap for an account and the daily cap for an
-- inbox count emails (asking again for the same request sends another one), not requests.
CREATE TABLE catalog_stock_confirm_sends (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email_key  text NOT NULL,
    sent_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX catalog_stock_confirm_sends_account_idx ON catalog_stock_confirm_sends(account_id, sent_at DESC);
CREATE INDEX catalog_stock_confirm_sends_inbox_idx ON catalog_stock_confirm_sends(account_id, email_key, sent_at DESC);

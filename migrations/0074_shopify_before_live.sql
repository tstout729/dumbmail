-- Shopify app, fixes before it is switched on. Additive only, on the app's own tables.

-- When an order or checkout last moved a customer's link to its own contact: a customer snapshot
-- no newer than this can't move the link back (it still applies its consent, which has its own times).
ALTER TABLE shopify_customers ADD COLUMN linked_at timestamptz;

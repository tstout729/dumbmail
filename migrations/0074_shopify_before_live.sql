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

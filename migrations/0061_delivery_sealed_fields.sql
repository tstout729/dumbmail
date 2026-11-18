-- Custom objects in integration deliveries: object fields a type marks sensitive are kept out of a
-- delivery's stored action list; a retry needs them, so they're kept here, sealed with the server
-- key, and cleared when the delivery finishes. And a pull page's key (its integration, run and
-- request), so a retried page carries on with the same delivery and ledger.
-- Nullable columns with no default: no table rewrite.
ALTER TABLE integration_deliveries ADD COLUMN sealed_fields text;
ALTER TABLE integration_deliveries ADD COLUMN page_key text;
-- Pull pages not yet checkpointed, found by their key (small: cleared once the page's progress is saved).
CREATE INDEX integration_deliveries_open_pages ON integration_deliveries (integration_id, page_key) WHERE page_key IS NOT NULL;

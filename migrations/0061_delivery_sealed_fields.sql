-- Custom objects in integration deliveries: object fields a type marks sensitive are kept out of a
-- delivery's stored action list; a retry needs them, so they're kept here, sealed with the server
-- key, and cleared when the delivery finishes. And a pull page's key (its integration, run and
-- request), so a retried page carries on with the same delivery and ledger.
-- Nullable columns with no default: no table rewrite.
ALTER TABLE integration_deliveries ADD COLUMN sealed_fields text;
ALTER TABLE integration_deliveries ADD COLUMN page_key text;

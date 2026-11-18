-- Apps prove they hold a device with a per-install secret, issued at its first registration (the
-- app keeps it; only its SHA-256 is stored). Web devices prove it with their auth secret.
-- App devices from before install secrets keep their rows: one that belongs to nobody named counts
-- as not registered (its next registration is its first and gets a secret); a named one adopts a
-- secret only on a call with a grant for the person it belongs to.
ALTER TABLE push_devices ADD COLUMN IF NOT EXISTS install_secret_hash text;

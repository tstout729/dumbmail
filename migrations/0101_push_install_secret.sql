-- Apps prove they hold a device with a per-install secret, issued at its first registration (the
-- app keeps it; only its SHA-256 is stored). Web devices prove it with their auth secret.
ALTER TABLE push_devices ADD COLUMN IF NOT EXISTS install_secret_hash text;
-- App devices from before install secrets that belong to nobody named: the token alone can't prove
-- who holds them, so they go (the app registers again on its next launch and gets a secret then).
-- Named ones stay, and adopt a secret only on a call with a grant for the person they belong to.
DELETE FROM push_devices WHERE kind IN ('ios', 'android') AND anonymous AND install_secret_hash IS NULL;

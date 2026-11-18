-- Apps prove they hold a device with a per-install secret, issued at its first registration (the
-- app keeps it; only its SHA-256 is stored). Devices registered before this have none until their
-- first call with the same owner and token adopts one. Web devices prove it with their auth secret.
ALTER TABLE push_devices ADD COLUMN install_secret_hash text;

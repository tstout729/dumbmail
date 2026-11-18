-- Push notifications: web push (through the website snippet) and mobile push (through the
-- account's own Apple and Firebase credentials). See src/features/push and docs/PUSH.md.

-- One row per account that has set push up. Secrets are encrypted with crate::crypto.
CREATE TABLE push_settings (
    account_id          uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    -- Web push: the account's VAPID key pair (P-256). The public key is base64url, uncompressed.
    web_enabled         boolean NOT NULL DEFAULT false,
    vapid_public_key    text,
    vapid_private_enc   text,
    -- Where the service worker lives on the customer's site, e.g. /dumbmail-sw.js.
    sw_path             text NOT NULL DEFAULT '/dumbmail-sw.js',
    -- The in-page card shown before the browser's own permission prompt (see push::settings::Prompt).
    prompt              jsonb NOT NULL DEFAULT '{}',
    -- Apple Push Notification service, token-based auth.
    apns_key_id         text,
    apns_team_id        text,
    apns_bundle_id      text,
    apns_key_enc        text,
    apns_production     boolean NOT NULL DEFAULT true,
    -- Firebase Cloud Messaging HTTP v1: the service account JSON, encrypted.
    fcm_project_id      text,
    fcm_client_email    text,
    fcm_credentials_enc text,
    -- Signs identities passed from the customer's own server (user_hash), encrypted.
    identity_secret_enc text,
    identity_secret_hint text,
    -- The last credential problem a provider reported, as a short class (never a vendor body).
    apns_error          text,
    fcm_error           text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- A browser or app install that allowed notifications. `profile_id` is NULL until the visitor is
-- identified with a signed identity (a link token or a user_hash). Dashboard test devices belong to
-- a team member instead (`test_user_id`) and are never in an audience.
CREATE TABLE push_devices (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id    uuid REFERENCES profiles(id) ON DELETE CASCADE,
    test_user_id  uuid REFERENCES users(id) ON DELETE CASCADE,
    kind          text NOT NULL CHECK (kind IN ('web', 'ios', 'android')),
    -- Web: the push service endpoint URL. Mobile: the APNs device token or FCM registration token.
    address       text NOT NULL,
    p256dh        text,
    auth_secret   text,
    status        text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    -- Failures in a row (not counting retries inside one send); disabled after push::send::MAX_FAILURES.
    failures      integer NOT NULL DEFAULT 0,
    last_error    text,
    user_agent    text,
    app_version   text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    last_sent_at  timestamptz,
    disabled_at   timestamptz
);
CREATE UNIQUE INDEX push_devices_address_uq ON push_devices(account_id, kind, address);
CREATE INDEX push_devices_profile_idx ON push_devices(profile_id) WHERE profile_id IS NOT NULL;
CREATE INDEX push_devices_unlinked_idx ON push_devices(account_id, created_at) WHERE profile_id IS NULL AND test_user_id IS NULL;

-- People who turned push off (they get no push on any device until it's cleared).
CREATE TABLE push_opt_outs (
    profile_id  uuid PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source      text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- One push message (a `messages` row, channel 'push') to one device.
CREATE TABLE push_deliveries (
    id            uuid PRIMARY KEY,
    message_id    uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    device_id     uuid REFERENCES push_devices(id) ON DELETE SET NULL,
    kind          text NOT NULL,
    -- sending: claimed and handed (or about to be handed) to the provider; never sent again.
    status        text NOT NULL DEFAULT 'sending' CHECK (status IN ('sending', 'sent', 'failed', 'gone', 'skipped')),
    provider_id   text,
    error         text,
    attempts      integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    sent_at       timestamptz,
    delivered_at  timestamptz,
    clicked_at    timestamptz
);
CREATE UNIQUE INDEX push_deliveries_message_device_uq ON push_deliveries(message_id, device_id);
CREATE INDEX push_deliveries_message_idx ON push_deliveries(message_id);
CREATE INDEX push_deliveries_sending_idx ON push_deliveries(created_at) WHERE status = 'sending';

-- Device registrations per account, per hashed client address and hour (a bound on the public endpoints).
CREATE TABLE push_registration_counts (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    bucket      text NOT NULL,
    hour        timestamptz NOT NULL,
    n           integer NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, bucket, hour)
);

-- Push notifications: web push (through the website snippet) and mobile push (through the
-- account's own Apple and Firebase credentials). See src/features/push and docs/PUSH.md.

-- One row per account that has set push up. Secrets are encrypted with crate::crypto.
CREATE TABLE push_settings (
    account_id          uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    -- Web push: the account's VAPID key pair (P-256). The public key is base64url, uncompressed.
    web_enabled         boolean NOT NULL DEFAULT false,
    vapid_public_key    text,
    vapid_private_enc   text,
    -- Where the service worker lives on the customer's site, e.g. /dumbmail-sw.js. It's always
    -- registered with DumbMail's own narrow scope (/dumbmail-push/), so it controls no pages.
    sw_path             text NOT NULL DEFAULT '/dumbmail-sw.js',
    -- The in-page card shown before the browser's own permission prompt (see push::settings::Prompt).
    prompt              jsonb NOT NULL DEFAULT '{}',
    -- Apple Push Notification service, token-based auth.
    apns_key_id         text,
    apns_team_id        text,
    apns_bundle_id      text,
    apns_key_enc        text,
    -- The APNs environment for devices whose app didn't say which one it was built for.
    apns_production     boolean NOT NULL DEFAULT true,
    -- When APNs first accepted a notification with the current credentials and environment.
    apns_verified_at    timestamptz,
    -- Firebase Cloud Messaging HTTP v1: the service account JSON, encrypted.
    fcm_project_id      text,
    fcm_client_email    text,
    fcm_credentials_enc text,
    -- Bumped whenever APNs or FCM credentials change (keys provider connections and token caches).
    credentials_version bigint NOT NULL DEFAULT 1,
    -- Signs the customer's server's enrollment grants, encrypted.
    identity_secret_enc text,
    identity_secret_hint text,
    -- The last account-level problem a provider reported (credentials, project, topic or
    -- environment), as a short class (never a vendor body). Cleared when that platform accepts again.
    apns_error          text,
    fcm_error           text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- A browser or app install. Every device belongs to a profile: the person an enrollment grant named,
-- or (without one) an anonymous profile of its own (`anonymous`). Dashboard test devices belong to a
-- team member instead (`test_user_id`, no profile) and are never in an audience.
CREATE TABLE push_devices (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id    uuid REFERENCES profiles(id) ON DELETE CASCADE,
    test_user_id  uuid REFERENCES users(id) ON DELETE CASCADE,
    anonymous     boolean NOT NULL DEFAULT false,
    kind          text NOT NULL CHECK (kind IN ('web', 'ios', 'android')),
    -- Web: the push service endpoint URL. Mobile: the APNs device token or FCM registration token.
    address       text NOT NULL,
    p256dh        text,
    auth_secret   text,
    -- Bumped on every registration (a refresh too) and whenever the owner changes. A delivery is handed over only to
    -- the version it was made for, and provider results apply only to that version.
    version       bigint NOT NULL DEFAULT 1,
    -- The device's notification permission as last reported: only granted or (iOS) provisional is sent to.
    permission    text NOT NULL CHECK (permission IN ('granted', 'provisional', 'denied')),
    status        text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    -- Failures in a row (not counting retries); disabled after push::send::MAX_FAILURES.
    failures      integer NOT NULL DEFAULT 0,
    last_error    text,
    user_agent    text,
    app_version   text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    last_sent_at  timestamptz,
    disabled_at   timestamptz,
    CHECK (profile_id IS NOT NULL OR test_user_id IS NOT NULL)
);
CREATE UNIQUE INDEX push_devices_address_uq ON push_devices(account_id, kind, address);
CREATE INDEX push_devices_profile_idx ON push_devices(profile_id) WHERE profile_id IS NOT NULL;
CREATE INDEX push_devices_anonymous_idx ON push_devices(account_id) WHERE anonymous;
CREATE INDEX push_devices_disabled_idx ON push_devices(disabled_at) WHERE status = 'disabled';

-- People who turned push off (they get no push on any device until it's cleared).
CREATE TABLE push_opt_outs (
    profile_id  uuid PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source      text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- One push message (a `messages` row, channel 'push') to one device registration.
CREATE TABLE push_deliveries (
    id            uuid PRIMARY KEY,
    message_id    uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    device_id     uuid REFERENCES push_devices(id) ON DELETE SET NULL,
    -- The registration it was made for: the device's version and owner then.
    device_version bigint NOT NULL,
    owner_profile_id uuid,
    owner_test_user_id uuid,
    kind          text NOT NULL,
    -- queued: waiting for its hand-off. sending: claimed for the provider call (never sent again).
    -- retry: the push service said it didn't take it (busy or down); a background job tries again.
    -- unconfirmed: sent, but no answer came back (timeout, dropped connection): not sent again.
    status        text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'sending', 'sent', 'retry', 'unconfirmed', 'failed', 'gone', 'skipped')),
    provider_id   text,
    error         text,
    attempts      integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz,
    -- What the device is sent (built once, so a retry sends the same thing).
    payload       jsonb NOT NULL,
    claimed_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    sent_at       timestamptz,
    delivered_at  timestamptz,
    clicked_at    timestamptz
);
CREATE UNIQUE INDEX push_deliveries_message_device_uq ON push_deliveries(message_id, device_id);
CREATE INDEX push_deliveries_message_idx ON push_deliveries(message_id);
CREATE INDEX push_deliveries_sending_idx ON push_deliveries(claimed_at) WHERE status = 'sending';
CREATE INDEX push_deliveries_retry_idx ON push_deliveries(next_attempt_at) WHERE status = 'retry';

-- Device registrations per account, per hashed client address and hour (a bound on the public endpoints).
CREATE TABLE push_registration_counts (
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    bucket      text NOT NULL,
    hour        timestamptz NOT NULL,
    n           integer NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, bucket, hour)
);

-- WhatsApp through Meta's WhatsApp Cloud API, with the account's own WhatsApp Business account.
-- See src/features/whatsapp and docs/WHATSAPP.md.

-- The connection: one per account. Secrets are encrypted with crate::crypto and never returned.
-- `connected_via` says how it was set up ('manual' today; Embedded Signup would write the same
-- columns with 'embedded_signup').
CREATE TABLE whatsapp_settings (
    account_id          uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    waba_id             text,
    phone_number_id     text,
    access_token_enc    text,
    access_token_hint   text,
    app_secret_enc      text,
    -- Meta calls GET <webhook URL>?hub.verify_token=... once when the webhook is set up.
    verify_token        text NOT NULL,
    -- The account's webhook address is /webhooks/whatsapp/<webhook_key>. Not a secret on its own:
    -- every delivery is checked against the app secret's signature.
    webhook_key         text NOT NULL UNIQUE,
    connected_via       text NOT NULL DEFAULT 'manual',
    -- From the last "Check connection" (or a phone number quality webhook).
    display_phone_number text,
    verified_name       text,
    quality_rating      text,
    messaging_limit     text,
    checked_at          timestamptz,
    -- The last account-level problem Meta reported (token, permission, number), as a short class.
    error               text,
    error_at            timestamptz,
    -- Numbers WhatsApp tests may go to (the team's own phones), E.164.
    test_numbers        text[] NOT NULL DEFAULT '{}',
    -- Last time a webhook delivery checked out, so the settings can show the webhook works.
    last_webhook_at     timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
-- One account per WhatsApp phone number, so an inbound message has exactly one home.
CREATE UNIQUE INDEX whatsapp_settings_phone_uq ON whatsapp_settings (phone_number_id) WHERE phone_number_id IS NOT NULL;

-- Message templates, synced from Meta (and created from DumbMail). Keyed by Meta's id.
CREATE TABLE whatsapp_templates (
    id               uuid PRIMARY KEY,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    meta_id          text NOT NULL,
    name             text NOT NULL,
    language         text NOT NULL,
    category         text NOT NULL,          -- MARKETING | UTILITY | AUTHENTICATION
    status           text NOT NULL,          -- APPROVED | PENDING | REJECTED | PAUSED | DISABLED | ... | DELETED
    parameter_format text NOT NULL DEFAULT 'POSITIONAL', -- POSITIONAL | NAMED
    components       jsonb NOT NULL DEFAULT '[]',
    -- Buttons DumbMail tracks: {"<button index>": "<where the click goes>"}; the template's URL is
    -- DumbMail's click address with the per-message token as its variable.
    tracked_buttons  jsonb NOT NULL DEFAULT '{}',
    reason           text,
    quality          text,
    created_by_dumbmail boolean NOT NULL DEFAULT false,
    synced_at        timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, meta_id)
);
CREATE INDEX whatsapp_templates_account ON whatsapp_templates (account_id, name, language);

-- What DumbMail knows about one WhatsApp number in an account: its WhatsApp marketing consent
-- (separate from texts), whether it's on WhatsApp, and when its customer service window opened.
-- Consent belongs to the number: a profile that changes its number starts with no consent.
CREATE TABLE whatsapp_contacts (
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    phone_number    text NOT NULL,
    profile_id      uuid REFERENCES profiles(id) ON DELETE SET NULL,
    consent         text NOT NULL DEFAULT 'never_subscribed', -- subscribed | unsubscribed | never_subscribed
    consent_source  text,
    consent_at      timestamptz,
    wa_id           text,
    -- Meta said this number can't get WhatsApp messages (error 131026); nothing more goes to it
    -- until it messages the account.
    unreachable_at  timestamptz,
    -- Their latest message: the 24-hour customer service window runs from here.
    last_inbound_at timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, phone_number)
);
CREATE INDEX whatsapp_contacts_profile ON whatsapp_contacts (profile_id) WHERE profile_id IS NOT NULL;

-- Every WhatsApp consent change, with where it came from (until the shared consent history lands).
CREATE TABLE whatsapp_consent_log (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    phone_number  text NOT NULL,
    profile_id    uuid REFERENCES profiles(id) ON DELETE SET NULL,
    consent       text NOT NULL,
    source        text NOT NULL,
    detail        jsonb NOT NULL DEFAULT '{}',
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX whatsapp_consent_log_number ON whatsapp_consent_log (account_id, phone_number, created_at DESC);

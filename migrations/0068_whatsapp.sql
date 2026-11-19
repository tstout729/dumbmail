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
    -- The verified sender everything else is bound to: a new id whenever the WhatsApp Business
    -- Account or phone number changes (NULL while not connected). Consent, service windows, queued
    -- sends and claims made for another sender don't carry over.
    sender_id           uuid,
    -- Words that sign someone up when sent on WhatsApp (explicitly WhatsApp's own, not texting
    -- keywords), the answer to them, and quick-reply button payloads that mean "yes, send me
    -- WhatsApp messages".
    signup_keywords     text[] NOT NULL DEFAULT '{}',
    signup_reply        text,
    optin_buttons       text[] NOT NULL DEFAULT '{}',
    -- When the owners were last told about a quality problem (at most once a day).
    quality_alert_at    timestamptz,
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
    -- Meta's time for the last status event applied, and when it was last paused or disabled:
    -- only a later event or a read made after the pause can approve it again.
    status_at        timestamptz,
    paused_at        timestamptz,
    category_at      timestamptz,
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
    -- Meta's business-scoped user id for this person (BSUID), learned from signed webhooks.
    user_id         text,
    -- Undeliverable answers (131026) in a row; after a few the number counts as off WhatsApp
    -- (`unreachable_at`) until it writes in.
    undeliverable_count integer NOT NULL DEFAULT 0,
    unreachable_at  timestamptz,
    -- Their latest message: the 24-hour customer service window runs from here.
    last_inbound_at timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, phone_number)
);
CREATE INDEX whatsapp_contacts_profile ON whatsapp_contacts (profile_id) WHERE profile_id IS NOT NULL;
CREATE UNIQUE INDEX whatsapp_contacts_user ON whatsapp_contacts (account_id, user_id) WHERE user_id IS NOT NULL;

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

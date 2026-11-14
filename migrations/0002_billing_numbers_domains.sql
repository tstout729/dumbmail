-- Pay-as-you-go billing, phone numbers, bring-your-own SMS, custom domains and the website snippet.

ALTER TABLE accounts
    ADD COLUMN balance_micros bigint NOT NULL DEFAULT 0,          -- prepaid credit in millionths of a dollar
    ADD COLUMN stripe_customer_id text,
    ADD COLUMN stripe_payment_method_id text,
    ADD COLUMN auto_recharge_enabled boolean NOT NULL DEFAULT false,
    ADD COLUMN auto_recharge_threshold_micros bigint NOT NULL DEFAULT 5000000,
    ADD COLUMN auto_recharge_amount_micros bigint NOT NULL DEFAULT 25000000,
    ADD COLUMN auto_recharge_failed_at timestamptz,
    ADD COLUMN mail_from_domain text,
    ADD COLUMN link_domain text,
    ADD COLUMN link_domain_status text NOT NULL DEFAULT 'not_started',
    ADD COLUMN link_domain_records jsonb NOT NULL DEFAULT '[]',
    ADD COLUMN link_domain_provider_id text,
    ADD COLUMN sms_mode text NOT NULL DEFAULT 'platform',         -- platform | byo
    ADD COLUMN sms_byo_provider text,
    ADD COLUMN sms_byo_credentials text,                          -- AES-GCM encrypted JSON
    ADD COLUMN webhook_token text,
    ADD COLUMN onsite_last_seen_at timestamptz,
    ADD COLUMN onsite_last_url text;

-- gen_random_uuid() draws from the server's CSPRNG; two of them give 244 random bits.
UPDATE accounts SET webhook_token = replace(gen_random_uuid()::text || gen_random_uuid()::text, '-', '') WHERE webhook_token IS NULL;
CREATE INDEX accounts_link_domain_idx ON accounts (link_domain) WHERE link_domain IS NOT NULL;
CREATE UNIQUE INDEX accounts_link_domain_active_uq ON accounts (link_domain) WHERE link_domain_status = 'active';
CREATE UNIQUE INDEX accounts_webhook_token_uq ON accounts (webhook_token) WHERE webhook_token IS NOT NULL;

-- Credits (top-ups) are positive, charges negative. Message charges are rolled up per account, kind and day.
CREATE TABLE ledger (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind          text NOT NULL,            -- topup | auto_recharge | email | sms | mms | number | adjustment
    day           date,                     -- set for rolled-up usage rows
    quantity      bigint NOT NULL DEFAULT 0,
    amount_micros bigint NOT NULL,
    description   text NOT NULL,
    reference     text,                     -- Stripe session/payment id, phone number id …
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ledger_account_idx ON ledger (account_id, created_at DESC);
CREATE UNIQUE INDEX ledger_usage_uq ON ledger (account_id, kind, day) WHERE day IS NOT NULL;
CREATE UNIQUE INDEX ledger_reference_uq ON ledger (kind, reference) WHERE reference IS NOT NULL AND day IS NULL;

CREATE TABLE phone_numbers (
    id                 uuid PRIMARY KEY,
    account_id         uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    phone_number       text NOT NULL,
    type               text NOT NULL,      -- toll_free | local
    source             text NOT NULL,      -- purchased | connected
    provider           text NOT NULL,      -- telnyx | twilio
    provider_id        text,
    status             text NOT NULL,      -- active | needs_verification | verification_pending | verified | rejected | released
    verification_id    text,
    verification_note  text,
    monthly_price_micros bigint NOT NULL DEFAULT 0,
    next_charge_at     timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    released_at        timestamptz
);
CREATE UNIQUE INDEX phone_numbers_active_uq ON phone_numbers (phone_number) WHERE status <> 'released';
CREATE INDEX phone_numbers_account_idx ON phone_numbers (account_id);

ALTER TABLE messages ADD COLUMN segments integer NOT NULL DEFAULT 1, ADD COLUMN cost_micros bigint NOT NULL DEFAULT 0;

ALTER TABLE accounts ALTER COLUMN webhook_token SET DEFAULT replace(gen_random_uuid()::text || gen_random_uuid()::text, '-', '');
ALTER TABLE accounts ADD COLUMN link_domain_set_at timestamptz;
ALTER TABLE accounts ADD COLUMN free_month text, ADD COLUMN free_emails_used integer NOT NULL DEFAULT 0;

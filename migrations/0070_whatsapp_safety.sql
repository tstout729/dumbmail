-- WhatsApp: team test numbers, opt-outs known only by Meta's business-scoped user id, webhook
-- events already applied, and back-off after Meta's rate limits.

-- A team member's own phone, verified by sending the shown code from it to the account's WhatsApp
-- number. Tests go only to verified numbers of current members.
CREATE TABLE whatsapp_test_numbers (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    phone_number text NOT NULL,
    code         text NOT NULL,
    verified_at  timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, phone_number)
);

-- An opt-out from someone Meta identified only by a business-scoped user id; applied to their number
-- as soon as a signed webhook connects the two.
CREATE TABLE whatsapp_bsuid_optouts (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id    text NOT NULL,
    at         timestamptz NOT NULL,
    PRIMARY KEY (account_id, user_id)
);

-- Webhook events applied once (preferences have no id of their own: who, when, what).
CREATE TABLE whatsapp_seen (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    key        text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, key)
);

-- No sends before `until` for a sender ('sender') or a sender and one recipient ('pair:<number>').
CREATE TABLE whatsapp_backoff (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    scope      text NOT NULL,
    until      timestamptz NOT NULL,
    PRIMARY KEY (account_id, scope)
);

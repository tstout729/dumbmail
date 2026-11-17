-- Unique coupon codes: a coupon is a pool of single-use codes. Each person gets their own code,
-- assigned atomically when a message or sign-up form needs one (see src/features/coupons).

CREATE TABLE coupons (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- What templates use: {% coupon_code 'NAME' %}. Can't change after creation.
    name                text NOT NULL,
    description         text NOT NULL DEFAULT '',
    -- 'uploaded' (codes made in the store) or 'generated' (DumbMail made them from `pattern`).
    source              text NOT NULL DEFAULT 'uploaded',
    -- Generated coupons: {"prefix": "WELCOME-", "length": 8, "alphabet": "letters_digits"}.
    pattern             jsonb,
    -- Codes expire on a fixed date, or N days after they're given out (or never).
    expires_at          timestamptz,
    expires_after_days  integer,
    -- false: a person gets the same code every time (reminders, resends). true: a new code per message.
    new_code_each_time  boolean NOT NULL DEFAULT false,
    -- Alert when fewer than this many codes are left.
    low_stock_threshold integer,
    -- Most codes sign-up forms may hand out per hour, so made-up sign-ups can't drain the pool.
    form_codes_per_hour integer NOT NULL DEFAULT 200,
    -- Every code ever added (codes are never deleted one by one).
    codes_total         bigint NOT NULL DEFAULT 0,
    -- Set when a send or sign-up found no code left; cleared when codes are added.
    out_of_codes_at     timestamptz,
    low_stock_alerted_at timestamptz,
    low_stock_checked_at timestamptz,
    -- Deleted coupons vanish at once; a job removes their codes in batches, then the row.
    deleted_at          timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX coupons_account_name_idx ON coupons (account_id, lower(name)) WHERE deleted_at IS NULL;
CREATE INDEX coupons_low_stock_idx ON coupons (low_stock_checked_at NULLS FIRST) WHERE low_stock_threshold IS NOT NULL AND deleted_at IS NULL;

CREATE TABLE coupon_codes (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id     uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    coupon_id      uuid NOT NULL REFERENCES coupons(id) ON DELETE CASCADE,
    code           text NOT NULL,
    -- Upper-cased: codes are compared case-insensitively.
    code_norm      text NOT NULL,
    -- 'available', 'assigned' (given to someone) or 'redeemed' (used in an order).
    status         text NOT NULL DEFAULT 'available',
    profile_id     uuid REFERENCES profiles(id) ON DELETE SET NULL,
    -- Who the code is for: the profile id (one code per person), or profile + message for coupons
    -- that give a new code each time. Unique per coupon, so a person can never get two by racing.
    assign_key     text,
    -- 'message' or 'form'.
    assigned_via   text,
    -- The message (or, for forms, nothing) the code was last prepared for.
    message_id     uuid,
    form_id        uuid,
    -- Hashed visitor address of a form sign-up, for the per-visitor limit.
    ip_hash        text,
    assigned_at    timestamptz,
    -- Last time a send prepared this code; an undelivered code is only released after a quiet spell.
    claimed_at     timestamptz,
    -- When the person first actually got it (the provider accepted the message, or the form showed it).
    delivered_at   timestamptz,
    expires_at     timestamptz,
    redeemed_at    timestamptz,
    -- false when the only order that used it was recorded by the website script (a visitor could
    -- have made it up): shown as unverified, left out of revenue and the "redeemed" segment rule.
    redeemed_verified boolean,
    order_event_id uuid,
    order_value    double precision,
    created_at     timestamptz NOT NULL DEFAULT now()
);
-- text_pattern_ops also serves prefix searches (code_norm LIKE 'ABC%').
CREATE UNIQUE INDEX coupon_codes_code_idx ON coupon_codes (coupon_id, code_norm text_pattern_ops);
CREATE UNIQUE INDEX coupon_codes_assign_idx ON coupon_codes (coupon_id, assign_key) WHERE assign_key IS NOT NULL;
CREATE INDEX coupon_codes_available_idx ON coupon_codes (coupon_id, id) WHERE status = 'available';
CREATE INDEX coupon_codes_given_idx ON coupon_codes (coupon_id, status, assigned_at) WHERE status <> 'available';
CREATE INDEX coupon_codes_profile_idx ON coupon_codes (profile_id, coupon_id) WHERE profile_id IS NOT NULL;
CREATE INDEX coupon_codes_redeem_idx ON coupon_codes (account_id, profile_id, code_norm) WHERE status = 'assigned';
CREATE INDEX coupon_codes_undelivered_idx ON coupon_codes (claimed_at) WHERE status = 'assigned' AND delivered_at IS NULL AND assigned_via = 'message';
CREATE INDEX coupon_codes_form_idx ON coupon_codes (coupon_id, assigned_at) WHERE assigned_via = 'form';
CREATE INDEX coupon_codes_ip_idx ON coupon_codes (ip_hash, assigned_at) WHERE ip_hash IS NOT NULL;

-- Sends waiting because a coupon ran out of codes: one row per campaign or flow, cleared when codes are added.
CREATE TABLE coupon_holds (
    coupon_id   uuid NOT NULL REFERENCES coupons(id) ON DELETE CASCADE,
    -- 'campaign:<id>' or 'flow:<id>'.
    subject     text NOT NULL,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    campaign_id uuid REFERENCES campaigns(id) ON DELETE CASCADE,
    flow_id     uuid REFERENCES flows(id) ON DELETE CASCADE,
    first_at    timestamptz NOT NULL DEFAULT now(),
    last_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (coupon_id, subject)
);
CREATE INDEX coupon_holds_account_idx ON coupon_holds (account_id);

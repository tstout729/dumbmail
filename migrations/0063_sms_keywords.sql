-- Texting keywords ("text JOIN to 12345"), saved replies, and the inbox's settings.

CREATE TABLE sms_keywords (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    keyword text NOT NULL,                       -- stored lowercase, letters and digits only
    aliases text[] NOT NULL DEFAULT '{}',
    match_type text NOT NULL DEFAULT 'exact',    -- exact | starts_with
    subscribe boolean NOT NULL DEFAULT false,    -- subscribe the sender to SMS marketing
    double_opt_in boolean NOT NULL DEFAULT false, -- ask them to reply Y first
    list_id uuid REFERENCES lists(id) ON DELETE SET NULL,
    reply text NOT NULL DEFAULT '',
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX sms_keywords_account_keyword ON sms_keywords (account_id, keyword);

CREATE TABLE sms_saved_replies (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name text NOT NULL,
    body text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX sms_saved_replies_account ON sms_saved_replies (account_id, name);

CREATE TABLE sms_inbox_settings (
    account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    -- Required disclosures in keyword sign-up confirmations.
    program_name text NOT NULL DEFAULT '',
    message_frequency text NOT NULL DEFAULT '',
    default_reply_enabled boolean NOT NULL DEFAULT false,
    default_reply text NOT NULL DEFAULT '',
    away_reply_enabled boolean NOT NULL DEFAULT false,
    away_reply text NOT NULL DEFAULT '',
    -- Opening hours in the account's time zone; days are ISO weekdays (1 = Monday).
    open_hour smallint NOT NULL DEFAULT 9,
    close_hour smallint NOT NULL DEFAULT 17,
    open_days smallint[] NOT NULL DEFAULT '{1,2,3,4,5}',
    -- Members emailed a digest of new texts, at most every 30 minutes.
    notify_user_ids uuid[] NOT NULL DEFAULT '{}',
    notified_through timestamptz,
    last_digest_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- A keyword sign-up with double opt-in, waiting for the person's "Y".
CREATE TABLE sms_pending_optins (
    profile_id uuid PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    keyword_id uuid REFERENCES sms_keywords(id) ON DELETE CASCADE,
    inbound_id uuid REFERENCES sms_inbound(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

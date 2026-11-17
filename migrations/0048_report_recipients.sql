-- Scheduled report emails go only to members who confirmed their address for them (or signed in
-- with Google using it), and each delivery is written down before it's sent, so a retried job never
-- emails the same report twice.

-- One row per member asked to confirm, per account.
CREATE TABLE report_recipients (
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    id           uuid NOT NULL UNIQUE,                 -- in the confirmation link
    email        text NOT NULL,                        -- the address asked about
    asked_at     timestamptz NOT NULL DEFAULT now(),   -- when the last confirmation email went out
    confirmed_at timestamptz,
    PRIMARY KEY (account_id, user_id)
);

-- One row per scheduled send (the time it was due) and recipient, written before the email is sent.
CREATE TABLE report_deliveries (
    report_id  uuid NOT NULL REFERENCES saved_reports(id) ON DELETE CASCADE,
    occurrence timestamptz NOT NULL,
    user_id    uuid NOT NULL,
    email      text NOT NULL,
    status     text NOT NULL DEFAULT 'sending', -- sending | sent | failed
    error      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (report_id, occurrence, user_id)
);

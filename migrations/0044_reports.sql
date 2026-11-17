-- Reporting: attribution settings, saved reports and their email schedules, CSV exports, and the
-- inbox provider behind custom email domains (looked up from public MX records).

-- One row per account that changed its attribution settings. No row means the defaults: emails
-- opened or clicked in the 5 days before a conversion, texts clicked in the 24 hours before, the
-- newest message gets the credit, and the account's conversion_metric counts.
CREATE TABLE attribution_settings (
    account_id               uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    email_open_window_hours  integer NOT NULL DEFAULT 120,
    email_click_window_hours integer NOT NULL DEFAULT 120,
    sms_click_window_hours   integer NOT NULL DEFAULT 24,
    count_opens              boolean NOT NULL DEFAULT true,
    model                    text NOT NULL DEFAULT 'last_touch', -- last_touch | first_touch
    conversion_metrics       text[],                             -- NULL: accounts.conversion_metric
    updated_at               timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE saved_reports (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name                text NOT NULL,
    description         text NOT NULL DEFAULT '',
    spec                jsonb NOT NULL,
    schedule            text,                         -- NULL | weekly | monthly
    schedule_recipients uuid[] NOT NULL DEFAULT '{}', -- user ids of account members
    next_run_at         timestamptz,
    last_sent_at        timestamptz,
    last_error          text,
    created_by          uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX saved_reports_account_idx ON saved_reports (account_id, updated_at DESC);
CREATE INDEX saved_reports_due_idx ON saved_reports (next_run_at) WHERE schedule IS NOT NULL;

-- CSV files, written in chunks by a background job and downloaded by signed-in members.
CREATE TABLE exports (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind        text NOT NULL,                  -- profiles | events | campaign_recipients | suppressions | report
    params      jsonb NOT NULL DEFAULT '{}',
    status      text NOT NULL DEFAULT 'queued', -- queued | running | done | failed
    filename    text NOT NULL,
    row_count   bigint NOT NULL DEFAULT 0,
    size_bytes  bigint NOT NULL DEFAULT 0,
    chunks      integer NOT NULL DEFAULT 0,
    cursor      jsonb,
    error       text,
    created_by  uuid,
    created_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    expires_at  timestamptz NOT NULL DEFAULT now() + interval '7 days'
);
CREATE INDEX exports_account_idx ON exports (account_id, created_at DESC);
CREATE INDEX exports_expires_idx ON exports (expires_at);
-- One export running per account at a time (scheduled report files are written in one go and don't count).
CREATE UNIQUE INDEX exports_one_running_uq ON exports (account_id) WHERE status IN ('queued', 'running') AND kind <> 'report';

CREATE TABLE export_chunks (
    export_id uuid NOT NULL REFERENCES exports(id) ON DELETE CASCADE,
    seq       integer NOT NULL,
    bytes     bytea NOT NULL,
    PRIMARY KEY (export_id, seq)
);

-- Public DNS facts, shared by all accounts: which inbox provider hosts a custom email domain.
CREATE TABLE inbox_provider_cache (
    domain     text PRIMARY KEY,
    provider   text NOT NULL, -- gmail | outlook | yahoo | apple | other
    checked_at timestamptz NOT NULL DEFAULT now()
);

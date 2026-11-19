-- Sender screening (see src/features/screening and docs/SCREENING.md). On the hosted service a new
-- account is checked before its first real send; existing accounts are already approved.
--
-- The column is added with the constant default 'approved' (no table rewrite: every existing row
-- reads it as approved), then the default for new rows becomes 'pending'.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sending_review text NOT NULL DEFAULT 'approved';
ALTER TABLE accounts ALTER COLUMN sending_review SET DEFAULT 'pending';
-- NOT VALID: checked for every new or changed row, without scanning the existing ones.
ALTER TABLE accounts ADD CONSTRAINT accounts_sending_review_ck CHECK (sending_review IN ('pending', 'approved', 'review', 'blocked')) NOT VALID;
-- When the standing last changed, and a short note the customer may see.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sending_review_at timestamptz;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sending_review_note text;
-- The customer's answers before the first check: {business, list_source, monthly_volume}.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sending_review_answers jsonb;
-- One account check at a time: set while one runs (a stale claim lapses after a few minutes).
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sending_review_checking_at timestamptz;
-- Brands an operator confirmed this account belongs to (exempt from the lookalike rule for them).
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sending_review_brands text[] NOT NULL DEFAULT '{}';

-- Every check's outcome. `reasons` are plain sentences safe to show the customer; `signals` are the
-- facts behind the verdict, for the operator.
CREATE TABLE screenings (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind         text NOT NULL CHECK (kind IN ('account', 'import', 'domain', 'content', 'engagement')),
    -- What it was about: `campaign`, `flow`, `import`, `domain` (with `subject_id` or `subject_name`).
    subject      text,
    subject_id   uuid,
    subject_name text,
    verdict      text NOT NULL CHECK (verdict IN ('pass', 'review', 'block')),
    reasons      jsonb NOT NULL DEFAULT '[]',
    signals      jsonb NOT NULL DEFAULT '{}',
    -- `existing`: content that was already live when screening started (recorded once, no check).
    source       text NOT NULL CHECK (source IN ('rules', 'ai', 'operator', 'existing')),
    model        text,
    input_tokens  integer,
    output_tokens integer,
    -- SHA-256 of what a content check judged, so unchanged content is judged once.
    content_hash text,
    -- The same for each message it covered (a flow step, a campaign version), so what's about to
    -- send can be matched to a verdict.
    part_hashes  text[] NOT NULL DEFAULT '{}',
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX screenings_account_idx ON screenings (account_id, created_at DESC);
CREATE INDEX screenings_hash_idx ON screenings (account_id, content_hash, created_at DESC) WHERE content_hash IS NOT NULL;
CREATE INDEX screenings_subject_idx ON screenings (subject_id, created_at DESC) WHERE subject_id IS NOT NULL;
CREATE INDEX screenings_parts_idx ON screenings USING gin (part_hashes) WHERE kind = 'content';

-- Claude calls per account per day (the account's time zone), counted in the statement that takes one.
CREATE TABLE screening_ai_usage (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    day        date NOT NULL,
    calls      integer NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, day)
);

-- Flow messages waiting (for the account to be approved, or for their content's check): when each
-- step of each flow run first waited, so it's skipped after 3 days instead of waiting forever.
-- `run_key` is the flow run's id (or `p:<profile id>` for a send outside a run).
CREATE TABLE screening_waits (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    flow_id    uuid NOT NULL,
    node_id    text NOT NULL,
    run_key    text NOT NULL,
    since      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (flow_id, node_id, run_key)
);
CREATE INDEX screening_waits_account_idx ON screening_waits (account_id);

-- When each account's engagement was last checked (at most hourly, claimed in the write).
CREATE TABLE screening_engagement_checks (
    account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    checked_at timestamptz NOT NULL DEFAULT now()
);

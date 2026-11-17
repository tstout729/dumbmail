-- Moving an account from Klaviyo: one row per import (connect, survey, import, optional ongoing sync).
CREATE TABLE klaviyo_imports (
    id              uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- surveying | ready | importing | paused | syncing | done | cancelled | failed
    status          text NOT NULL DEFAULT 'surveying',
    -- The customer's private Klaviyo key, AES-GCM encrypted. Cleared when the import finishes (unless the
    -- ongoing sync is on), is cancelled or fails.
    api_key_enc     text,
    key_hint        text NOT NULL DEFAULT '',
    -- Klaviyo account name, id, time zone and currency, from GET /api/accounts.
    klaviyo_account jsonb NOT NULL DEFAULT '{}',
    survey          jsonb,
    options         jsonb NOT NULL DEFAULT '{}',
    -- The step running now, the Klaviyo page to fetch next and step bookkeeping, saved after every page
    -- so an import survives a restart.
    phase           text,
    cursor          text,
    step_state      jsonb NOT NULL DEFAULT '{}',
    -- Done/total per step.
    progress        jsonb NOT NULL DEFAULT '{}',
    -- Klaviyo-side numbers for the comparison report (list and segment sizes, event counts, revenue).
    source_counts   jsonb NOT NULL DEFAULT '{}',
    -- Switch-over checklist items the customer ticked by hand.
    checklist       jsonb NOT NULL DEFAULT '{}',
    sync_enabled    boolean NOT NULL DEFAULT false,
    last_sync_at    timestamptz,
    error           text,
    started_at      timestamptz,
    finished_at     timestamptz,
    compared_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX klaviyo_imports_account_idx ON klaviyo_imports (account_id, created_at DESC);
CREATE INDEX klaviyo_imports_sync_idx ON klaviyo_imports (last_sync_at) WHERE sync_enabled;

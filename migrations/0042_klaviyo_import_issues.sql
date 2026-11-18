-- Everything an import couldn't bring across exactly: unmapped segment rules, template tags, flow steps,
-- skipped records. Shown in the comparison report so nothing is lost silently.
CREATE TABLE klaviyo_import_issues (
    id          bigserial PRIMARY KEY,
    import_id   uuid NOT NULL REFERENCES klaviyo_imports(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    step        text NOT NULL,
    -- snapshot | partial | unmapped | skipped | converted | note
    kind        text NOT NULL,
    item        text NOT NULL DEFAULT '',
    klaviyo_id  text,
    local_id    uuid,
    message     text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX klaviyo_import_issues_import_idx ON klaviyo_import_issues (import_id, id);
CREATE UNIQUE INDEX klaviyo_import_issues_once_uq ON klaviyo_import_issues (import_id, step, kind, coalesce(klaviyo_id, ''), md5(message));

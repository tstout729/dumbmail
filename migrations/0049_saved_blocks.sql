-- Saved blocks ("universal content"): a named group of email blocks reused across emails.
-- Emails point at a saved block by id and pick up its current content whenever they render,
-- until they're sent: a campaign copies the content in when its send starts.
-- Deleting only hides a saved block (deleted_at), so emails that still point at it keep rendering.
CREATE TABLE saved_blocks (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name text NOT NULL,
    blocks jsonb NOT NULL,
    created_by uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz
);

CREATE INDEX saved_blocks_account ON saved_blocks (account_id, updated_at DESC) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX saved_blocks_account_name ON saved_blocks (account_id, lower(name)) WHERE deleted_at IS NULL;

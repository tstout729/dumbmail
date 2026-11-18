-- Link tagging (UTM parameters added to the links in campaign and flow messages).
-- Account defaults live in their own table; a campaign or flow can override them.
CREATE TABLE link_tag_settings (
    account_id   uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    enabled      boolean NOT NULL DEFAULT false,
    -- {"source", "medium", "campaign", "content", "term", "custom": [{"name", "value"}]}; see features::campaigns_plus::utm.
    params       jsonb NOT NULL DEFAULT '{}',
    -- Links to these domains (and their subdomains) are never tagged.
    skip_domains text[] NOT NULL DEFAULT '{}',
    updated_at   timestamptz NOT NULL DEFAULT now()
);

-- NULL: use the account's defaults. {"mode": "off"} or {"mode": "custom", "params": {...}}.
-- Nullable with no default, so adding them doesn't rewrite either table.
ALTER TABLE campaigns ADD COLUMN link_tags jsonb;
ALTER TABLE flows ADD COLUMN link_tags jsonb;

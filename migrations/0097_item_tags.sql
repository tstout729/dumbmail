-- Tags and archiving for campaigns, flows and templates.
CREATE TABLE tags (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX tags_account_name_uq ON tags(account_id, lower(name));

-- One row per tag on one item; exactly one of the item columns is set, so deleting the item
-- deletes its tags.
CREATE TABLE item_tags (
    tag_id      uuid NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    campaign_id uuid REFERENCES campaigns(id) ON DELETE CASCADE,
    flow_id     uuid REFERENCES flows(id) ON DELETE CASCADE,
    template_id uuid REFERENCES templates(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(campaign_id, flow_id, template_id) = 1)
);
CREATE UNIQUE INDEX item_tags_campaign_uq ON item_tags(campaign_id, tag_id) WHERE campaign_id IS NOT NULL;
CREATE UNIQUE INDEX item_tags_flow_uq ON item_tags(flow_id, tag_id) WHERE flow_id IS NOT NULL;
CREATE UNIQUE INDEX item_tags_template_uq ON item_tags(template_id, tag_id) WHERE template_id IS NOT NULL;
CREATE INDEX item_tags_tag_idx ON item_tags(tag_id);

-- Archived items are hidden from lists by default; archived campaigns can't be sent or scheduled
-- and archived flows can't be turned on. Nullable with no default: no table rewrite.
ALTER TABLE campaigns ADD COLUMN archived_at timestamptz;
ALTER TABLE flows ADD COLUMN archived_at timestamptz;
ALTER TABLE templates ADD COLUMN archived_at timestamptz;

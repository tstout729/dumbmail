-- A/B tests. Campaigns: `ab_test` holds the extra versions and test settings (version A is
-- `content`); test-slice messages record their version in `messages.variant`, and the rest of
-- the audience gets `ab_winner` once it is picked. Forms count views and sign-ups per version.
ALTER TABLE campaigns ADD COLUMN ab_test jsonb;
ALTER TABLE campaigns ADD COLUMN ab_test_ends_at timestamptz;
ALTER TABLE campaigns ADD COLUMN ab_winner text;
ALTER TABLE messages ADD COLUMN variant text;
CREATE INDEX campaigns_testing_idx ON campaigns(ab_test_ends_at) WHERE status = 'testing';

CREATE TABLE form_variant_stats (
    form_id uuid NOT NULL REFERENCES forms(id) ON DELETE CASCADE,
    variant text NOT NULL,
    views   bigint NOT NULL DEFAULT 0,
    submits bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (form_id, variant)
);

-- Form views and sign-ups by hour, so the Forms page can show a date range and a trend.
-- forms.views and forms.submits stay the all-time totals.
CREATE TABLE form_stats_hourly (
    form_id    uuid NOT NULL REFERENCES forms(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    hour       timestamptz NOT NULL,
    views      bigint NOT NULL DEFAULT 0,
    submits    bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (form_id, hour)
);
CREATE INDEX form_stats_hourly_account_idx ON form_stats_hourly(account_id, hour);

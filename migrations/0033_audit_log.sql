-- Who did what and when. Kept 13 months. Never holds secrets or message bodies.
CREATE TABLE audit_log (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    actor_type    text NOT NULL,            -- user | api_key | system
    actor_user_id uuid,
    actor_key_id  uuid,
    actor_label   text,
    action        text NOT NULL,
    target        text,
    details       jsonb NOT NULL DEFAULT '{}',
    ip            text,
    user_agent    text
);
CREATE INDEX audit_log_account_idx ON audit_log (account_id, created_at DESC);
CREATE INDEX audit_log_action_idx ON audit_log (account_id, action, created_at DESC);
CREATE INDEX audit_log_created_idx ON audit_log (created_at);

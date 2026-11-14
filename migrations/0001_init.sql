-- Postlark initial schema

CREATE TABLE accounts (
    id            uuid PRIMARY KEY,
    public_key    text NOT NULL UNIQUE,
    name          text NOT NULL,
    website       text,
    from_name     text,
    from_email    text,
    reply_to      text,
    sms_from      text,
    timezone      text NOT NULL DEFAULT 'America/Los_Angeles',
    address       text,
    brand_color   text NOT NULL DEFAULT '#1f6f5c',
    logo_url      text,
    plan          text NOT NULL DEFAULT 'free',
    onboarding    jsonb NOT NULL DEFAULT '{"brand":false,"sender":false,"contacts":false,"first_flow":false,"dismissed":false}',
    sending_domain        text,
    sending_domain_status text NOT NULL DEFAULT 'not_started',
    sending_domain_records jsonb NOT NULL DEFAULT '[]',
    conversion_metric text NOT NULL DEFAULT 'Placed Order',
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id            uuid PRIMARY KEY,
    email         text NOT NULL UNIQUE,
    name          text,
    password_hash text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE memberships (
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    role       text NOT NULL DEFAULT 'owner',
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, account_id)
);

CREATE TABLE sessions (
    token_hash text PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user_idx ON sessions(user_id);

CREATE TABLE api_keys (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         text NOT NULL,
    prefix       text NOT NULL,
    key_hash     text NOT NULL UNIQUE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);
CREATE INDEX api_keys_account_idx ON api_keys(account_id);

CREATE TABLE profiles (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    email         text,
    phone_number  text,
    external_id   text,
    first_name    text,
    last_name     text,
    properties    jsonb NOT NULL DEFAULT '{}',
    email_consent text NOT NULL DEFAULT 'never_subscribed',
    sms_consent   text NOT NULL DEFAULT 'never_subscribed',
    email_suppressed_reason text, -- hard bounce or spam complaint: never email again
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    last_event_at timestamptz
);
CREATE UNIQUE INDEX profiles_email_uq ON profiles(account_id, email) WHERE email IS NOT NULL;
CREATE UNIQUE INDEX profiles_phone_uq ON profiles(account_id, phone_number) WHERE phone_number IS NOT NULL;
CREATE UNIQUE INDEX profiles_external_uq ON profiles(account_id, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX profiles_account_created_idx ON profiles(account_id, created_at DESC);
CREATE INDEX profiles_properties_gin ON profiles USING gin (properties jsonb_path_ops);

CREATE TABLE lists (
    id            uuid PRIMARY KEY,
    account_id    uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name          text NOT NULL,
    double_opt_in boolean NOT NULL DEFAULT false,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX lists_account_idx ON lists(account_id);

CREATE TABLE list_members (
    list_id    uuid NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    added_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (list_id, profile_id)
);
CREATE INDEX list_members_profile_idx ON list_members(profile_id);

CREATE TABLE segments (
    id           uuid PRIMARY KEY,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         text NOT NULL,
    definition   jsonb NOT NULL,
    member_count integer NOT NULL DEFAULT 0,
    refreshed_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX segments_account_idx ON segments(account_id);

-- Materialized membership, refreshed by the worker, so "entered segment" flow triggers can fire.
CREATE TABLE segment_members (
    segment_id uuid NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    entered_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (segment_id, profile_id)
);
CREATE INDEX segment_members_profile_idx ON segment_members(profile_id);

CREATE TABLE metrics (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, name)
);

CREATE TABLE events (
    id          uuid PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    metric_id   uuid NOT NULL REFERENCES metrics(id) ON DELETE CASCADE,
    properties  jsonb NOT NULL DEFAULT '{}',
    value       double precision,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    unique_id   text,
    message_id  uuid,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX events_metric_time_idx ON events(account_id, metric_id, occurred_at DESC);
CREATE INDEX events_profile_time_idx ON events(profile_id, occurred_at DESC);
CREATE INDEX events_message_idx ON events(message_id) WHERE message_id IS NOT NULL;
CREATE UNIQUE INDEX events_unique_uq ON events(account_id, metric_id, unique_id) WHERE unique_id IS NOT NULL;

CREATE TABLE templates (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       text NOT NULL,
    channel    text NOT NULL,
    content    jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX templates_account_idx ON templates(account_id);

CREATE TABLE campaigns (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       text NOT NULL,
    channel    text NOT NULL,
    status     text NOT NULL DEFAULT 'draft',
    audience   jsonb NOT NULL DEFAULT '{"include":[],"exclude":[]}',
    content    jsonb NOT NULL DEFAULT '{}',
    send_at    timestamptz,
    sent_at    timestamptz,
    error      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX campaigns_account_idx ON campaigns(account_id, created_at DESC);

CREATE TABLE flows (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       text NOT NULL,
    status     text NOT NULL DEFAULT 'draft',
    trigger    jsonb NOT NULL,
    definition jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX flows_account_idx ON flows(account_id);

CREATE TABLE flow_runs (
    id           uuid PRIMARY KEY,
    flow_id      uuid NOT NULL REFERENCES flows(id) ON DELETE CASCADE,
    account_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id   uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    current_node text,
    status       text NOT NULL DEFAULT 'active', -- active | completed | exited | failed
    context      jsonb NOT NULL DEFAULT '{}',
    visited      jsonb NOT NULL DEFAULT '[]',
    started_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz
);
CREATE INDEX flow_runs_flow_profile_idx ON flow_runs(flow_id, profile_id);
CREATE INDEX flow_runs_active_idx ON flow_runs(flow_id) WHERE status = 'active';

CREATE TABLE messages (
    id                  uuid PRIMARY KEY,
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id          uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    channel             text NOT NULL,
    campaign_id         uuid REFERENCES campaigns(id) ON DELETE SET NULL,
    flow_id             uuid REFERENCES flows(id) ON DELETE SET NULL,
    flow_node_id        text,
    to_address          text NOT NULL,
    subject             text,
    status              text NOT NULL DEFAULT 'queued', -- queued | sent | delivered | bounced | complained | failed | skipped
    provider_message_id text,
    error               text,
    is_test             boolean NOT NULL DEFAULT false,
    sent_at             timestamptz,
    delivered_at        timestamptz,
    opened_at           timestamptz,
    clicked_at          timestamptz,
    unsubscribed_at     timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX messages_campaign_idx ON messages(campaign_id) WHERE campaign_id IS NOT NULL;
CREATE INDEX messages_flow_idx ON messages(flow_id, flow_node_id) WHERE flow_id IS NOT NULL;
CREATE INDEX messages_profile_idx ON messages(profile_id, created_at DESC);
CREATE INDEX messages_account_time_idx ON messages(account_id, created_at DESC);
CREATE INDEX messages_provider_idx ON messages(provider_message_id) WHERE provider_message_id IS NOT NULL;
CREATE UNIQUE INDEX messages_campaign_profile_uq ON messages(campaign_id, profile_id) WHERE campaign_id IS NOT NULL AND NOT is_test;

CREATE TABLE message_clicks (
    id         bigserial PRIMARY KEY,
    message_id uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    url        text NOT NULL,
    clicked_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX message_clicks_message_idx ON message_clicks(message_id);

CREATE TABLE forms (
    id         uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       text NOT NULL,
    status     text NOT NULL DEFAULT 'draft',
    kind       text NOT NULL DEFAULT 'popup',
    list_id    uuid REFERENCES lists(id) ON DELETE SET NULL,
    config     jsonb NOT NULL DEFAULT '{}',
    views      bigint NOT NULL DEFAULT 0,
    submits    bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX forms_account_idx ON forms(account_id);

-- Durable job queue (FOR UPDATE SKIP LOCKED).
CREATE TABLE jobs (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL,
    payload      jsonb NOT NULL DEFAULT '{}',
    run_at       timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    locked_at    timestamptz,
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_ready_idx ON jobs(run_at) WHERE locked_at IS NULL;

-- Lenient casts used by segment conditions on free-form JSON properties.
CREATE FUNCTION pl_num(t text) RETURNS numeric LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN RETURN t::numeric; EXCEPTION WHEN others THEN RETURN NULL; END $$;
CREATE FUNCTION pl_ts(t text) RETURNS timestamptz LANGUAGE plpgsql STABLE AS $$
BEGIN RETURN t::timestamptz; EXCEPTION WHEN others THEN RETURN NULL; END $$;

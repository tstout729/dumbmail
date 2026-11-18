-- Product and stay reviews (see docs/REVIEWS.md). All new tables.

CREATE TABLE review_settings (
    account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    -- Verified reviews at or above this many stars with no flagged words are published at once.
    auto_publish boolean NOT NULL DEFAULT true,
    auto_publish_min_rating smallint NOT NULL DEFAULT 4 CHECK (auto_publish_min_rating BETWEEN 1 AND 5),
    flag_words text[] NOT NULL DEFAULT '{}',
    -- Up to 3 questions: [{id, label, kind: "choice"|"text", options: [..]}].
    questions jsonb NOT NULL DEFAULT '[]',
    -- The "leave a review" entry without a link (unverified, confirmed by email). Off by default.
    public_reviews boolean NOT NULL DEFAULT false,
    request_days int NOT NULL DEFAULT 60 CHECK (request_days BETWEEN 7 AND 365),
    -- A coupon (by name) emailed as a thank-you for any review, whatever the rating.
    thank_you_coupon text,
    -- A sign-up form whose list a reviewer may join by ticking a box (through the form's consent path).
    subscribe_form_id uuid REFERENCES forms(id) ON DELETE SET NULL,
    widget jsonb NOT NULL DEFAULT '{}',
    -- More metrics that mean a real purchase or stay (besides Placed Order, Fulfilled Order and the
    -- server-only metrics integrations declare), for verified reviews.
    purchase_metrics text[] NOT NULL DEFAULT '{}',
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- What people review: a catalog product, a listing or a service. Holds the published summary,
-- changed in the same transaction as every publish and unpublish.
CREATE TABLE review_subjects (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    external_id text NOT NULL,
    kind text NOT NULL DEFAULT 'product' CHECK (kind IN ('product', 'listing', 'service')),
    name text NOT NULL,
    image_url text,
    url text,
    -- Each field's source: true when the catalog, the API, an integration or a server-recorded event
    -- supplied it. Only trusted images and addresses are ever loaded or linked; an untrusted name is
    -- shown as text to the person who supplied it only.
    name_trusted boolean NOT NULL DEFAULT false,
    image_trusted boolean NOT NULL DEFAULT false,
    url_trusted boolean NOT NULL DEFAULT false,
    review_count int NOT NULL DEFAULT 0,
    rating_sum bigint NOT NULL DEFAULT 0,
    rating_counts int[] NOT NULL DEFAULT '{0,0,0,0,0}',
    -- The same for reviews collected here (not imported): what structured data may mark up.
    native_count int NOT NULL DEFAULT 0,
    native_sum bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, external_id)
);

-- One signed link for one person and one thing they bought or booked.
CREATE TABLE review_requests (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES review_subjects(id) ON DELETE CASCADE,
    -- Requests made together (one order) share a batch, so the page can offer the other items.
    batch_id uuid NOT NULL,
    -- order id + item: one ask per order line, ever.
    line_key text,
    source text NOT NULL CHECK (source IN ('flow', 'email', 'api')),
    -- Tied to a purchase or stay a trusted source recorded for this person and item.
    verified boolean NOT NULL,
    -- What proves it: 'event:<id>' (a server-recorded purchase or stay event) or 'api:<order ref>'.
    verified_ref text,
    -- The message that carried it, when a block or tag in an email made it (sent once that message is).
    message_id uuid,
    -- The name and image shown to this person.
    item jsonb NOT NULL DEFAULT '{}',
    flow_run_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    sent_at timestamptz,
    reminded_at timestamptz,
    review_id uuid,
    submitted_at timestamptz
);
CREATE UNIQUE INDEX review_requests_line ON review_requests (account_id, line_key) WHERE line_key IS NOT NULL;
CREATE INDEX review_requests_person ON review_requests (profile_id, subject_id);
CREATE INDEX review_requests_account ON review_requests (account_id, created_at);
CREATE INDEX review_requests_batch ON review_requests (batch_id);
CREATE INDEX review_requests_unsent ON review_requests (message_id) WHERE sent_at IS NULL AND message_id IS NOT NULL;

CREATE TABLE reviews (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES review_subjects(id) ON DELETE CASCADE,
    profile_id uuid REFERENCES profiles(id) ON DELETE CASCADE,
    request_id uuid UNIQUE REFERENCES review_requests(id) ON DELETE SET NULL,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'published', 'rejected', 'removed')),
    reject_reason text,
    rating smallint NOT NULL CHECK (rating BETWEEN 1 AND 5),
    title text NOT NULL DEFAULT '',
    body text NOT NULL DEFAULT '',
    display_name text NOT NULL,
    recommend boolean,
    answers jsonb NOT NULL DEFAULT '[]',
    verified boolean NOT NULL DEFAULT false,
    -- Imports: what the source said about verification (never shown publicly as a badge).
    source_verified boolean NOT NULL DEFAULT false,
    source text NOT NULL CHECK (source IN ('request', 'public', 'import')),
    incentivized boolean NOT NULL DEFAULT false,
    flags text[] NOT NULL DEFAULT '{}',
    featured boolean NOT NULL DEFAULT false,
    reply text,
    replied_at timestamptz,
    -- Public reviews: the address waiting to confirm (cleared once confirmed), its key and the visitor.
    pending_email text,
    email_key text,
    ip_hash text,
    wants_marketing boolean NOT NULL DEFAULT false,
    confirmed_at timestamptz,
    confirm_sent_at timestamptz,
    thanked_at timestamptz,
    reviewed_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    edited_at timestamptz,
    published_at timestamptz,
    moderated_at timestamptz,
    import_id uuid,
    import_row int
);
CREATE INDEX reviews_subject ON reviews (subject_id, status, reviewed_at DESC);
CREATE INDEX reviews_account ON reviews (account_id, created_at DESC);
CREATE INDEX reviews_profile ON reviews (profile_id);
CREATE INDEX reviews_unconfirmed ON reviews (created_at) WHERE confirmed_at IS NULL;
-- One public review waiting for its confirmation per (normalised) address per account.
CREATE UNIQUE INDEX reviews_one_pending_public ON reviews (account_id, email_key) WHERE confirmed_at IS NULL AND source = 'public';

-- Review photos, re-encoded (no EXIF) and served at /i/rv/{id}.jpg once their review is published.
CREATE TABLE review_photos (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    request_id uuid REFERENCES review_requests(id) ON DELETE CASCADE,
    review_id uuid REFERENCES reviews(id) ON DELETE CASCADE,
    position smallint NOT NULL DEFAULT 0,
    width int NOT NULL,
    height int NOT NULL,
    size int NOT NULL,
    bytes bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX review_photos_review ON review_photos (review_id);
CREATE INDEX review_photos_request ON review_photos (request_id);

-- CSV imports of existing reviews, run as background jobs.
CREATE TABLE review_imports (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed')),
    mapping jsonb NOT NULL,
    publish boolean NOT NULL DEFAULT true,
    csv text NOT NULL,
    total int NOT NULL DEFAULT 0,
    next_row int NOT NULL DEFAULT 0,
    imported int NOT NULL DEFAULT 0,
    skipped int NOT NULL DEFAULT 0,
    errors jsonb NOT NULL DEFAULT '[]',
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE INDEX review_imports_account ON review_imports (account_id, created_at DESC);
-- The same CSV row is imported once, however often its batch runs.
CREATE UNIQUE INDEX reviews_import_row ON reviews (import_id, import_row) WHERE import_id IS NOT NULL;

-- One send intent per ask or reminder: its id is the message id, so a retried step finds the
-- message it already sent instead of sending another.
CREATE TABLE review_sends (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    flow_run_id uuid,
    node_id text,
    kind text NOT NULL CHECK (kind IN ('ask', 'reminder', 'api')),
    channel text NOT NULL,
    request_ids uuid[] NOT NULL,
    -- Requests this intent created (dropped again if nothing reaches the person).
    created_ids uuid[] NOT NULL DEFAULT '{}',
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sent', 'skipped', 'failed')),
    claimed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE UNIQUE INDEX review_sends_step ON review_sends (flow_run_id, node_id) WHERE flow_run_id IS NOT NULL;
CREATE INDEX review_sends_pending ON review_sends (created_at) WHERE status = 'pending';

-- Confirmation emails for public reviews per normalised address per day, across all accounts.
CREATE TABLE review_address_limits (
    email_key text NOT NULL,
    day date NOT NULL,
    sent int NOT NULL DEFAULT 0,
    PRIMARY KEY (email_key, day)
);

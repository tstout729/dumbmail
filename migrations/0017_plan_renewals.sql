-- One row per attempt to charge a plan renewal, written before Stripe is called. The request
-- (card, amount, idempotency key) is frozen here, so a retry after an unknown outcome repeats the
-- exact same request, and an attempt is resolved (by asking Stripe) before another one starts.
CREATE TABLE plan_renewals (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- The end of the paid month this attempt renews. Results only touch the account while it still
    -- has this period, so a stale attempt can't change a plan bought since.
    period_end timestamptz NOT NULL,
    attempt smallint NOT NULL,
    tier smallint NOT NULL,
    amount_cents bigint NOT NULL,
    stripe_customer text,
    stripe_payment_method text,
    idempotency_key text NOT NULL UNIQUE,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'succeeded', 'declined')),
    payment_intent text,
    failure text,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- When a worker last took this attempt; others leave it alone for a couple of minutes.
    claimed_at timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    UNIQUE (account_id, period_end, attempt)
);
CREATE INDEX plan_renewals_pending_idx ON plan_renewals (claimed_at) WHERE status = 'pending';
CREATE INDEX plan_renewals_account_idx ON plan_renewals (account_id, period_end);

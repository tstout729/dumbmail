-- Monthly email plans (hosted billing). A plan is a prepaid monthly email allowance charged to the
-- saved card; emails past the free allowance use it before prepaid credit. Tier N costs $10 * 2^(N-1)
-- for 30,000 * 2^(N-1) emails (see src/subscription.rs).
ALTER TABLE accounts
    ADD COLUMN email_plan_tier smallint,
    ADD COLUMN email_plan_next_tier smallint,
    ADD COLUMN email_plan_cancel_at_period_end boolean NOT NULL DEFAULT false,
    ADD COLUMN email_plan_period_start timestamptz,
    ADD COLUMN email_plan_period_end timestamptz,
    ADD COLUMN email_plan_emails_included bigint NOT NULL DEFAULT 0,
    ADD COLUMN email_plan_emails_used bigint NOT NULL DEFAULT 0,
    ADD COLUMN email_plan_renewal_attempts smallint NOT NULL DEFAULT 0,
    ADD COLUMN email_plan_renewal_failed_at timestamptz,
    ADD COLUMN email_plan_ended_at timestamptz,
    ADD COLUMN email_plan_ended_reason text;

CREATE INDEX accounts_email_plan_due_idx ON accounts (email_plan_period_end) WHERE email_plan_tier IS NOT NULL;

-- Card payments for plans. stripe_ref (Checkout session or PaymentIntent id) makes each payment apply once.
CREATE TABLE plan_payments (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('start', 'upgrade', 'renewal')),
    tier smallint NOT NULL,
    amount_cents bigint NOT NULL,
    stripe_ref text NOT NULL UNIQUE,
    -- True when the plan had changed by the time the payment landed, so the money went to credit instead.
    credited_to_balance boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX plan_payments_account_idx ON plan_payments (account_id, created_at DESC);

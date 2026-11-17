-- An account can send from several verified domains (e.g. two brands, or brand.com and mail.brand.com).
-- The old single-domain columns on accounts are copied here and no longer used.
CREATE TABLE sending_domains (
    id               uuid PRIMARY KEY,
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    domain           text NOT NULL,
    status           text NOT NULL DEFAULT 'pending',
    records          jsonb NOT NULL DEFAULT '[]',
    mail_from_domain text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    verified_at      timestamptz,
    UNIQUE (account_id, domain)
);
CREATE INDEX sending_domains_domain_idx ON sending_domains (domain);

INSERT INTO sending_domains (id, account_id, domain, status, records, mail_from_domain, created_at, verified_at)
SELECT gen_random_uuid(), id, sending_domain, sending_domain_status, sending_domain_records, mail_from_domain, now(),
       CASE WHEN sending_domain_status = 'verified' THEN now() END
FROM accounts WHERE sending_domain IS NOT NULL;

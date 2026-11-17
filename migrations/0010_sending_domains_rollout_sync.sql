-- During a rolling deploy the previous version still writes the old columns; mirror those writes here.
-- The new code never writes them, so this only fires for the old version. Drop it in a later migration.
CREATE FUNCTION sync_legacy_sending_domain() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.sending_domain IS NOT NULL AND OLD.sending_domain IS DISTINCT FROM NEW.sending_domain THEN
        DELETE FROM sending_domains WHERE account_id = NEW.id AND domain = OLD.sending_domain;
    END IF;
    IF NEW.sending_domain IS NOT NULL THEN
        INSERT INTO sending_domains (id, account_id, domain, status, records, mail_from_domain, verified_at)
        VALUES (gen_random_uuid(), NEW.id, NEW.sending_domain, NEW.sending_domain_status, NEW.sending_domain_records, NEW.mail_from_domain,
                CASE WHEN NEW.sending_domain_status = 'verified' THEN now() END)
        ON CONFLICT (account_id, domain) DO UPDATE
            SET status = EXCLUDED.status, records = EXCLUDED.records, mail_from_domain = EXCLUDED.mail_from_domain,
                verified_at = CASE WHEN EXCLUDED.status = 'verified' THEN COALESCE(sending_domains.verified_at, now()) END;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER accounts_legacy_sending_domain
    AFTER UPDATE OF sending_domain, sending_domain_status, sending_domain_records, mail_from_domain ON accounts
    FOR EACH ROW EXECUTE FUNCTION sync_legacy_sending_domain();

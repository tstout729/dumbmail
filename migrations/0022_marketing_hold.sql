-- Marketing hold: we have this person's details for service reasons (a booking, an order), but they
-- haven't agreed to marketing. Campaigns never email them; flows do only when marked as service
-- messages. A subscribed person is never held: the hold clears the moment they become subscribed to
-- email, by any route (forms, double opt-in, the API, an integration's explicit opt-in).
-- (Another change adds the same column; IF NOT EXISTS lets the two apply in either order.)
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS marketing_hold boolean NOT NULL DEFAULT false;

-- Who set the profile's current email and phone: 'integration:<id>' when an integration supplied
-- it. A flow marked as service messages reaches a held person only at an address an integration
-- supplied (never one a web form or the website script added). Any other change of the address
-- clears it (the trigger below), whatever path made the change.
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS email_source text;
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS phone_source text;

CREATE OR REPLACE FUNCTION profiles_clear_marketing_hold() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.marketing_hold AND NEW.email_consent = 'subscribed' THEN
        NEW.marketing_hold := false;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        IF NEW.email IS DISTINCT FROM OLD.email AND NEW.email_source IS NOT DISTINCT FROM OLD.email_source THEN
            NEW.email_source := NULL;
        END IF;
        IF NEW.phone_number IS DISTINCT FROM OLD.phone_number AND NEW.phone_source IS NOT DISTINCT FROM OLD.phone_source THEN
            NEW.phone_source := NULL;
        END IF;
    END IF;
    RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS profiles_clear_marketing_hold ON profiles;
CREATE TRIGGER profiles_clear_marketing_hold BEFORE INSERT OR UPDATE OF email_consent, marketing_hold, email, phone_number ON profiles
    FOR EACH ROW EXECUTE FUNCTION profiles_clear_marketing_hold();

-- Events only a server may record (an integration's bookings and orders): the public browser
-- endpoint refuses them, so nobody can start a booking flow from a web page.
ALTER TABLE metrics ADD COLUMN IF NOT EXISTS server_only boolean NOT NULL DEFAULT false;

-- Values an integration itself wrote on a profile (e.g. guesty_guest_id, stripe_customer_id).
-- Integrations find people by these, never by ordinary properties, which a browser can also set:
-- otherwise anyone could claim a guest's id and receive their booking emails.
-- Its index is built after startup (features::integrations::indexes), not here: building it would
-- scan profiles while holding a lock that blocks writes. Lookups work without it, slower.
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS integration_keys jsonb NOT NULL DEFAULT '{}';

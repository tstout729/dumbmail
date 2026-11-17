-- Marketing hold: we have this person's details for service reasons (a booking, an order), but they
-- haven't agreed to marketing. Campaigns never email them; flows do only when marked as service
-- messages. A subscribed person is never held: the hold clears the moment they become subscribed to
-- email, by any route (forms, double opt-in, the API, an integration's explicit opt-in).
-- (Another change adds the same column; IF NOT EXISTS lets the two apply in either order.)
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS marketing_hold boolean NOT NULL DEFAULT false;

CREATE OR REPLACE FUNCTION profiles_clear_marketing_hold() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.marketing_hold AND NEW.email_consent = 'subscribed' THEN
        NEW.marketing_hold := false;
    END IF;
    RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS profiles_clear_marketing_hold ON profiles;
CREATE TRIGGER profiles_clear_marketing_hold BEFORE INSERT OR UPDATE OF email_consent, marketing_hold ON profiles
    FOR EACH ROW EXECUTE FUNCTION profiles_clear_marketing_hold();

-- Events only a server may record (an integration's bookings and orders): the public browser
-- endpoint refuses them, so nobody can start a booking flow from a web page.
ALTER TABLE metrics ADD COLUMN IF NOT EXISTS server_only boolean NOT NULL DEFAULT false;

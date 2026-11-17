-- Preference page: lists people can join or leave themselves from the unsubscribe link, instead of
-- only unsubscribing from all email.
ALTER TABLE lists ADD COLUMN show_on_preferences boolean NOT NULL DEFAULT false;
ALTER TABLE lists ADD COLUMN description text NOT NULL DEFAULT '';

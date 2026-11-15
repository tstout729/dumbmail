-- Social profile links pulled from the brand's website (shown in email footers).
ALTER TABLE accounts ADD COLUMN social_links jsonb NOT NULL DEFAULT '{}';

-- Google sign-in: users may have no password, and are matched by Google's stable subject id.
ALTER TABLE users ALTER COLUMN password_hash DROP NOT NULL;
ALTER TABLE users ADD COLUMN google_sub text UNIQUE;

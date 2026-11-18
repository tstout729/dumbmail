-- Messages in each person's language (src/features/translations).

-- A person's language as a BCP 47 tag (normalized: fr, fr-CA, pt-BR). Nullable, no default: adding it
-- doesn't rewrite the table.
ALTER TABLE profiles ADD COLUMN locale text;

-- The language a message was sent in, when its content had language versions (for reports).
-- Nullable, no default, no index.
ALTER TABLE messages ADD COLUMN language text;

-- The language existing content is written in, and the other languages the account sends in.
CREATE TABLE account_languages (
    account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    default_language text NOT NULL DEFAULT 'en',
    languages text[] NOT NULL DEFAULT '{}',
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Terms with a fixed translation per language, or that are never translated (a brand name).
CREATE TABLE translation_glossary (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    term text NOT NULL,
    never_translate boolean NOT NULL DEFAULT false,
    translations jsonb NOT NULL DEFAULT '{}',
    note text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX translation_glossary_term ON translation_glossary (account_id, lower(term));

-- The account's own wording of DumbMail's system texts (unsubscribe page, preference page, double
-- opt-in email and page, email footer lines), per language. Missing keys use the shipped text.
CREATE TABLE system_texts (
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    language text NOT NULL,
    key text NOT NULL,
    text text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, language, key)
);

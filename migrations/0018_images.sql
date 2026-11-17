-- Uploaded images (form images, email images), served publicly at /i/{id}.
CREATE TABLE images (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    content_type text NOT NULL,
    size int NOT NULL,
    sha256 text NOT NULL,
    bytes bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- The same file uploaded twice is stored once per account.
    UNIQUE (account_id, sha256)
);

CREATE INDEX images_account_created ON images (account_id, created_at DESC);

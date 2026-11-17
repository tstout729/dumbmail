-- A double opt-in confirmation is for the address it was sent to. When that address leaves the
-- profile (a merge with another address, an edit), the waiting confirmation is voided so clicking it
-- can't subscribe a different address. Nullable with no default: adding it doesn't rewrite the table.
ALTER TABLE optin_confirmations ADD COLUMN voided_at timestamptz;

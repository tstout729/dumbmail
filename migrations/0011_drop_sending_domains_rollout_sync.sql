-- The rollout to sending_domains is finished; nothing writes the old columns any more.
DROP TRIGGER IF EXISTS accounts_legacy_sending_domain ON accounts;
DROP FUNCTION IF EXISTS sync_legacy_sending_domain();

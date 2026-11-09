-- Revoking an environment API key is permanent, as it is for an org key.
-- `active` stays the reversible switch: a revoked key is inactive for good.
ALTER TABLE api_key ADD COLUMN revoked_at text CHECK (revoked_at IS NULL OR active = 0);
CREATE TRIGGER api_key_revocation_is_permanent BEFORE UPDATE ON api_key
WHEN OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS NOT OLD.revoked_at
BEGIN SELECT RAISE(ABORT, 'API key revocation is permanent'); END;

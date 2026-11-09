-- Revoking an environment API key is permanent, as it is for an org key.
-- `active` stays the reversible switch: a revoked key is inactive for good.
ALTER TABLE hot.api_key ADD COLUMN revoked_at timestamptz;
ALTER TABLE hot.api_key ADD CONSTRAINT api_key_revoked_is_inactive
    CHECK (revoked_at IS NULL OR NOT active);
CREATE TRIGGER api_key_revocation_is_permanent BEFORE UPDATE ON hot.api_key
    FOR EACH ROW WHEN (OLD.revoked_at IS NOT NULL AND OLD.revoked_at IS DISTINCT FROM NEW.revoked_at)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('revoked_at');

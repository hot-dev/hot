-- Org-scoped API keys replace the 3.0.0 management credentials (hm_) and
-- /v1/management. An org key belongs to one root organization and calls only
-- control-plane routes. Environment API keys keep their exact binding.
SET row_security = on;
-- A rolling upgrade keeps 3.0.0 processes serving after this migration, and
-- they read api_key and api_key_management_issuer on every authentication.
-- Take every lock those reads contend for first, in one order, so that a
-- request mid-transaction cannot deadlock the migration.
LOCK TABLE hot.api_key, hot.management_key, hot.api_key_management_issuer IN ACCESS EXCLUSIVE MODE;

-- Runtime keys that a management credential issued depended on it. Retiring
-- the issuer must not turn them into independent keys, so they are
-- deactivated first.
DO $$
DECLARE issued uuid[];
BEGIN
    PERFORM set_config('hot.scope_mode', 'bootstrap', true);
    PERFORM set_config('hot.scope_operation', 'authentication', true);
    SELECT COALESCE(array_agg(api_key_id), '{}') INTO issued FROM hot.api_key_management_issuer;
    PERFORM set_config('hot.scope_operation', 'onboarding', true);
    UPDATE hot.api_key SET active = false, active_toggle_at = CURRENT_TIMESTAMP
    WHERE api_key_id = ANY (issued);
    PERFORM set_config('hot.scope_mode', '', true);
    PERFORM set_config('hot.scope_operation', '', true);
END $$;

-- The management tables stay, empty of authority, until a later release
-- drops them: a 3.0.0 API that cannot read them rejects every credential. No
-- policy lets anyone delete their rows or revoke their keys, so the owner
-- does both with row security briefly unforced. The issuer rows go, because
-- they would block deleting the keys they name; every management key is
-- revoked, so a 3.0.0 process refuses hm_ credentials; and with their write
-- policies dropped, nothing can create new ones.
ALTER TABLE hot.api_key_management_issuer NO FORCE ROW LEVEL SECURITY;
ALTER TABLE hot.management_key NO FORCE ROW LEVEL SECURITY;
DELETE FROM hot.api_key_management_issuer;
UPDATE hot.management_key SET revoked_at = CURRENT_TIMESTAMP WHERE revoked_at IS NULL;
ALTER TABLE hot.api_key_management_issuer FORCE ROW LEVEL SECURITY;
ALTER TABLE hot.management_key FORCE ROW LEVEL SECURITY;
DROP POLICY api_key_management_issuer_insert ON hot.api_key_management_issuer;
DROP POLICY management_key_insert ON hot.management_key;
DROP POLICY management_key_update ON hot.management_key;
COMMENT ON TABLE hot.management_key IS 'Retired by Hot 3.0.1: read only, for Hot 3.0.0 processes during an upgrade.';
COMMENT ON TABLE hot.api_key_management_issuer IS 'Retired by Hot 3.0.1: always empty, for Hot 3.0.0 processes during an upgrade.';

-- An org key is owned by its organization; its creator is attribution only.
-- A minted org key names its issuing org key, in the same organization.
CREATE TABLE hot.org_api_key (
    org_api_key_id uuid PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES hot.org(org_id),
    description text NOT NULL,
    key_data jsonb NOT NULL,
    previous_key_data jsonb,
    previous_key_expires_at timestamptz,
    permissions jsonb NOT NULL DEFAULT '{}'::jsonb,
    active boolean NOT NULL DEFAULT true,
    expires_at timestamptz,
    revoked_at timestamptz,
    issuer_org_api_key_id uuid REFERENCES hot.org_api_key(org_api_key_id) ON DELETE RESTRICT,
    created_by_user_id uuid NOT NULL REFERENCES hot."user"(user_id),
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (issuer_org_api_key_id IS DISTINCT FROM org_api_key_id)
);
CREATE INDEX org_api_key_org_idx ON hot.org_api_key(org_id);
CREATE INDEX org_api_key_issuer_idx ON hot.org_api_key(issuer_org_api_key_id);

-- An environment key minted by an org key. Lineage is immutable: a cascade
-- or detach would let a delegated key outlive its issuer's revocation.
CREATE TABLE hot.api_key_issuer (
    api_key_id uuid PRIMARY KEY REFERENCES hot.api_key(api_key_id) ON DELETE RESTRICT,
    issuer_org_api_key_id uuid NOT NULL REFERENCES hot.org_api_key(org_api_key_id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX api_key_issuer_issuer_idx ON hot.api_key_issuer(issuer_org_api_key_id);

-- Optional expiry, and a previous secret that stays valid until a rotation's
-- overlap ends. Rotation keeps the key ID, so lineage is unaffected.
ALTER TABLE hot.api_key
    ADD COLUMN expires_at timestamptz,
    ADD COLUMN previous_key_data jsonb,
    ADD COLUMN previous_key_expires_at timestamptz;

ALTER TABLE hot.org_api_key ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.org_api_key FORCE ROW LEVEL SECURITY;
-- Only the owning organization's exact scope sees its org keys: never a child,
-- an environment, or a read set.
CREATE POLICY org_api_key_read ON hot.org_api_key FOR SELECT USING (
    hot.scope_bootstrap('authentication') OR hot.scope_org_writes(org_id)
);
CREATE POLICY org_api_key_insert ON hot.org_api_key FOR INSERT WITH CHECK (hot.scope_org_writes(org_id));
CREATE POLICY org_api_key_update ON hot.org_api_key FOR UPDATE USING (hot.scope_org_writes(org_id)) WITH CHECK (hot.scope_org_writes(org_id));
-- No DELETE policy: org keys are revoked, never removed.
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.org_api_key
    FOR EACH ROW WHEN ((OLD.org_id, OLD.issuer_org_api_key_id) IS DISTINCT FROM (NEW.org_id, NEW.issuer_org_api_key_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'issuer_org_api_key_id');

ALTER TABLE hot.api_key_issuer ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.api_key_issuer FORCE ROW LEVEL SECURITY;
CREATE POLICY api_key_issuer_read ON hot.api_key_issuer FOR SELECT USING (
    hot.scope_bootstrap('authentication') OR EXISTS (
        SELECT 1 FROM hot.api_key k WHERE k.api_key_id = api_key_issuer.api_key_id AND hot.scope_env_allows(k.env_id)
    )
);
CREATE POLICY api_key_issuer_insert ON hot.api_key_issuer FOR INSERT WITH CHECK (
    EXISTS (SELECT 1 FROM hot.api_key k WHERE k.api_key_id = api_key_issuer.api_key_id AND hot.scope_env_writes(k.env_id))
);
-- No UPDATE or DELETE policy: issuer lineage cannot be detached or reassigned.

-- Org-scoped API keys replace the 3.0.0 management credentials (hm_) and
-- /v1/management. An org key belongs to one root organization and calls only
-- control-plane routes. Environment API keys keep their exact binding.

-- Runtime keys that a management credential issued depended on it. Dropping
-- the issuer must not turn them into independent keys, so they are
-- deactivated first.
UPDATE api_key SET active = 0, active_toggle_at = CURRENT_TIMESTAMP
WHERE api_key_id IN (SELECT api_key_id FROM api_key_management_issuer);
DROP TRIGGER delegated_api_key_no_delete;
DROP TRIGGER api_key_management_issuer_no_delete;
DROP TRIGGER immutable_api_key_management_issuer;
DROP TRIGGER management_key_scope_immutable;
DROP TABLE api_key_management_issuer;
DROP TABLE management_key;

-- An org key is owned by its organization; its creator is attribution only.
-- A minted org key names its issuing org key, in the same organization.
CREATE TABLE org_api_key (
    org_api_key_id blob PRIMARY KEY NOT NULL,
    org_id blob NOT NULL REFERENCES org(org_id),
    description text NOT NULL,
    key_data text NOT NULL,
    previous_key_data text,
    previous_key_expires_at text,
    permissions text NOT NULL DEFAULT '{}',
    active integer NOT NULL DEFAULT 1,
    expires_at text,
    revoked_at text,
    issuer_org_api_key_id blob REFERENCES org_api_key(org_api_key_id) ON DELETE RESTRICT,
    created_by_user_id blob NOT NULL REFERENCES user(user_id),
    created_at text NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at text NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (issuer_org_api_key_id IS NULL OR issuer_org_api_key_id <> org_api_key_id)
);
CREATE INDEX org_api_key_org_idx ON org_api_key(org_id);
CREATE INDEX org_api_key_issuer_idx ON org_api_key(issuer_org_api_key_id);
CREATE TRIGGER org_api_key_scope_immutable BEFORE UPDATE ON org_api_key
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.issuer_org_api_key_id IS NOT OLD.issuer_org_api_key_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER org_api_key_no_delete BEFORE DELETE ON org_api_key
BEGIN SELECT RAISE(ABORT, 'Org API keys must be revoked'); END;

-- An environment key minted by an org key. Lineage is immutable: a cascade
-- or detach would let a delegated key outlive its issuer's revocation.
CREATE TABLE api_key_issuer (
    api_key_id blob PRIMARY KEY NOT NULL REFERENCES api_key(api_key_id) ON DELETE RESTRICT,
    issuer_org_api_key_id blob NOT NULL REFERENCES org_api_key(org_api_key_id) ON DELETE RESTRICT,
    created_at text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX api_key_issuer_issuer_idx ON api_key_issuer(issuer_org_api_key_id);
CREATE TRIGGER api_key_issuer_immutable BEFORE UPDATE ON api_key_issuer
BEGIN SELECT RAISE(ABORT, 'API key issuer is immutable'); END;
CREATE TRIGGER api_key_issuer_no_delete BEFORE DELETE ON api_key_issuer
BEGIN SELECT RAISE(ABORT, 'API key issuer is immutable'); END;
-- Foreign keys are not always enforced on SQLite connections; keep the
-- delegated key itself from being deleted out from under its lineage.
CREATE TRIGGER delegated_api_key_no_delete BEFORE DELETE ON api_key
WHEN EXISTS (SELECT 1 FROM api_key_issuer WHERE api_key_id = OLD.api_key_id)
BEGIN SELECT RAISE(ABORT, 'Delegated API keys must be deactivated'); END;

-- Optional expiry, and a previous secret that stays valid until a rotation's
-- overlap ends. Rotation keeps the key ID, so lineage is unaffected.
ALTER TABLE api_key ADD COLUMN expires_at text;
ALTER TABLE api_key ADD COLUMN previous_key_data text;
ALTER TABLE api_key ADD COLUMN previous_key_expires_at text;

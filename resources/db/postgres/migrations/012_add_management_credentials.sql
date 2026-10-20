CREATE TABLE hot.management_key (
    management_key_id uuid PRIMARY KEY,
    root_org_id uuid NOT NULL REFERENCES hot.org(org_id),
    created_by_user_id uuid NOT NULL REFERENCES hot."user"(user_id),
    name text NOT NULL,
    secret_hash bytea NOT NULL,
    capabilities jsonb NOT NULL,
    target_org_ids jsonb NOT NULL DEFAULT '[]'::jsonb,
    all_children boolean NOT NULL DEFAULT false,
    delegation_permissions jsonb,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX management_key_root_idx ON hot.management_key(root_org_id);
CREATE TABLE hot.api_key_management_issuer (
    api_key_id uuid PRIMARY KEY REFERENCES hot.api_key(api_key_id) ON DELETE CASCADE,
    management_key_id uuid NOT NULL REFERENCES hot.management_key(management_key_id),
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX api_key_management_issuer_key_idx ON hot.api_key_management_issuer(management_key_id);

ALTER TABLE hot.management_key ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.management_key FORCE ROW LEVEL SECURITY;
CREATE POLICY management_key_read ON hot.management_key FOR SELECT USING (
    hot.scope_bootstrap('authentication') OR hot.scope_org_allows(root_org_id)
);
CREATE POLICY management_key_insert ON hot.management_key FOR INSERT WITH CHECK (hot.scope_org_allows(root_org_id));
CREATE POLICY management_key_update ON hot.management_key FOR UPDATE USING (hot.scope_org_allows(root_org_id)) WITH CHECK (hot.scope_org_allows(root_org_id));
ALTER TABLE hot.api_key_management_issuer ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.api_key_management_issuer FORCE ROW LEVEL SECURITY;
CREATE POLICY api_key_management_issuer_read ON hot.api_key_management_issuer FOR SELECT USING (
    hot.scope_bootstrap('authentication') OR EXISTS (
        SELECT 1 FROM hot.api_key k WHERE k.api_key_id = api_key_management_issuer.api_key_id AND hot.scope_env_allows(k.env_id)
    )
);
CREATE POLICY api_key_management_issuer_insert ON hot.api_key_management_issuer FOR INSERT WITH CHECK (
    EXISTS (SELECT 1 FROM hot.api_key k WHERE k.api_key_id = api_key_management_issuer.api_key_id AND hot.scope_env_writes(k.env_id))
);
-- No UPDATE or DELETE policy: runtime-key issuer lineage cannot be detached or
-- reassigned. Parent API-key deletion may cascade its no-longer-usable lineage.

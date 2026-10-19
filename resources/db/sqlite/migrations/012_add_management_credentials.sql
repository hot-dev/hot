CREATE TABLE management_key (
    management_key_id blob PRIMARY KEY NOT NULL,
    root_org_id blob NOT NULL REFERENCES org(org_id),
    created_by_user_id blob NOT NULL REFERENCES user(user_id),
    name text NOT NULL,
    secret_hash blob NOT NULL,
    capabilities text NOT NULL,
    target_org_ids text NOT NULL DEFAULT '[]',
    all_children integer NOT NULL DEFAULT 0,
    delegation_permissions text,
    expires_at text NOT NULL,
    revoked_at text,
    created_at text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX management_key_root_idx ON management_key(root_org_id);
CREATE TABLE api_key_management_issuer (
    api_key_id blob PRIMARY KEY NOT NULL REFERENCES api_key(api_key_id) ON DELETE CASCADE,
    management_key_id blob NOT NULL REFERENCES management_key(management_key_id),
    created_at text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX api_key_management_issuer_key_idx ON api_key_management_issuer(management_key_id);
CREATE TRIGGER immutable_api_key_management_issuer BEFORE UPDATE ON api_key_management_issuer
BEGIN SELECT RAISE(ABORT, 'API key management issuer is immutable'); END;

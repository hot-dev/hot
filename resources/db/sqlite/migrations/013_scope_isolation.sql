-- SQLite shares ownership integrity with PostgreSQL; request authorization
-- remains in the service layer because SQLite has no database RLS.

-- Refuse an upgrade over inconsistent alert deliveries or retained runtime
-- references, matching PostgreSQL. Preserve historical provenance for an
-- explicit, reviewed repair and retry instead of rewriting it.
CREATE TEMP TABLE scope_integrity_preflight (valid integer CHECK (valid = 1));
INSERT INTO scope_integrity_preflight
SELECT 0 FROM alert_delivery d
JOIN alert a ON a.alert_id = d.alert_id
JOIN alert_subscription s ON s.alert_subscription_id = d.subscription_id
JOIN alert_destination t ON t.alert_destination_id = d.destination_id
WHERE s.org_id <> a.org_id OR t.org_id <> a.org_id
   OR (s.env_id IS NOT NULL AND s.env_id <> a.env_id)
LIMIT 1;
DROP TABLE scope_integrity_preflight;
CREATE TEMP TABLE retained_runtime_scope_preflight (
    run_origin integer NOT NULL DEFAULT 1
        CONSTRAINT run_origin_environment CHECK (run_origin = 1),
    task_origin integer NOT NULL DEFAULT 1
        CONSTRAINT task_origin_environment CHECK (task_origin = 1),
    run_event integer NOT NULL DEFAULT 1
        CONSTRAINT run_event_environment CHECK (run_event = 1)
);
INSERT INTO retained_runtime_scope_preflight(run_origin)
SELECT 0 FROM run r
LEFT JOIN run origin ON origin.run_id = r.origin_run_id
WHERE r.origin_run_id IS NOT NULL
  AND (origin.run_id IS NULL OR r.env_id <> origin.env_id)
LIMIT 1;
INSERT INTO retained_runtime_scope_preflight(task_origin)
SELECT 0 FROM task t
LEFT JOIN run origin ON origin.run_id = t.origin_run_id
WHERE t.origin_run_id IS NOT NULL
  AND (origin.run_id IS NULL OR t.env_id <> origin.env_id)
LIMIT 1;
INSERT INTO retained_runtime_scope_preflight(run_event)
SELECT 0 FROM run r
LEFT JOIN event e ON e.event_id = r.event_id
WHERE r.event_id IS NOT NULL
  AND (e.event_id IS NULL OR r.env_id <> e.env_id)
LIMIT 1;
DROP TABLE retained_runtime_scope_preflight;

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

-- Store each call's environment, matching PostgreSQL. The same pass
-- reconciles historical trace sizes, which 014's storage accounting sums.
ALTER TABLE call ADD COLUMN env_id blob NOT NULL DEFAULT X'00000000000000000000000000000000';
-- TEXT length counts Unicode characters; storage admission counts UTF-8 bytes.
-- Keep the established fixed per-call overhead and include persisted errors.
UPDATE call SET env_id = (SELECT env_id FROM run WHERE run.run_id = call.run_id), size =
    COALESCE(length(CAST(args AS BLOB)), 0) +
    COALESCE(length(CAST(return_value AS BLOB)), 0) +
    COALESCE(length(CAST(flow AS BLOB)), 0) +
    COALESCE(length(CAST(error AS BLOB)), 0) + 50;

CREATE INDEX call_env_start_time_idx ON call(env_id, start_time);
CREATE TRIGGER call_environment_insert BEFORE INSERT ON call
WHEN NOT EXISTS (SELECT 1 FROM run r WHERE r.run_id=NEW.run_id AND r.env_id=NEW.env_id)
BEGIN SELECT RAISE(ABORT, 'Call environment does not own its run'); END;
CREATE TRIGGER call_environment_update BEFORE UPDATE OF env_id, run_id ON call
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.run_id IS NOT OLD.run_id
BEGIN SELECT RAISE(ABORT, 'Call ownership is immutable'); END;

CREATE TRIGGER alert_delivery_scope_insert BEFORE INSERT ON alert_delivery
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM alert a
        JOIN alert_subscription s ON s.alert_subscription_id = NEW.subscription_id
        JOIN alert_destination d ON d.alert_destination_id = NEW.destination_id
        WHERE a.alert_id = NEW.alert_id AND s.org_id = a.org_id AND d.org_id = a.org_id
          AND (s.env_id IS NULL OR s.env_id = a.env_id)
    ) THEN RAISE(ABORT, 'Alert delivery ownership mismatch') END;
END;
CREATE TRIGGER alert_delivery_scope_update BEFORE UPDATE ON alert_delivery
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM alert a
        JOIN alert_subscription s ON s.alert_subscription_id = NEW.subscription_id
        JOIN alert_destination d ON d.alert_destination_id = NEW.destination_id
        WHERE a.alert_id = NEW.alert_id AND s.org_id = a.org_id AND d.org_id = a.org_id
          AND (s.env_id IS NULL OR s.env_id = a.env_id)
    ) THEN RAISE(ABORT, 'Alert delivery ownership mismatch') END;
END;
CREATE TRIGGER alert_delivery_scope_immutable BEFORE UPDATE ON alert_delivery
WHEN NEW.alert_id IS NOT OLD.alert_id OR NEW.subscription_id IS NOT OLD.subscription_id
    OR NEW.destination_id IS NOT OLD.destination_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER management_key_scope_immutable BEFORE UPDATE ON management_key
WHEN NEW.root_org_id IS NOT OLD.root_org_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;

-- Retain delegated lineage when a key is revoked. Do not let physical deletion
-- (including the original FK cascade) turn an issued key into an independent one.
CREATE TRIGGER delegated_api_key_no_delete BEFORE DELETE ON api_key
WHEN EXISTS (SELECT 1 FROM api_key_management_issuer WHERE api_key_id = OLD.api_key_id)
BEGIN SELECT RAISE(ABORT, 'Delegated API keys must be deactivated'); END;
CREATE TRIGGER api_key_management_issuer_no_delete BEFORE DELETE ON api_key_management_issuer
BEGIN SELECT RAISE(ABORT, 'API key management issuer is immutable'); END;

-- SQLite shares ownership integrity with PostgreSQL; request authorization
-- remains in the service layer because SQLite has no database RLS.
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

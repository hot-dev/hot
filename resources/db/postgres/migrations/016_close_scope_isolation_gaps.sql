-- Management credentials are account control data. An environment of the root
-- organization must not gain account authority merely by sharing its org_id.
ALTER POLICY management_key_read ON hot.management_key USING (
    hot.scope_bootstrap('authentication') OR hot.scope_org_writes(root_org_id)
);
ALTER POLICY management_key_insert ON hot.management_key
    WITH CHECK (hot.scope_org_writes(root_org_id));
ALTER POLICY management_key_update ON hot.management_key
    USING (hot.scope_org_writes(root_org_id))
    WITH CHECK (hot.scope_org_writes(root_org_id));
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.management_key
    FOR EACH ROW WHEN (OLD.root_org_id IS DISTINCT FROM NEW.root_org_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('root_org_id');

-- A cascade would let an environment delete and recreate a delegated API key
-- without its issuer, laundering the management ceiling and revocation chain.
-- Delegated keys are revoked using active=false; physical deletion requires
-- administrative removal of the protected issuer record first.
ALTER TABLE hot.api_key_management_issuer
    DROP CONSTRAINT api_key_management_issuer_api_key_id_fkey;
ALTER TABLE hot.api_key_management_issuer
    ADD CONSTRAINT api_key_management_issuer_api_key_id_fkey
    FOREIGN KEY (api_key_id) REFERENCES hot.api_key(api_key_id) ON DELETE RESTRICT;

-- FK checks bypass RLS. A delivery must not attach an alert to a foreign
-- subscription or destination, including another environment of the same org.
-- Validate existing rows under the named maintenance read scope even when the
-- migration owner is subject to FORCE RLS. Leave inconsistent data for an
-- operator to investigate instead of silently changing its owner.
DO $$
DECLARE
    prior_mode text := COALESCE(current_setting('hot.scope_mode', true), '');
    prior_operation text := COALESCE(current_setting('hot.scope_operation', true), '');
BEGIN
    PERFORM set_config('hot.scope_mode', 'maintenance', true);
    PERFORM set_config('hot.scope_operation', 'alerts', true);
    IF EXISTS (
        SELECT 1 FROM hot.alert_delivery d
        JOIN hot.alert a ON a.alert_id = d.alert_id
        JOIN hot.alert_subscription s ON s.alert_subscription_id = d.subscription_id
        JOIN hot.alert_destination t ON t.alert_destination_id = d.destination_id
        WHERE s.org_id <> a.org_id OR t.org_id <> a.org_id
           OR (s.env_id IS NOT NULL AND s.env_id <> a.env_id)
    ) THEN
        RAISE EXCEPTION 'Existing alert delivery ownership mismatch';
    END IF;
    PERFORM set_config('hot.scope_mode', prior_mode, true);
    PERFORM set_config('hot.scope_operation', prior_operation, true);
END $$;

CREATE FUNCTION hot.enforce_alert_delivery_scope() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM hot.alert a
        JOIN hot.alert_subscription s ON s.alert_subscription_id = NEW.subscription_id
        JOIN hot.alert_destination d ON d.alert_destination_id = NEW.destination_id
        WHERE a.alert_id = NEW.alert_id AND s.org_id = a.org_id AND d.org_id = a.org_id
          AND (s.env_id IS NULL OR s.env_id = a.env_id)
    ) THEN
        RAISE EXCEPTION 'Alert delivery ownership mismatch' USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.alert_delivery
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_alert_delivery_scope();
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.alert_delivery
    FOR EACH ROW WHEN ((OLD.alert_id, OLD.subscription_id, OLD.destination_id) IS DISTINCT FROM (NEW.alert_id, NEW.subscription_id, NEW.destination_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('alert_id', 'subscription_id', 'destination_id');

-- Organizations remain the runtime data owner. Billing and human administration
-- may be inherited from a single root; ownership cannot be reparented.
ALTER TABLE hot.org ADD COLUMN parent_org_id uuid REFERENCES hot.org(org_id) ON DELETE RESTRICT;
ALTER TABLE hot.org ADD COLUMN external_id text;
ALTER TABLE hot.org ADD COLUMN lifecycle_state text NOT NULL DEFAULT 'active'
    CHECK (lifecycle_state IN ('active', 'suspended', 'archived'));
UPDATE hot.org SET active = false WHERE active IS NULL;
UPDATE hot.org SET lifecycle_state = 'suspended' WHERE NOT active;
ALTER TABLE hot.org ALTER COLUMN active SET NOT NULL;
CREATE INDEX idx_org_parent ON hot.org(parent_org_id);
CREATE UNIQUE INDEX idx_org_parent_external ON hot.org(parent_org_id, external_id)
    WHERE parent_org_id IS NOT NULL AND external_id IS NOT NULL;

CREATE FUNCTION hot.enforce_org_hierarchy() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE parent hot.org%ROWTYPE;
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.parent_org_id IS DISTINCT FROM OLD.parent_org_id THEN
        RAISE EXCEPTION 'Organization parent is immutable';
    END IF;
    IF TG_OP = 'INSERT' AND NEW.parent_org_id IS NOT NULL THEN
        IF NEW.parent_org_id = NEW.org_id THEN RAISE EXCEPTION 'Organization cannot parent itself'; END IF;
        SELECT * INTO parent FROM hot.org WHERE org_id = NEW.parent_org_id FOR UPDATE;
        IF NOT FOUND OR parent.parent_org_id IS NOT NULL OR parent.org_type::text <> 'organization'
            OR parent.lifecycle_state <> 'active' THEN
            RAISE EXCEPTION 'Child requires an active organization root';
        END IF;
        IF NEW.org_type::text <> 'organization' THEN RAISE EXCEPTION 'Children must be organizations'; END IF;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        IF OLD.lifecycle_state = 'archived' AND
            (NEW.lifecycle_state = 'active' OR (NEW.active AND NOT OLD.active)) THEN
            RAISE EXCEPTION 'Restore archived organization to suspended before activation';
        END IF;
        IF NEW.lifecycle_state IS DISTINCT FROM OLD.lifecycle_state THEN
            NEW.active := NEW.lifecycle_state = 'active';
        ELSIF NEW.active IS DISTINCT FROM OLD.active THEN
            NEW.lifecycle_state := CASE WHEN NEW.active THEN 'active' ELSE 'suspended' END;
        END IF;
    ELSIF NOT NEW.active AND NEW.lifecycle_state = 'active' THEN
        NEW.lifecycle_state := 'suspended';
    END IF;
    NEW.active := NEW.lifecycle_state = 'active';
    RETURN NEW;
END $$;
CREATE TRIGGER org_hierarchy_guard BEFORE INSERT OR UPDATE ON hot.org
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_org_hierarchy();

CREATE TABLE hot.org_child_provisioning (
    root_org_id uuid NOT NULL REFERENCES hot.org(org_id) ON DELETE RESTRICT,
    idempotency_key text NOT NULL,
    child_org_id uuid NOT NULL REFERENCES hot.org(org_id) ON DELETE RESTRICT,
    env_id uuid NOT NULL REFERENCES hot.env(env_id) ON DELETE RESTRICT,
    request_fingerprint text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(root_org_id, idempotency_key)
);

-- Human grants cannot outlive root-directory removal. Deactivation is also
-- treated as removal, so later reactivation never resurrects child authority.
CREATE FUNCTION hot.revoke_child_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' OR (OLD.active AND NOT NEW.active) THEN
        UPDATE hot.org_user SET active = false, updated_at = now()
        WHERE user_id = OLD.user_id AND org_id IN
            (SELECT org_id FROM hot.org WHERE parent_org_id = OLD.org_id);
    END IF;
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER org_user_revoke_children AFTER DELETE OR UPDATE OF active ON hot.org_user
    FOR EACH ROW EXECUTE FUNCTION hot.revoke_child_grants();

CREATE FUNCTION hot.root_only_org_plan() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM hot.org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL) THEN
        RAISE EXCEPTION 'Child subscriptions are managed by their root';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER org_plan_root_only BEFORE INSERT OR UPDATE OF org_id ON hot.org_plan
    FOR EACH ROW EXECUTE FUNCTION hot.root_only_org_plan();

-- Serialize explicit grants against root-member deletion/deactivation. The row
-- lock makes either the grant precede revocation or fail after it, never revive
-- authority when the same global user is subsequently invited again.
CREATE FUNCTION hot.validate_child_grant() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE root_id uuid;
BEGIN
    SELECT parent_org_id INTO root_id FROM hot.org WHERE org_id = NEW.org_id;
    IF root_id IS NOT NULL AND NEW.active THEN
        PERFORM 1 FROM hot.org_user WHERE org_id = root_id AND user_id = NEW.user_id
            AND active = true FOR UPDATE;
        IF NOT FOUND THEN RAISE EXCEPTION 'Child grant requires an active root membership'; END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER org_user_child_grant BEFORE INSERT OR UPDATE OF active, org_id, user_id ON hot.org_user
    FOR EACH ROW EXECUTE FUNCTION hot.validate_child_grant();
CREATE FUNCTION hot.root_only_invite() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM hot.org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL) THEN
        RAISE EXCEPTION 'Invitations belong to the root member directory';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER invite_root_only BEFORE INSERT OR UPDATE OF org_id ON hot.invite
    FOR EACH ROW EXECUTE FUNCTION hot.root_only_invite();

-- One shared counter per billing account, independent of API replica count.
CREATE TABLE hot.account_request_window (
    root_org_id uuid PRIMARY KEY REFERENCES hot.org(org_id) ON DELETE RESTRICT,
    window_start bigint NOT NULL,
    request_count bigint NOT NULL CHECK (request_count >= 0)
);
CREATE UNIQUE INDEX idx_env_org_name ON hot.env(org_id, name);

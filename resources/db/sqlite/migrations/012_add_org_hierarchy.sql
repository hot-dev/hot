ALTER TABLE org ADD COLUMN parent_org_id blob REFERENCES org(org_id) ON DELETE RESTRICT;
ALTER TABLE org ADD COLUMN external_id text;
ALTER TABLE org ADD COLUMN lifecycle_state text NOT NULL DEFAULT 'active'
    CHECK (lifecycle_state IN ('active', 'suspended', 'archived'));
UPDATE org SET active = 0 WHERE active IS NULL;
UPDATE org SET lifecycle_state = 'suspended' WHERE active = 0;
CREATE INDEX idx_org_parent ON org(parent_org_id);
CREATE UNIQUE INDEX idx_org_parent_external ON org(parent_org_id, external_id)
    WHERE parent_org_id IS NOT NULL AND external_id IS NOT NULL;

CREATE TRIGGER org_parent_immutable BEFORE UPDATE OF parent_org_id ON org
WHEN NEW.parent_org_id IS NOT OLD.parent_org_id
BEGIN SELECT RAISE(ABORT, 'Organization parent is immutable'); END;
CREATE TRIGGER org_parent_valid BEFORE INSERT ON org WHEN NEW.parent_org_id IS NOT NULL
BEGIN
    SELECT CASE WHEN NEW.parent_org_id = NEW.org_id OR NEW.org_type <> 'organization'
        OR NOT EXISTS (SELECT 1 FROM org WHERE org_id = NEW.parent_org_id
            AND parent_org_id IS NULL AND org_type = 'organization' AND lifecycle_state = 'active')
        THEN RAISE(ABORT, 'Child requires an active organization root') END;
END;
CREATE TRIGGER org_no_null_active_insert BEFORE INSERT ON org WHEN NEW.active IS NULL
BEGIN SELECT RAISE(ABORT, 'Organization active is required'); END;
CREATE TRIGGER org_no_null_active_update BEFORE UPDATE OF active ON org WHEN NEW.active IS NULL
BEGIN SELECT RAISE(ABORT, 'Organization active is required'); END;
CREATE TRIGGER org_restore_before_activate BEFORE UPDATE ON org
WHEN OLD.lifecycle_state = 'archived' AND (NEW.lifecycle_state = 'active' OR NEW.active = 1)
BEGIN SELECT RAISE(ABORT, 'Restore archived organization to suspended before activation'); END;
CREATE TRIGGER org_insert_lifecycle AFTER INSERT ON org
WHEN NEW.active <> (NEW.lifecycle_state = 'active')
BEGIN
    UPDATE org SET lifecycle_state = CASE WHEN NEW.active = 0 AND NEW.lifecycle_state = 'active'
        THEN 'suspended' ELSE NEW.lifecycle_state END,
        active = CASE WHEN NEW.active = 0 AND NEW.lifecycle_state = 'active' THEN 0
            ELSE (NEW.lifecycle_state = 'active') END WHERE org_id = NEW.org_id;
END;
CREATE TRIGGER org_lifecycle_projection AFTER UPDATE OF lifecycle_state ON org
WHEN NEW.active <> (NEW.lifecycle_state = 'active')
BEGIN UPDATE org SET active = (NEW.lifecycle_state = 'active') WHERE org_id = NEW.org_id; END;
CREATE TRIGGER org_active_projection AFTER UPDATE OF active ON org
WHEN NEW.lifecycle_state = OLD.lifecycle_state AND NEW.active <> (NEW.lifecycle_state = 'active')
BEGIN UPDATE org SET lifecycle_state = CASE WHEN NEW.active = 1 THEN 'active' ELSE 'suspended' END
    WHERE org_id = NEW.org_id; END;

CREATE TABLE org_child_provisioning (
    root_org_id blob NOT NULL REFERENCES org(org_id) ON DELETE RESTRICT,
    idempotency_key text NOT NULL,
    child_org_id blob NOT NULL REFERENCES org(org_id) ON DELETE RESTRICT,
    env_id blob NOT NULL REFERENCES env(env_id) ON DELETE RESTRICT,
    request_fingerprint text NOT NULL,
    created_at datetime NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY(root_org_id, idempotency_key)
);
CREATE TRIGGER org_user_revoke_children_delete AFTER DELETE ON org_user
BEGIN UPDATE org_user SET active = 0, updated_at = current_timestamp
    WHERE user_id = OLD.user_id AND org_id IN
        (SELECT org_id FROM org WHERE parent_org_id = OLD.org_id); END;
CREATE TRIGGER org_user_revoke_children_update AFTER UPDATE OF active ON org_user
WHEN OLD.active = 1 AND NEW.active = 0
BEGIN UPDATE org_user SET active = 0, updated_at = current_timestamp
    WHERE user_id = OLD.user_id AND org_id IN
        (SELECT org_id FROM org WHERE parent_org_id = OLD.org_id); END;
CREATE TRIGGER org_plan_root_only_insert BEFORE INSERT ON org_plan
WHEN EXISTS (SELECT 1 FROM org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Child subscriptions are managed by their root'); END;
CREATE TRIGGER org_plan_root_only_update BEFORE UPDATE OF org_id ON org_plan
WHEN EXISTS (SELECT 1 FROM org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Child subscriptions are managed by their root'); END;
CREATE TRIGGER org_user_child_grant_insert BEFORE INSERT ON org_user
WHEN NEW.active = 1 AND EXISTS (SELECT 1 FROM org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL)
BEGIN
    SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM org_user ru JOIN org c ON c.parent_org_id = ru.org_id
        WHERE c.org_id = NEW.org_id AND ru.user_id = NEW.user_id AND ru.active = 1)
        THEN RAISE(ABORT, 'Child grant requires an active root membership') END;
END;
CREATE TRIGGER org_user_child_grant_update BEFORE UPDATE OF active, org_id, user_id ON org_user
WHEN NEW.active = 1 AND EXISTS (SELECT 1 FROM org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL)
BEGIN
    SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM org_user ru JOIN org c ON c.parent_org_id = ru.org_id
        WHERE c.org_id = NEW.org_id AND ru.user_id = NEW.user_id AND ru.active = 1)
        THEN RAISE(ABORT, 'Child grant requires an active root membership') END;
END;
CREATE TRIGGER invite_root_only_insert BEFORE INSERT ON invite
WHEN EXISTS (SELECT 1 FROM org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Invitations belong to the root member directory'); END;
CREATE TRIGGER invite_root_only_update BEFORE UPDATE OF org_id ON invite
WHEN EXISTS (SELECT 1 FROM org WHERE org_id = NEW.org_id AND parent_org_id IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Invitations belong to the root member directory'); END;
CREATE TABLE account_request_window (
    root_org_id blob PRIMARY KEY REFERENCES org(org_id) ON DELETE RESTRICT,
    window_start integer NOT NULL,
    request_count integer NOT NULL CHECK (request_count >= 0)
);
CREATE UNIQUE INDEX idx_env_org_name ON env(org_id, name);

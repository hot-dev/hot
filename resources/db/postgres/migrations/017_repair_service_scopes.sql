-- Alert delivery resolves Org, Team and User recipients from the directory.
-- Like its existing org_note access, the Alerts operation is a trusted
-- service scope and reads the directory of every account it delivers for.
CREATE POLICY alert_directory_read ON hot.org_user FOR SELECT USING (hot.scope_maintenance('alerts'));
-- An org's own scope lists its members, deactivated ones included, so they can
-- be reactivated; Billing emails reach the members of its account.
CREATE POLICY service_directory_read ON hot."user" FOR SELECT USING (
    hot.scope_maintenance('alerts') OR EXISTS (
        SELECT 1 FROM hot.org_user ou WHERE ou.user_id = "user".user_id
        AND (hot.scope_org_writes(ou.org_id) OR hot.scope_billing_org(ou.org_id))
    )
);
-- A child org's directory includes the active administrators it inherits from
-- its root.
CREATE POLICY inherited_directory_read ON hot.org_user FOR SELECT USING (
    active AND org_user_role_id = 2 AND EXISTS (
        SELECT 1 FROM hot.org child WHERE child.parent_org_id = org_user.org_id
        AND hot.scope_org_writes(child.org_id)
    )
);
CREATE POLICY inherited_user_read ON hot."user" FOR SELECT USING (
    EXISTS (SELECT 1 FROM hot.org_user ou JOIN hot.org child ON child.parent_org_id = ou.org_id
        WHERE ou.user_id = "user".user_id AND ou.active AND ou.org_user_role_id = 2
        AND hot.scope_org_writes(child.org_id))
);
-- The container worker records an account's compute-unit threshold alerts as
-- notes under Billing, and reads them back to send each alert once.
CREATE POLICY billing_note_read ON hot.org_note FOR SELECT USING (hot.scope_billing_org(org_id));
CREATE POLICY billing_note_insert ON hot.org_note FOR INSERT WITH CHECK (hot.scope_billing_org(org_id));

-- An org-wide subscription may deliver to one of its org's environment
-- channels, as the dashboard has always allowed.
CREATE OR REPLACE FUNCTION hot.scope_subscription_channel_matches(subscription uuid, channel uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM hot.alert_subscription p JOIN hot.alert_channel c ON c.alert_channel_id = channel
        WHERE p.alert_subscription_id = subscription
          AND (c.org_id IS NULL OR c.org_id = p.org_id)
          AND (c.env_id IS NULL OR p.env_id IS NULL OR c.env_id = p.env_id))
$$;

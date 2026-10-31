-- Prefix reads over store keys, continued from 022, which added `key_text`
-- and the trigger that keeps it. This migration takes no table lock that
-- stops reads: the backfill locks the rows it updates, and building the index
-- blocks store writes, not reads, while it runs.

-- Fill in the text of the string keys written before 022, one environment at
-- a time, under the same FORCE RLS contract used at runtime: a non-BYPASS
-- schema owner reads an account's entries in its billing scope, and updates
-- an environment's entries only in that environment's scope. Writing the key
-- back unchanged runs the trigger. Filter by account and environment
-- explicitly: a superuser or BYPASSRLS migration role sees every row in every
-- pass. Policies apply here even after 001's session turned row security off.
SET LOCAL row_security = on;
DO $$
DECLARE m text:=COALESCE(current_setting('hot.scope_mode',true),''); op text:=COALESCE(current_setting('hot.scope_operation',true),'');
    org text:=COALESCE(current_setting('hot.scope_org',true),''); env text:=COALESCE(current_setting('hot.scope_env',true),'');
    roots uuid[]; root_id uuid; orgs uuid[]; envs uuid[];
BEGIN
    PERFORM hot.apply_scope('maintenance','','','[]','[]','scheduler','');
    SELECT COALESCE(array_agg(org_id),ARRAY[]::uuid[]) INTO roots FROM hot.org WHERE parent_org_id IS NULL;
    FOREACH root_id IN ARRAY roots LOOP
        -- Collect the account's environments before the scope changes: the
        -- policies read it row by row.
        PERFORM hot.apply_scope('maintenance',root_id::text,'','[]','[]','billing','');
        SELECT COALESCE(array_agg(s.org_id),ARRAY[]::uuid[]), COALESCE(array_agg(s.env_id),ARRAY[]::uuid[]) INTO orgs, envs
        FROM (SELECT DISTINCT e.org_id, e.env_id FROM hot.store_map_entry e
              WHERE e.key_text IS NULL AND jsonb_typeof(e.key) = 'string'
                AND e.org_id IN (SELECT o.org_id FROM hot.org o WHERE o.org_id=root_id OR o.parent_org_id=root_id)) s;
        FOR i IN 1..COALESCE(array_length(orgs, 1), 0) LOOP
            PERFORM hot.apply_scope('environment',orgs[i]::text,envs[i]::text,'[]','[]','','');
            UPDATE hot.store_map_entry SET key = key
            WHERE org_id = orgs[i] AND env_id = envs[i]
              AND key_text IS NULL AND jsonb_typeof(key) = 'string';
        END LOOP;
    END LOOP;
    PERFORM hot.apply_scope(m,org,env,'[]','[]',op,'');
END $$;

CREATE INDEX idx_store_map_entry_key_prefix ON hot.store_map_entry
    (org_id, env_id, store_name, key_text)
    WHERE key_text IS NOT NULL;

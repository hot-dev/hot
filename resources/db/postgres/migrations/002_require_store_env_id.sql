-- Stores are always environment-scoped. Keep store metadata aligned with
-- store_map_entry and the SQLite backend by rejecting environment-less maps.

-- Earlier in-development builds could create store_map rows with NULL env_id
-- before writes failed against store_map_entry.env_id NOT NULL. If entries
-- exist for that org/name, materialize env-scoped metadata rows from the
-- legacy NULL row before removing the unusable environment-less metadata.
INSERT INTO hot.store_map (
    store_id,
    name,
    org_id,
    env_id,
    embedding_model,
    embedding_dimensions,
    embedding_field,
    text_search,
    created_at
)
SELECT
    gen_random_uuid(),
    m.name,
    m.org_id,
    entries.env_id,
    m.embedding_model,
    m.embedding_dimensions,
    m.embedding_field,
    m.text_search,
    m.created_at
FROM hot.store_map m
JOIN (
    SELECT DISTINCT
        org_id,
        store_name AS name,
        env_id
    FROM hot.store_map_entry
) entries
    ON entries.org_id = m.org_id
    AND entries.name = m.name
WHERE m.env_id IS NULL
ON CONFLICT (org_id, env_id, name) DO NOTHING;

DELETE FROM hot.store_map
WHERE env_id IS NULL;

ALTER TABLE hot.store_map
    ALTER COLUMN env_id SET NOT NULL;

-- Prefix reads over store keys (`::hot::store/list` and `keys` with a
-- `prefix` option). The primary key orders jsonb strings by the database
-- collation, where the keys sharing a prefix need not be one range. A string
-- key's text, ordered by byte, makes a prefix the range [prefix, next string
-- after it).
--
-- The text is a column rather than an index expression on `key`: jsonb's
-- operators are not leakproof, so under forced row-level security PostgreSQL
-- cannot use a condition on them as an index condition, and would filter
-- every key in the store. A plain text comparison is leakproof.
ALTER TABLE hot.store_map_entry ADD COLUMN key_text text COLLATE "C"
    GENERATED ALWAYS AS (CASE WHEN jsonb_typeof(key) = 'string' THEN key #>> '{}' END) STORED;
CREATE INDEX idx_store_map_entry_key_prefix ON hot.store_map_entry
    (org_id, env_id, store_name, key_text)
    WHERE key_text IS NOT NULL;

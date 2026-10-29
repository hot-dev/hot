-- Prefix reads over store keys (`::hot::store/list` and `keys` with a
-- `prefix` option). The primary key orders jsonb strings by the database
-- collation, where the keys sharing a prefix need not be one range. This
-- index orders string keys' text by byte, so a prefix is the range
-- [prefix, next string after it).
CREATE INDEX idx_store_map_entry_key_prefix ON hot.store_map_entry
    (org_id, env_id, store_name, (key #>> '{}') COLLATE "C")
    WHERE jsonb_typeof(key) = 'string';

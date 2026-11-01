-- Prefix reads over store keys (`::hot::store/list` and `keys` with a
-- `prefix` option). PostgreSQL adds a key text column here, and fills it in
-- and indexes it in 023. SQLite needs neither: keys are stored as JSON text,
-- and a JSON string's text starts with its prefix's text exactly when the
-- string starts with the prefix. So a prefix is one range of the unique
-- (org_id, env_id, store_name, key) index, compared byte by byte.
SELECT 1;

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
--
-- A trigger keeps the column, rather than a generation expression, which
-- would rewrite the table: adding a nullable column changes no row. This
-- migration only adds the column and its trigger, so it commits at once and
-- releases the table's lock; 023 fills in the existing entries and builds the
-- index while reads go on.
ALTER TABLE hot.store_map_entry ADD COLUMN key_text text COLLATE "C";

-- A string key's text; null for every other key. It is recomputed whenever
-- the key or the text itself is written, so it never disagrees with the key.
CREATE FUNCTION hot.store_map_entry_key_text() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.key_text := CASE WHEN jsonb_typeof(NEW.key) = 'string' THEN NEW.key #>> '{}' END;
    RETURN NEW;
END $$;
CREATE TRIGGER store_map_entry_key_text BEFORE INSERT OR UPDATE OF key, key_text ON hot.store_map_entry
    FOR EACH ROW EXECUTE FUNCTION hot.store_map_entry_key_text();

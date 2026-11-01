-- Prefix reads over store keys, continued from 022. PostgreSQL fills in the
-- key text column 022 added and indexes it here. SQLite needs neither: it
-- reads a prefix from its unique key, which holds keys as JSON text (see 022).
SELECT 1;

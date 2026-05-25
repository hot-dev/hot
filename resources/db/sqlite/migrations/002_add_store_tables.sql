-- Add ::hot::store metadata + entry tables to the main hot SQLite database.
--
-- These mirror the multi-tenant Postgres `store_map` / `store_map_entry` tables
-- so SqliteStore and PgStore have identical semantics and SqliteStore can use
-- the shared DatabasePool instead of a separate `.hot/store/store.db` file.
--
-- Stores are always environment-scoped, so env_id is required in both metadata
-- and entry tables. This keeps SQLite and Postgres behavior aligned and avoids
-- nullable uniqueness edge cases.

CREATE TABLE store_map (
    store_id blob primary key,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    name text not null,
    embedding_model text,
    embedding_dimensions integer,
    embedding_field text default 'content',
    text_search integer default 0,
    created_at datetime default current_timestamp,
    unique (org_id, env_id, name)
);

CREATE TABLE store_map_entry (
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    store_name text not null,
    key text not null,    -- canonical JSON of the entry key
    value text not null,  -- canonical JSON of the entry value
    seq integer primary key autoincrement,
    embedding blob,
    text_content text,
    size integer not null default 0,
    created_at datetime default current_timestamp,
    updated_at datetime default current_timestamp,
    unique (org_id, env_id, store_name, key)
);

CREATE INDEX idx_store_map_entry_order on store_map_entry(org_id, env_id, store_name, seq);

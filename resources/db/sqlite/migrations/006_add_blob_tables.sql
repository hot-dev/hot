-- Content-addressed blob storage for large Val payloads.
--
-- blob_object holds one row per unique content hash per org/env and points at
-- the raw bytes in file storage (local disk or S3). blob_ref tracks which
-- persisted surfaces (call args, run results, event data, store values, queue
-- payloads) still reference an object, and is the authoritative liveness table
-- for GC. Blob objects are internal storage: they never create `file` rows and
-- never appear in the Files UI.

CREATE TABLE blob_object (
    blob_object_id blob primary key,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    env_id blob references env(env_id) on update restrict on delete restrict,
    hash_alg text not null default 'blake3',
    hash text not null,
    size integer not null,
    content_type text,
    storage_backend text not null,
    storage_path text not null,
    -- pending | available | delete_pending | deleted
    status text not null default 'pending',
    created_at datetime not null default current_timestamp,
    last_referenced_at datetime not null default current_timestamp
);

-- Content-addressed dedupe key. COALESCE avoids NULL-uniqueness surprises for
-- org-scoped objects with no env.
CREATE UNIQUE INDEX idx_blob_object_content_unique
    ON blob_object(org_id, COALESCE(env_id, X''), hash_alg, hash);

CREATE INDEX idx_blob_object_status ON blob_object(status, last_referenced_at);

CREATE TABLE blob_ref (
    blob_ref_id blob primary key,
    blob_object_id blob not null references blob_object(blob_object_id) on update restrict on delete restrict,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    env_id blob references env(env_id) on update restrict on delete restrict,
    -- call_args | call_return | call_flow | run_result | run_failure |
    -- event_data | task_args | task_result | store_value | stream_payload | manual
    source_kind text not null,
    source_id text,
    -- capped JSON array of approximate leaf paths, for diagnostics only
    json_paths text,
    created_by_run_id blob,
    active integer not null default 1,
    created_at datetime not null default current_timestamp,
    expires_at datetime,
    deactivated_at datetime
);

-- One ref per (source, object): repeated occurrences of the same object in one
-- source share a ref row.
CREATE UNIQUE INDEX idx_blob_ref_source_object_unique
    ON blob_ref(org_id, COALESCE(env_id, X''), source_kind, COALESCE(source_id, ''), blob_object_id);

CREATE INDEX idx_blob_ref_object_active ON blob_ref(blob_object_id, active);
CREATE INDEX idx_blob_ref_source ON blob_ref(source_kind, source_id);

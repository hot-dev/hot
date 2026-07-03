-- Content-addressed blob storage for large Val payloads.
--
-- blob_object holds one row per unique content hash per org/env and points at
-- the raw bytes in file storage (local disk or S3). blob_ref tracks which
-- persisted surfaces (call args, run results, event data, store values, queue
-- payloads) still reference an object, and is the authoritative liveness table
-- for GC. Blob objects are internal storage: they never create `file` rows and
-- never appear in the Files UI.

CREATE TABLE hot.blob_object (
    blob_object_id uuid DEFAULT uuidv7() NOT NULL PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    env_id uuid REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    hash_alg text NOT NULL DEFAULT 'blake3',
    hash text NOT NULL,
    size bigint NOT NULL,
    content_type text,
    storage_backend text NOT NULL,
    storage_path text NOT NULL,
    -- pending | available | delete_pending | deleted
    status text NOT NULL DEFAULT 'pending',
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    last_referenced_at timestamp with time zone NOT NULL DEFAULT now()
);

-- Content-addressed dedupe key. COALESCE avoids NULL-uniqueness surprises for
-- org-scoped objects with no env.
CREATE UNIQUE INDEX idx_blob_object_content_unique
    ON hot.blob_object(org_id, COALESCE(env_id, '00000000-0000-0000-0000-000000000000'::uuid), hash_alg, hash);

CREATE INDEX idx_blob_object_status ON hot.blob_object(status, last_referenced_at);

CREATE TABLE hot.blob_ref (
    blob_ref_id uuid DEFAULT uuidv7() NOT NULL PRIMARY KEY,
    blob_object_id uuid NOT NULL REFERENCES hot.blob_object(blob_object_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    org_id uuid NOT NULL REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    env_id uuid REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    -- call_args | call_return | call_flow | run_result | run_failure |
    -- event_data | task_args | task_result | store_value | stream_payload | manual
    source_kind text NOT NULL,
    source_id text,
    -- capped JSON array of approximate leaf paths, for diagnostics only
    json_paths jsonb,
    created_by_run_id uuid,
    active boolean NOT NULL DEFAULT true,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    expires_at timestamp with time zone,
    deactivated_at timestamp with time zone
);

-- One ref per (source, object): repeated occurrences of the same object in one
-- source share a ref row.
CREATE UNIQUE INDEX idx_blob_ref_source_object_unique
    ON hot.blob_ref(org_id, COALESCE(env_id, '00000000-0000-0000-0000-000000000000'::uuid), source_kind, COALESCE(source_id, ''), blob_object_id);

CREATE INDEX idx_blob_ref_object_active ON hot.blob_ref(blob_object_id, active);
CREATE INDEX idx_blob_ref_source ON hot.blob_ref(source_kind, source_id);

-- no-transaction
-- Active refs to spilled call payloads. Call retention deactivates them by org
-- and age, in batches; without this index each batch scans the table.
CREATE INDEX CONCURRENTLY blob_ref_call_active_idx ON hot.blob_ref(org_id, created_at)
    WHERE active = true AND source_kind IN ('call_args', 'call_return', 'call_flow');

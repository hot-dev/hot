-- Active refs to spilled call payloads. Call retention deactivates them by org
-- and age, in batches; without this index each batch scans the table.
CREATE INDEX blob_ref_call_active_idx ON blob_ref(org_id, created_at)
    WHERE active = 1 AND source_kind IN ('call_args', 'call_return', 'call_flow');

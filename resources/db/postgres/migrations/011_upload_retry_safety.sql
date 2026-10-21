-- Preserve completion responses and reserve immutable part content before remote IO.
ALTER TABLE hot.file_upload ADD COLUMN completion_result jsonb;
CREATE TABLE hot.file_upload_part_claim (
    upload_id uuid NOT NULL REFERENCES hot.file_upload(upload_id) ON DELETE CASCADE,
    part_number integer NOT NULL CHECK (part_number BETWEEN 1 AND 10000),
    content_hash text NOT NULL,
    owner uuid NOT NULL,
    lease_until timestamptz NOT NULL,
    PRIMARY KEY (upload_id, part_number)
);

DROP INDEX hot.idx_file_upload_expires;
CREATE INDEX idx_file_upload_expires ON hot.file_upload (expires_at)
    WHERE status IN ('pending', 'completing', 'aborting');

-- Preserve completion responses and reserve immutable part content before remote IO.
ALTER TABLE file_upload ADD COLUMN completion_result text;
CREATE TABLE file_upload_part_claim (
    upload_id blob NOT NULL REFERENCES file_upload(upload_id) ON DELETE CASCADE,
    part_number integer NOT NULL CHECK (part_number BETWEEN 1 AND 10000),
    content_hash text NOT NULL,
    owner blob NOT NULL,
    lease_until datetime NOT NULL,
    PRIMARY KEY (upload_id, part_number)
);

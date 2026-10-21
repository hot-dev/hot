-- Multipart completion bookkeeping. completion_base records the file version
-- a completion was claimed against: the active file's storage_path, '' when
-- the path had no active file, or NULL for uploads claimed before this
-- column existed. The completion only replaces that version, so a retried
-- completion can never revert a write that landed after an earlier attempt
-- committed.
ALTER TABLE file_upload ADD COLUMN completion_base text;

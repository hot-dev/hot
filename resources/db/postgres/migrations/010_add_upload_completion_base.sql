-- Multipart completion bookkeeping. completion_base records the file version
-- a completion was claimed against: the active file's storage_path, '' when
-- the path had no active file, or NULL for uploads claimed before this
-- column existed. The completion only replaces that version, so a retried
-- completion can never revert a write that landed after an earlier attempt
-- committed.
ALTER TABLE hot.file_upload ADD COLUMN completion_base text;

-- Uploads that are 'completing' count toward the pending limits and are
-- swept on expiry like 'pending' ones. Rebuild the partial indexes over both
-- statuses so those queries can still use them.
DROP INDEX hot.idx_file_upload_expires;
CREATE INDEX idx_file_upload_expires ON hot.file_upload USING btree (expires_at) WHERE (status IN ('pending', 'completing'));
DROP INDEX hot.idx_file_upload_status;
CREATE INDEX idx_file_upload_status ON hot.file_upload USING btree (status) WHERE (status IN ('pending', 'completing'));

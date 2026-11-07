-- Quota waits wake on a billing window or account limit change.
ALTER TABLE parked_event ADD COLUMN quota_epoch text;

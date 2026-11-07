-- Quota waits wake on a billing window or account limit change.
ALTER TABLE hot.parked_event ADD COLUMN quota_epoch text;

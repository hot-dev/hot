-- Retry lineage: a retry row records the task it retries in parent_task_id.
-- The partial unique index makes retry creation idempotent at the database
-- level — a crash replay or a second racing writer inserting a retry for the
-- same (parent, attempt) conflicts instead of double-retrying. Infra retries
-- (shutdown drain) preserve the parent's retry_attempt while budget retries
-- increment it, so the two kinds occupy distinct slots by construction and
-- only collide with a duplicate of their own kind.
-- Existing rows keep NULL parents; NULL rows are exempt from the index.
ALTER TABLE task ADD COLUMN parent_task_id blob;
CREATE UNIQUE INDEX idx_task_parent_retry_unique
    ON task(parent_task_id, retry_attempt)
    WHERE parent_task_id IS NOT NULL;

-- no-transaction
-- 037's index serves everything this one did, and every event insert would
-- otherwise maintain both. Dropped concurrently so event writes never wait.
DROP INDEX CONCURRENTLY IF EXISTS hot.idx_event_stream_id;

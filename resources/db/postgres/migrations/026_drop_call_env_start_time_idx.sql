-- no-transaction
-- 025's index serves everything this one did, and every call insert would
-- otherwise maintain both. Dropped concurrently so trace writes never wait.
DROP INDEX CONCURRENTLY IF EXISTS hot.call_env_start_time_idx;

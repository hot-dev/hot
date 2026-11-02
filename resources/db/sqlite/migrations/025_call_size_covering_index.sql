-- Reconciliation sums a whole environment's `size`, which this reads without
-- visiting the table. Replaces call_env_start_time_idx, which 026 drops.
CREATE INDEX call_env_start_time_size_idx ON call(env_id, start_time, size);

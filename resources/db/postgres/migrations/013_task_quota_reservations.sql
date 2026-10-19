-- A running task is its durable account quota reservation. Completion changes
-- status and writes actual metering in one UPDATE, releasing unused capacity.
ALTER TABLE task ADD COLUMN reserved_compute_units bigint NOT NULL DEFAULT 0 CHECK (reserved_compute_units >= 0);
ALTER TABLE task ADD COLUMN reserved_duration_ms bigint NOT NULL DEFAULT 0 CHECK (reserved_duration_ms >= 0);

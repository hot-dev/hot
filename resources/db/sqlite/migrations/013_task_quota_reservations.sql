ALTER TABLE task ADD COLUMN reserved_compute_units integer NOT NULL DEFAULT 0 CHECK (reserved_compute_units >= 0);
ALTER TABLE task ADD COLUMN reserved_duration_ms integer NOT NULL DEFAULT 0 CHECK (reserved_duration_ms >= 0);

-- Retention maintenance prunes admissions older than any billing period can
-- reach. Run counters never decrease when rows go, so deleting history cannot
-- reset run capacity. PostgreSQL also restricts the delete to its retention
-- scope; SQLite has no database RLS, so that check stays in the service layer.
CREATE INDEX run_admission_admitted_at_idx ON run_admission(admitted_at);

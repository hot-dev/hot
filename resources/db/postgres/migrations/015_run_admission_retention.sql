-- Retention maintenance prunes admissions older than any billing period can
-- reach. Runtime scopes still cannot delete admissions, so deleting history
-- cannot reset run capacity, and run counters never decrease when rows go.
CREATE INDEX run_admission_admitted_at_idx ON hot.run_admission(admitted_at);
CREATE POLICY run_admission_retention_read ON hot.run_admission FOR SELECT USING (
    hot.scope_maintenance('retention')
);
CREATE POLICY run_admission_retention_delete ON hot.run_admission FOR DELETE USING (
    hot.scope_maintenance('retention')
);

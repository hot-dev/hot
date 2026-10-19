CREATE TABLE run_admission (
    run_id blob PRIMARY KEY NOT NULL,
    root_org_id blob NOT NULL REFERENCES org(org_id),
    org_id blob NOT NULL REFERENCES org(org_id),
    env_id blob NOT NULL REFERENCES env(env_id),
    admitted_at text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX run_admission_account_period_idx ON run_admission(root_org_id, admitted_at);
INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at)
SELECT r.run_id,COALESCE(o.parent_org_id,o.org_id),o.org_id,e.env_id,COALESCE(r.start_time,CURRENT_TIMESTAMP)
FROM run r JOIN env e ON e.env_id=r.env_id JOIN org o ON o.org_id=e.org_id;
CREATE TRIGGER immutable_run_admission BEFORE UPDATE ON run_admission
BEGIN SELECT RAISE(ABORT, 'Run admission is immutable'); END;

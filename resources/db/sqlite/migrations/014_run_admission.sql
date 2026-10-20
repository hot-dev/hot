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

CREATE TABLE usage_run (
    root_org_id blob NOT NULL, period_start text NOT NULL,
    admitted bigint NOT NULL, PRIMARY KEY(root_org_id,period_start)
);
CREATE TRIGGER count_run_admission AFTER INSERT ON run_admission
BEGIN
    UPDATE usage_run SET admitted=admitted+1
    WHERE root_org_id=NEW.root_org_id AND julianday(period_start)<=julianday(NEW.admitted_at);
END;
CREATE TRIGGER immutable_usage_run BEFORE UPDATE OF root_org_id,period_start ON usage_run
WHEN NEW.root_org_id IS NOT OLD.root_org_id OR NEW.period_start IS NOT OLD.period_start
BEGIN SELECT RAISE(ABORT,'Usage ownership is immutable'); END;

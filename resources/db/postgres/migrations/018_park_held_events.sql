-- Events for a suspended environment or an inactive project wait here instead
-- of cycling through the queue as infrastructure retries. The worker replays
-- them once the environment and project are active again.
CREATE TABLE hot.parked_event (
    message_id uuid PRIMARY KEY,
    env_id uuid NOT NULL REFERENCES hot.env(env_id),
    build_id uuid,
    payload text NOT NULL,
    leased_until timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX parked_event_lease_idx ON hot.parked_event(leased_until);
ALTER TABLE hot.parked_event ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.parked_event FORCE ROW LEVEL SECURITY;
CREATE POLICY parked_queue ON hot.parked_event USING (
    hot.scope_maintenance('queue') OR hot.scope_env_allows(env_id)
) WITH CHECK (
    hot.scope_maintenance('queue') OR hot.scope_env_writes(env_id)
);

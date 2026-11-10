-- no-transaction
CREATE INDEX CONCURRENTLY event_env_event_time_idx ON hot.event(env_id, event_time DESC);

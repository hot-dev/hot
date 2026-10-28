-- Events for a suspended environment or an inactive project wait here instead
-- of cycling through the queue as infrastructure retries. The worker replays
-- them once the environment and project are active again.
CREATE TABLE parked_event (
    message_id BLOB PRIMARY KEY NOT NULL,
    env_id BLOB NOT NULL REFERENCES env(env_id),
    build_id BLOB,
    payload TEXT NOT NULL,
    leased_until TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX parked_event_lease_idx ON parked_event(leased_until);

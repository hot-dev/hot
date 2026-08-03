-- Persist the number of infrastructure retries in a task lineage. Queue
-- delivery counts restart for every newly-enqueued retry message, so they
-- cannot bound shutdown-driven retry generations on their own.
ALTER TABLE task
    ADD COLUMN infra_retry_count integer NOT NULL DEFAULT 0;


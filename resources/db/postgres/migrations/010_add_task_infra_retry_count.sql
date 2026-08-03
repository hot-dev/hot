-- Persist the number of infrastructure retries in a task lineage. Redis
-- delivery counts restart for every newly-enqueued retry message, so they
-- cannot bound shutdown-driven retry generations on their own.
ALTER TABLE hot.task
    ADD COLUMN infra_retry_count smallint NOT NULL DEFAULT 0;


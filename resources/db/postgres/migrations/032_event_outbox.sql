-- A pending enqueue retains the exact queue packet in the same row insert.
-- Clearing the packet after enqueue keeps the durable recovery window small.
ALTER TABLE hot.event ADD COLUMN enqueue_payload jsonb;
ALTER TABLE hot.event ALTER COLUMN enqueue_payload SET COMPRESSION lz4;
CREATE INDEX event_outbox_due_idx ON hot.event(enqueue_leased_until, event_id)
    WHERE enqueue_payload IS NOT NULL;

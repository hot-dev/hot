-- no-transaction
CREATE INDEX CONCURRENTLY event_outbox_due_idx ON hot.event(enqueue_leased_until, event_id)
    WHERE enqueue_payload IS NOT NULL;

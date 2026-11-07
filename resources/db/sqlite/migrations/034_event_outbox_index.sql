CREATE INDEX event_outbox_due_idx ON event(enqueue_leased_until, event_id)
    WHERE enqueue_payload IS NOT NULL;

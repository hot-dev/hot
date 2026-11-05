-- A publisher that names its event leases the event's enqueue. While this is
-- set the enqueue may not have happened, and a resend of the publish takes it
-- over once the lease ends. NULL means nothing is owed, as for every other event.
ALTER TABLE event ADD COLUMN enqueue_leased_until datetime;

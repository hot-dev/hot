-- no-transaction
-- Emails a worker has claimed to send (status 4). The recovery sweep looks for
-- ones left in that state.
CREATE INDEX CONCURRENTLY email_queue_sending_idx ON hot.email_queue(updated_at)
    WHERE status_id = 4;

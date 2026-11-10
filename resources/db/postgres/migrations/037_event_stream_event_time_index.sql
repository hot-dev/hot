-- no-transaction
CREATE INDEX CONCURRENTLY event_stream_event_time_idx ON hot.event(stream_id, event_time DESC);

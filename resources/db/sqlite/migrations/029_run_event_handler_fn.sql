-- The event handler (ns/var) a run executes: retries follow that handler's
-- own `retry` meta and run that handler alone.
ALTER TABLE run ADD COLUMN event_handler_fn text;

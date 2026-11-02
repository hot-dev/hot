-- Index-only scans still visit the heap for pages vacuum hasn't marked all-visible.
-- By default, autovacuum waits until new rows reach 20% of the table. At that
-- point, a reconciliation reads five times as much as it does just after a vacuum.
-- Vacuuming inserted rows only reads the pages they are on, so doing it sooner
-- costs little, and it needs no lock that trace writes wait for.
ALTER TABLE hot.call SET (autovacuum_vacuum_insert_scale_factor = 0.02);

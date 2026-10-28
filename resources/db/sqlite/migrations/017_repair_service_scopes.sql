-- PostgreSQL 017 repairs row-level security policies for the Alerts and
-- Billing service scopes. SQLite has no database RLS (those checks stay in the
-- service layer), so this migration only keeps the two ledgers numbered alike.
SELECT 1;

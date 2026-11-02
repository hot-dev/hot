-- PostgreSQL recounts a new billing period without holding the counter row and
-- counts concurrent task changes in columns 024 adds there. SQLite holds its
-- writer lock across the recount, so no task change can happen during one.
SELECT 1;

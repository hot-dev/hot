-- An email address identifies one account, whatever its capitals. A unique
-- index on the lowered address enforces that, and sign-up, sign-in and OAuth
-- find accounts through it. Only ASCII letters fold: COLLATE "C" gives the same
-- answer under every database locale, as SQLite's lower() and the application's
-- eq_ignore_ascii_case do. https://hot.dev/docs/migrations#upgrading-from-3-0-1-to-3-0-2
-- explains what this means for existing accounts.
--
-- The lowered address is a column rather than an index expression on
-- lower(email): lower() is not leakproof, so under forced row-level security
-- PostgreSQL cannot use a condition on it as an index condition, and would run
-- the row policies over every account at each sign-in. A plain text comparison
-- is leakproof, as 022 found for store keys. The column is generated, so it
-- never disagrees with the address. Adding it rewrites the table, under a lock
-- this migration holds until it commits; both tables here are small.

-- Accounts that already differ only in capitals stay separate: a merge can't
-- tell whether they belong to one person, which password to keep, or which role
-- wins in an organization they share. In each such set the oldest active account
-- keeps the lowered address and answers to any spelling. The others answer only
-- to their exact spelling, and the index leaves them out.
ALTER TABLE hot."user" ADD COLUMN email_exact_only boolean NOT NULL DEFAULT false;
ALTER TABLE hot."user" ADD COLUMN email_lower text COLLATE "C"
    GENERATED ALWAYS AS (CASE WHEN email_exact_only THEN NULL ELSE lower(email COLLATE "C") END) STORED;

-- A plain query here would see only what FORCE RLS shows the schema owner,
-- which is nothing, and the unique index would then fail on the rows it hid.
ALTER TABLE hot."user" NO FORCE ROW LEVEL SECURITY;
UPDATE hot."user" u SET email_exact_only = true
FROM (
    SELECT user_id, row_number() OVER (
        PARTITION BY lower(email COLLATE "C")
        ORDER BY COALESCE(active, false) DESC, created_at NULLS LAST, user_id
    ) AS position
    FROM hot."user"
) ranked
WHERE ranked.user_id = u.user_id AND ranked.position > 1;
ALTER TABLE hot."user" FORCE ROW LEVEL SECURITY;

CREATE UNIQUE INDEX idx_user_email_lower ON hot."user"(email_lower);

-- Sign-up and resend find a pending verification the same way, before anyone
-- has signed in, so the table's policies give the planner nothing else to use.
ALTER TABLE hot.email_verification ADD COLUMN email_lower text COLLATE "C"
    GENERATED ALWAYS AS (lower(email COLLATE "C")) STORED;
CREATE INDEX idx_email_verification_email_lower ON hot.email_verification(email_lower);
DROP INDEX hot.idx_email_verification_email;

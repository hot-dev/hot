-- An email address identifies one account, whatever its capitals. A unique
-- index on the lowered address enforces that, and sign-up, sign-in and OAuth
-- find accounts through it. SQLite's lower() folds only ASCII letters, as
-- PostgreSQL's migration asks for. https://hot.dev/docs/migrations#upgrading-from-3-0-1-to-3-0-2
-- explains what this means for existing accounts.

-- Accounts that already differ only in capitals stay separate: a merge can't
-- tell whether they belong to one person, which password to keep, or which role
-- wins in an organization they share. In each such set the oldest active account
-- keeps the lowered address and answers to any spelling. The others answer only
-- to their exact spelling, and the index leaves them out.
ALTER TABLE user ADD COLUMN email_exact_only integer not null default 0;
ALTER TABLE user ADD COLUMN email_lower text
    GENERATED ALWAYS AS (CASE WHEN email_exact_only THEN NULL ELSE lower(email) END) VIRTUAL;

UPDATE user SET email_exact_only = 1
WHERE user_id IN (
    SELECT user_id FROM (
        SELECT user_id, row_number() OVER (
            PARTITION BY lower(email)
            ORDER BY COALESCE(active, 0) DESC, created_at IS NULL, created_at, user_id
        ) AS position
        FROM user
    )
    WHERE position > 1
);

CREATE UNIQUE INDEX idx_user_email_lower ON user(email_lower);

-- Sign-up and resend find a pending verification the same way.
ALTER TABLE email_verification ADD COLUMN email_lower text
    GENERATED ALWAYS AS (lower(email)) VIRTUAL;
CREATE INDEX idx_email_verification_email_lower ON email_verification(email_lower);
DROP INDEX idx_email_verification_email;

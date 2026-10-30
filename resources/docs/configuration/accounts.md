---
description: "Create the first administrator of a self-hosted Hot installation, choose who can sign up, and recover access without email."
---

# Accounts and Sign-Up

A new self-hosted installation has no users. Create the first administrator
from the command line, then let them invite everyone else from the app.

## First administrator

```bash
hot admin bootstrap --email admin@example.com --name "Admin" \
  --password-file /run/secrets/admin-password
```

The command writes directly to the database configured by `hot.db.uri`, so run
it where the app's database settings are available, after migrations. It
creates the user if needed and lets them sign in with the password in the file.
One trailing newline in the file is ignored, and the password must be at least
eight characters. Running it again changes nothing, even with a different
password, so a deployment can run it on every start.

The administrator then signs in, creates an organization, and invites
teammates from the organization's **Users** page.

## Who can sign up

`app.signup` (`HOT_APP_SIGNUP`) controls who can create an account:

| Value | Meaning |
|-------|---------|
| `open` (default) | Anyone can sign up. The sign-up form sends a verification email. |
| `invite` | Only people with a pending invitation can sign up, through the form or through Google or GitHub sign-in. |

Signing up through the form proves that a person controls their mailbox with a
verification email. A self-hosted installation without an
[email provider](/docs/configuration/email) therefore admits only invited users
through the form, whatever `app.signup` says. Google and GitHub verify email
addresses themselves, so with `open` they still create accounts.

## Without email

An installation can run without an email provider. Then:

- Inviting a user shows the administrator the invitation link instead of
  emailing it. Share it with the invitee, who signs up through it with the
  invited email address. The link expires after seven days.
- Alert email destinations cannot be verified or receive alerts. Use webhook,
  Slack, or PagerDuty destinations instead.

## Reset a password

When a user cannot sign in, an operator sets a new password:

```bash
hot admin reset-password --email user@example.com \
  --password-file /run/secrets/new-password
```

Resetting a password does not sign the user out of browsers that are already
signed in.

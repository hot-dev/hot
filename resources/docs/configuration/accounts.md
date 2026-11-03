---
description: "Create the first administrator of a self-hosted Hot installation, choose who can sign up, set up Google and GitHub sign-in, and recover access without email."
---

# Accounts and Sign-Up

A new self-hosted installation has no users. Create the first administrator
from the command line, then let them invite everyone else from the app.

## Product experience

A deployment sets the app's experience to `self-host`:

| Setting | Environment variable | Default |
|---------|----------------------|---------|
| `product.experience` | `HOT_PRODUCT_EXPERIENCE` | `local-dev` |

The configuration from `hot conf generate app` uses the same default until you set
the variable. `local-dev` is for one person on their own machine. Whatever `HOT_ENV`
says, the app then refuses to create organizations, environments, or teams, or to
invite users. Hot's self-hosted AWS and Google Cloud stacks set `self-host`.

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
| `invite` | Only people with an invitation can sign up, through its link in the form or through Google or GitHub sign-in. |

Hot's self-hosted AWS and Google Cloud stacks set `invite` unless you change it.

Signing up through the form proves that a person controls their mailbox with a
verification email. A self-hosted installation without a working
[email provider](/docs/configuration/email#when-email-is-unavailable) therefore
admits only invited users through the form, whatever `app.signup` says. Outside
Hot Cloud, so does any other installation whose verification email the app
[would refuse to write](/docs/configuration/email#senders-and-links), such as the
default [local development experience](#product-experience) with
`HOT_ENV=production` and no sender of its own. Google and GitHub verify email
addresses themselves, so with `open` they still create accounts for anyone, with
or without email: see
[Sign in with Google or GitHub](#sign-in-with-google-or-github).

### Invitations

Without open sign-up, a new account needs an invitation that can still be
applied: sent to that address, in any letter case, not expired, in an active
organization with a seat free. It is applied when the account is created.

- The form needs the invitation link. The sign-up page without one sends people
  to sign in, even when an invitation is waiting for their address.
- Google and GitHub sign-in work with the link, or without it when an invitation
  is waiting for the address the provider reports.
- In the form, a link to an invitation that cannot be applied reports "This
  invitation has expired or can no longer be used. Ask an administrator to
  invite you again." An expired invitation can be sent again from **Invite
  User**.
- The link overrides a pending verification: the person gets an account with the
  password they just typed, and the earlier verification link stops working.

Emailed links use [`email.app-url`](/docs/configuration/email#senders-and-links).
An installation that sends email sets it, or `HOT_APP_URL`, on the app and the
worker.

### Changing the policy

After a change from `open` to `invite`, verification emails sent earlier can no
longer be confirmed, and resending stops refreshing them, even if the address is
invited later. An invited person signs up through the link in their invitation,
which ends the earlier verification. The exception is a verification that carries
an invitation of its own, from a sign-up that started at that invitation link:
while the invitation can still be applied, confirming it creates the account and
joins that organization. A verification refused this way stays pending, so until
it expires it works again if the installation goes back to `open`.

Turning off the email provider alone does not cancel verifications already sent.
On a self-hosted installation that cannot send email, resending does nothing,
shows the same page, and leaves the link already sent valid.

## Sign in with Google or GitHub

Each installation registers its own client with the provider. Hot has no shared
or hosted credentials to borrow. A provider's button appears on the sign-in and
sign-up pages once both of its client settings have a value, in any experience.
Automatic sign-in applies only with `HOT_ENV=development`, or with `HOT_ENV` unset
on a loopback address, where these pages send visitors to the dashboard instead.

### Create a Google client

1. In a Google Cloud project of your choice, set up the OAuth consent screen, in
   the console's **Google Auth Platform**. **Internal** admits only your Google
   Cloud organization's members, and **External** any Google account. While an
   External app's status is **Testing**, only the test users you list can sign in,
   so publish it to admit others. Hot asks for the `openid`, `email`, and `profile`
   scopes and nothing more.
2. Create an OAuth client of type **Web application**.
3. Under **Authorized redirect URIs**, add the address users reach the app at,
   followed by `/auth/google/callback`:

   ```text
   https://app.example.com/auth/google/callback
   ```

   It must match what Hot sends exactly, or Google answers `redirect_uri_mismatch`.
   The app needs a real public domain name with HTTPS: Google refuses an IP address
   and a name outside the public suffix list.
4. Keep the client ID and client secret for the settings below.

### Create a GitHub client

Under your account's or organization's **Settings > Developer settings > OAuth
apps**, create an OAuth app. Set its **Authorization callback URL** to the address
users reach the app at, followed by `/auth/github/callback`, and generate a client
secret. Hot asks for the `user:email` scope, to read the account's verified
addresses.

### Settings

The app reads these from its environment, not from `hot.hot`. Each also takes the
[`<NAME>_FILE`](/docs/configuration#secrets-from-files) form:

| Variable | Value |
|----------|-------|
| `HOT_GOOGLE_CLIENT_ID`, `HOT_GOOGLE_CLIENT_SECRET` | Google's client |
| `HOT_GITHUB_CLIENT_ID`, `HOT_GITHUB_CLIENT_SECRET` | GitHub's client |
| `HOT_OAUTH_REDIRECT_BASE_URL` | The address users reach the app at, such as `https://app.example.com`, with no trailing slash. Unset or blank, it is `http://localhost:4680` |

A provider needs both of its values. With only one, the app refuses to start, in
local development too, and says which is missing: `hot.dev: APP OAuth configuration
failed: <MISSING> has no value but <PRESENT> does`. An empty or whitespace-only
value counts as unset, so blank on both sides leaves the provider off. A
`<NAME>_FILE` that is empty is its own startup error.

### Who gets in

- **An address that matches an account** signs in to it, with or without an
  invitation, and the provider is linked to the account from then on. That
  includes the first administrator. The comparison is exact, letter case included,
  so create the administrator with the address as the provider reports it.
- **Any other address** gets a new account under `open`. Under `invite` it needs an
  invitation, as in [Invitations](#invitations), or the sign-in fails with "Signing
  up requires an invitation sent to this email address."
- **Only verified addresses count.** Google's sign-in fails on an address Google
  has not verified, and GitHub's considers only the account's verified addresses.
  A GitHub account with several chooses one at its first sign-in, unless an
  invitation it is using names one of them.

### On Hot's self-hosted stacks

Put `HOT_GOOGLE_CLIENT_ID` and `HOT_GOOGLE_CLIENT_SECRET`, or GitHub's pair, in the
operator secret bundle, and run `deploy-now`, a host agent command that the
[`selfhost.py` helper](https://github.com/hot-dev/hot/blob/main/infra/README.md#operating-an-installation) sends. The first time, the release has to be
rendered again to mount them, and `restart-service` is refused until it has been.
To rotate a value later, update it and run `restart-service app`. On a scalable
Google Cloud installation `selfhost.py` sends neither: instead,
[replace the app group's instances](https://github.com/hot-dev/hot/blob/main/infra/README.md#rotating-secrets-and-certificates),
which render and deliver the current secrets. The client ID may be in the
installation's `settings` instead; the secret goes only in the bundle.

The stacks set the redirect address to `https://<app_hostname>`, and their
`settings` refuses `HOT_OAUTH_REDIRECT_BASE_URL`, so the address to register is
`https://<app_hostname>/auth/google/callback` or
`https://<app_hostname>/auth/github/callback`.

A pair with an entry missing or blank, or with both blank, is refused, naming what
is missing, by any deploy before anything stops, by `deploy-now`, and by
`restart-service app`. On a scalable installation, only the app's hosts refuse it
this way, and the migration host refuses a new release before it changes anything.
Other hosts log `sign_in_not_usable` and carry on, and so does a migration host that
is new, so that release stops at the app's hosts. A reboot does not check it: the
installed release starts, and the app stays down on the same problem until the pair
is fixed. Two entries of only spaces instead leave the provider off.

To turn a provider off, remove both entries, then run `deploy-now`, or on a scalable
Google Cloud installation replace the app group's instances. The installed release
still mounts entries you removed, so the app will not start again, after a reboot
say, until `deploy-now` or the replacement of the app group's instances, which render
the release without them, or name the cause if what is left is refused. Blank values
don't turn it off here, and `restart-service app` is refused until the release has
been rendered again without them.

After the browser comes back, the app's host calls the provider itself:
`oauth2.googleapis.com` and `www.googleapis.com` for Google, `github.com` and
`api.github.com` for GitHub. A network the stack created reaches them through NAT.
On an existing network, allow them from the app's hosts. These calls go direct:
`network.egress-proxy` does not carry them.

A real Google sign-in on one of these installations is still on the
[real-account qualification list](https://github.com/hot-dev/hot/blob/main/infra/README.md#real-account-qualification).

## Without email

An installation can run without an email provider. Then:

- Inviting a user shows the administrator the invitation link instead of
  emailing it, at the address the administrator's browser is on. Share it with
  the invitee, who signs up through it with the invited email address. The link
  expires after seven days.
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

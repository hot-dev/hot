---
description: "Send verification, invitation, and alert emails from a self-hosted Hot installation through your own SMTP server."
---

# Email

Hot sends email for account verification, invitations, and alert delivery. The
app writes verification and invitation emails and queues them, and the worker
sends them. The worker also writes and sends alerts. A self-hosted installation
delivers through an SMTP server you choose, such as your mail provider's
submission endpoint or a relay such as Amazon SES or SendGrid.

Without a usable email provider, the worker marks the messages it takes from the
queue failed. Outside Hot Cloud, the app also refuses to write mail with the wrong
sender or links: see [Senders and links](#senders-and-links).

## SMTP

```hot
hot.email.provider "smtp"
hot.email.smtp.host "smtp.example.com"
hot.email.smtp.username "hot@example.com"
hot.email.from "hot@example.com"
hot.email.from-name "Hot"
hot.email.app-url "https://app.example.com"
```

Supply the password from a file rather than writing it into configuration:

```bash
HOT_EMAIL_SMTP_PASSWORD_FILE=/run/secrets/smtp-password hot worker
```

The worker sends the mail, so only it needs the username and password. The app
needs everything else: it reads the provider, the relay's settings, and the
sender to decide whether email works, and it puts the sender and links in each
message it queues.

| Setting | Environment variable | Default | Meaning |
|---------|----------------------|---------|---------|
| `email.provider` | `HOT_EMAIL_PROVIDER` | `none` | `smtp` enables SMTP delivery |
| `email.from` | `HOT_EMAIL_FROM` | (required with `smtp`) | Sender of verification and invitation emails, and of alerts unless `email.alerts.from` is set |
| `email.smtp.host` | `HOT_EMAIL_SMTP_HOST` | (required) | SMTP server hostname; also the name its TLS certificate must match |
| `email.smtp.security` | `HOT_EMAIL_SMTP_SECURITY` | `starttls` | `starttls`, `tls`, or `none` |
| `email.smtp.port` | `HOT_EMAIL_SMTP_PORT` | 587, 465, or 25 | Defaults follow `security` |
| `email.smtp.username` | `HOT_EMAIL_SMTP_USERNAME` | none | Set with `password`, or leave both unset |
| `email.smtp.password` | `HOT_EMAIL_SMTP_PASSWORD` | none | Prefer `HOT_EMAIL_SMTP_PASSWORD_FILE`. Used exactly as given, surrounding spaces included; other settings are trimmed |
| `email.smtp.timeout-seconds` | `HOT_EMAIL_SMTP_TIMEOUT_SECONDS` | 30 | Timeout for each SMTP command |
| `email.smtp.ca-cert` | `HOT_EMAIL_SMTP_CA_CERT` | none | PEM certificates to trust instead of the system's, for a relay with a private certificate authority. Needs `starttls` or `tls` |

Security modes:

- **`starttls`** connects in plaintext and requires the server to upgrade with
  STARTTLS before any credentials or mail are sent. If the server does not
  offer STARTTLS, delivery fails rather than continuing unencrypted.
- **`tls`** encrypts from the first byte (SMTPS, usually port 465).
- **`none`** never encrypts. Use it only for a relay on a trusted local
  network. Hot refuses `none` together with a username and password, or with
  `email.smtp.ca-cert`.

Certificates are verified against the operating system's trust store. To use an
internal relay whose certificate a private certificate authority issued, set
`email.smtp.ca-cert` (or `HOT_EMAIL_SMTP_CA_CERT_FILE`) to that authority's PEM
certificates. They replace the system's roots for this connection, so a public
relay's certificate no longer verifies.

## Senders and links

`email.from` and `email.from-name` set the sender of verification and
invitation emails. Alerts use `email.alerts.from` and
`email.alerts.from-name`, and fall back to `email.from`. Set
`email.alerts.provider` to send alerts through a different provider than
other email; it defaults to `email.provider`. A sender name can hold commas,
`@`, `:`, or quotes: Hot quotes it in the `From` header.

| Setting | Environment variable | Default |
|---------|----------------------|---------|
| `email.from-name` | `HOT_EMAIL_FROM_NAME` | `Hot Dev` |
| `email.alerts.from` | `HOT_EMAIL_ALERTS_FROM` | `email.from` |
| `email.alerts.from-name` | `HOT_EMAIL_ALERTS_FROM_NAME` | `Hot Alerts` |
| `email.alerts.provider` | `HOT_EMAIL_ALERTS_PROVIDER` | `email.provider` |
| `email.app-url` | `HOT_EMAIL_APP_URL` | `HOT_APP_URL` |
| `email.web-url` | `HOT_EMAIL_WEB_URL` | The product's web address |
| `email.docs-url` | `HOT_EMAIL_DOCS_URL` | `email.web-url` with `/docs` |
| `email.logo-url` | `HOT_EMAIL_LOGO_URL` | Hot's logo |

Use a sender address your SMTP server is allowed to send for, and publish
SPF, DKIM, and DMARC records for its domain so recipients accept the mail.

SMTP requires a sender of your own: `email.from` for verification and invitation
emails, and `email.alerts.from` or `email.from` for alerts. Hot refuses one on
`notifications.hot.dev`, which configuration from `hot conf generate` uses until
you set `HOT_EMAIL_FROM`. An alerts sender on that domain falls back to
`email.from`.

Set `HOT_EMAIL_FROM` on both the app and the worker. The app needs it for
verification and invitation emails, because each queued message keeps the sender
the app wrote. The worker needs it for alerts unless `email.alerts.from` is set,
and `hot db redeploy` needs one for its own emails. With it only on the app, the
app reports email as working and its emails go out, but the worker logs `SMTP
email is misconfigured` when it starts and sends no alert emails. On Hot's
self-hosted stacks, `settings` reaches every service, so setting it there covers
both.

Links in these emails use `email.app-url`, which defaults to the app URL
(`HOT_APP_URL`). Set it to the address users reach the app at, on the worker as
well as the app. The app writes the links in verification and invitation
emails, and the worker writes those in alerts.

Through SMTP, Hot refuses a link address that would send people to Hot Cloud's
app (`https://app.hot.dev` or `https://app.hot-stg.dev`) or nowhere. That is what
`HOT_ENV=production` or `staging` falls back to without `HOT_APP_URL`, and what
the configuration from `hot conf generate app` and `worker` writes. Outside
development, Hot also refuses a loopback address, such as the
`http://localhost:4680` that `hot conf generate all` writes. Until these are
fixed, the app treats email as [unavailable](#when-email-is-unavailable). A worker
with such an address still sends verification and invitation emails, with the
links the app wrote. It sends alerts with links that may not open, and logs the
reason when it starts.

Outside Hot Cloud, the app writes no mail at all, whatever its provider setting,
if the mail would come from `notifications.hot.dev` or an invalid sender, or would
carry one of these link addresses. So `HOT_EMAIL_FROM` and `HOT_APP_URL` belong on
the app even when only the worker holds the SMTP settings. The
[local development experience](/docs/configuration/accounts#product-experience)
with `HOT_ENV` unset or `development` skips the sender check. The
`self-host` experience does not, even with `HOT_ENV` unset.

## When email is unavailable

The app counts email as working only when `email.provider` names a provider this
build has (public releases have `smtp`) and the settings every service reads are
usable: the host, security mode, port, timeout, CA certificate, sender, and link
address in [Senders and links](#senders-and-links). With a provider named but
unusable, it logs `hot.dev: APP cannot send email: <reason>` at startup. With
`email.provider` unset or `none`, no startup line says so. Either way, it shows
admins invitation links, and a self-hosted installation admits only invited users
through the sign-up form. See [Accounts and Sign-Up](/docs/configuration/accounts).

The worker reads its settings once and keeps one SMTP sender for app email and
one for alerts for as long as it runs. It logs `SMTP email is misconfigured:
<reason>` when it starts, and marks the mail it cannot send failed. A queued
message whose sender is on `notifications.hot.dev` is refused on each attempt,
retried, and then moved to the dead-letter queue.

An alert email destination created or changed while the app refuses to write its
verification email is still saved, and the page says "Destination saved, but the
verification email could not be queued. Resend it once this installation can send
email." Resending then fails with "Failed to resend verification email." and keeps
any link already delivered. The app logs the reason.

## Self-hosted stacks

On [Hot's self-hosted AWS and Google Cloud stacks](https://github.com/hot-dev/hot/blob/main/infra/README.md),
set the provider, relay, sender, and link settings by their `HOT_*` names in the
installation's `settings`, which every service reads. Put
`HOT_EMAIL_SMTP_USERNAME` and `HOT_EMAIL_SMTP_PASSWORD` in the
operator secret bundle, which only the worker mounts. `HOT_EMAIL_SMTP_CA_CERT`
goes in either one, with the certificates as its value. The bundle refuses any
other email setting, and any name ending in `_FILE`.

A new bundle entry reaches the worker only when the release is rendered again. Until
then the worker keeps sending without it, so a relay that requires credentials
rejects the mail, and `restart-service worker` is refused, naming it. After adding or
removing an entry, run
[`deploy-now`](https://github.com/hot-dev/hot/blob/main/infra/README.md#operating-an-installation),
which renders the release again to mount or unmount it. To rotate a value later,
update it and run `restart-service worker`. On a scalable Google Cloud installation,
where the `selfhost.py` helper sends neither,
[replace the worker group's instances](https://github.com/hot-dev/hot/blob/main/infra/README.md#rotating-secrets-and-certificates)
instead, which render and deliver the current secrets.

A deploy is refused before anything stops for a username without a password or
the reverse, credentials with security `none`, a CA certificate with `none`, or
a CA certificate with no PEM header. The stacks set `HOT_APP_URL` on every
service, so email links need nothing more.

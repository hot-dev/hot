---
description: "Send verification, invitation, and alert emails from a self-hosted Hot installation through your own SMTP server."
---

# Email

Hot sends email for account verification, invitations, and alert delivery. The
worker process sends it; the app only queues it. A self-hosted installation
delivers through an SMTP server you choose, such as your mail provider's
submission endpoint or a relay such as Amazon SES or SendGrid.

Without an email provider, Hot queues these messages and marks them failed.

## SMTP

```hot
hot.email.provider "smtp"
hot.email.smtp.host "smtp.example.com"
hot.email.smtp.username "hot@example.com"
hot.email.from "hot@example.com"
hot.email.from-name "Hot"
```

Supply the password from a file rather than writing it into configuration:

```bash
HOT_EMAIL_SMTP_PASSWORD_FILE=/run/secrets/smtp-password hot worker
```

| Setting | Environment variable | Default | Meaning |
|---------|----------------------|---------|---------|
| `email.provider` | `HOT_EMAIL_PROVIDER` | `none` | `smtp` enables SMTP delivery |
| `email.smtp.host` | `HOT_EMAIL_SMTP_HOST` | (required) | SMTP server hostname; also the name its TLS certificate must match |
| `email.smtp.security` | `HOT_EMAIL_SMTP_SECURITY` | `starttls` | `starttls`, `tls`, or `none` |
| `email.smtp.port` | `HOT_EMAIL_SMTP_PORT` | 587, 465, or 25 | Defaults follow `security` |
| `email.smtp.username` | `HOT_EMAIL_SMTP_USERNAME` | none | Set with `password`, or leave both unset |
| `email.smtp.password` | `HOT_EMAIL_SMTP_PASSWORD` | none | Prefer `HOT_EMAIL_SMTP_PASSWORD_FILE` |
| `email.smtp.timeout-seconds` | `HOT_EMAIL_SMTP_TIMEOUT_SECONDS` | 30 | Timeout for each SMTP command |

Security modes:

- **`starttls`** connects in plaintext and requires the server to upgrade with
  STARTTLS before any credentials or mail are sent. If the server does not
  offer STARTTLS, delivery fails rather than continuing unencrypted.
- **`tls`** encrypts from the first byte (SMTPS, usually port 465).
- **`none`** never encrypts. Use it only for a relay on a trusted local
  network. Hot refuses `none` together with a username and password.

Certificates are verified against the operating system's trust store. Add a
private certificate authority there to use an internal relay with its own
certificate.

## Senders

`email.from` and `email.from-name` set the sender of verification and
invitation emails. Alerts use `email.alerts.from` and
`email.alerts.from-name`, and fall back to `email.from`. Set
`email.alerts.provider` to send alerts through a different provider than
other email; it defaults to `email.provider`.

Use a sender address your SMTP server is allowed to send for, and publish
SPF, DKIM, and DMARC records for its domain so recipients accept the mail.

Links in these emails use `email.app-url`, which defaults to the app URL
(`HOT_APP_URL`). Set it to the address users reach the app at.

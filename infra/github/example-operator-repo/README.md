# Example operator repository

Copy this directory into your own repository to manage a Hot self-host
installation from GitHub Actions. Hot's reusable workflows do the work; this
repository holds only non-secret configuration and three small callers.

```text
.github/workflows/infrastructure.yml   plan on pull requests, apply on merge
.github/workflows/release.yml          deploy a Hot release
.github/workflows/command.yml          status, diagnostics, logs, and other host commands
installation/installation.tfvars.json  the installation (no secrets)
installation/backend_override.tf       where OpenTofu keeps state
installation/.terraform.lock.hcl       optional: your reviewed provider lockfile
```

Pin the `uses:` lines and `hot_version` to the same Hot tag, and change them
together when you upgrade.

## One-time setup

1. **State.** Create a versioned, encrypted bucket for OpenTofu state in an
   account or project you control, and name it in `backend_override.tf`.
   Keep it out of the installation's own stack, so destroying the
   installation can never remove its state.
2. **State encryption.** Generate a long random passphrase, store it as the
   repository secret `HOT_STATE_ENCRYPTION_PASSPHRASE`, and keep a copy
   somewhere you can recover it from. State and saved plans are unreadable
   without it.
3. **Cloud roles.** Create three roles that GitHub's OIDC provider can
   assume, each limited by the token's `sub` claim to this repository and,
   for apply and release, to the `production` environment. No long-lived
   cloud keys are stored in GitHub.

   | Role | Needs |
   |------|-------|
   | plan | Read access to the resources the stack manages, plus state read and lock |
   | apply | Create and change those resources |
   | release | Write the release pointer, send the Hot command, and read the agent's reports |

   On AWS the release role needs only `ssm:PutParameter` on
   `/hot/NAME/release`, `ssm:GetParameter` on `/hot/NAME/status`,
   `ssm:SendCommand` on the `hot-NAME-command` document and on instances
   tagged `HotInstallation=NAME`, and `logs:FilterLogEvents` on the
   `/hot/NAME` log group. The stack grants these when you set
   `release_principal`. It has no shell, Session Manager, or SSH
   permissions. On Google Cloud it needs `roles/secretmanager.secretVersionAdder`
   on `hot-NAME-release`, `roles/pubsub.publisher` on `hot-NAME-commands`, and
   `roles/logging.viewer`.
4. **Environment.** Create a GitHub Environment named `production` with
   required reviewers. Apply and release jobs wait for their approval.

## Operator secrets

After the first apply, put a JSON object in the operator secret the stack
created (output `operator_secret_arn` on AWS, `operator_secret` on Google
Cloud). It needs at least the HTTPS certificate chain and key for both
hostnames:

```json
{
  "TLS_CERTIFICATE": "-----BEGIN CERTIFICATE-----\n...",
  "TLS_PRIVATE_KEY": "-----BEGIN PRIVATE KEY-----\n...",
  "HOT_EMAIL_SMTP_USERNAME": "...",
  "HOT_EMAIL_SMTP_PASSWORD": "..."
}
```

Secrets never go in `installation.tfvars.json`. Non-secret Hot settings do, under
`settings`.

## Deploying a release

Run the `release` workflow with a Hot version. It verifies the release's
signed manifest, points the installation at it, and waits until the host
agent reports the release deployed or failed. The first deployment also
creates the administrator named by `admin_email`; their generated password
is `ADMIN_PASSWORD` in the installation's generated secret.

## Host commands

Run the `command` workflow for `status`, `diagnostics`, a service's `logs`,
a `backup`, `restart-service`, or `drain-and-stop`. It uses the release role,
so it waits for the `production` environment's approval like a release, and
writes the agent's redacted report to the job summary. Anyone who can read
this repository's Actions runs can read those summaries, and logs can contain
user data. The same commands run locally with
`python3 infra/operator/selfhost.py installation/installation.tfvars.json ...`
from a Hot checkout; see [Operating an installation](../../README.md#operating-an-installation).

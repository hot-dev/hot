# Example operator repository

Copy this directory, from the Hot release tag you want to run, into your own
repository to manage a Hot self-host installation from GitHub Actions. Hot's
reusable workflows do the work; this repository holds only non-secret
configuration and three small callers.

```text
.github/workflows/infrastructure.yml   plan on pull requests, apply on merge, check drift daily
.github/workflows/release.yml          deploy a Hot release
.github/workflows/command.yml          status, diagnostics, logs, and other host commands
installation/installation.tfvars.json  the installation (no secrets)
installation/backend_override.tf       where OpenTofu keeps state; the plan workflow fails without it
installation/.terraform.lock.hcl       optional: your reviewed provider lockfile
installation/imports.tf                optional: import blocks to adopt resources after lost state
```

Pin the `uses:` lines and `hot_version` to the same Hot tag, and change them
together when you upgrade. Each tag's callers already pin that tag; copied from
`main`, they can pin a release that is not out yet, and every run fails with
workflow not found. A pull request that changes
`.github/workflows/infrastructure.yml`, as that upgrade does, is planned, and
applied on merge. The workflow can also be run by hand on `main`, where it applies
after approval. The bootstrap's plan role does not trust other branches, so a run by
hand on one fails when it signs in; open a pull request to plan a branch.

## One-time setup

1. **State.** Create a versioned, encrypted bucket for OpenTofu state in an
   account or project you control, and name it in `backend_override.tf`.
   Keep it out of the installation's own stack, so destroying the
   installation can never remove its state. The state bootstrap
   (`infra/opentofu/bootstrap/aws` or `gcp`) can create it and the roles below,
   and `tofu output -raw backend_override` prints the file, with `NAME` to replace.
   The plan workflow fails without that file, with one that configures a local
   backend, and with an `s3` backend that has neither `use_lockfile = true` nor a
   `dynamodb_table`, in an error naming `use_lockfile`. Keep `use_lockfile`, and
   add `kms_key_id` with a KMS state key.
2. **State encryption.** Generate a long random passphrase, store it as the
   repository secret `HOT_STATE_ENCRYPTION_PASSPHRASE`, and keep a copy
   somewhere you can recover it from. State and saved plans are unreadable
   without it.
3. **Cloud roles.** Create three roles that GitHub's OIDC provider can
   assume, each limited to this repository and, for apply and release, to the
   `production` environment on `main`. The state bootstrap pins them to the
   repository's IDs, which it needs as `github_repository_id` and
   `github_repository_owner_id`: `gh api repos/OWNER/REPO --jq '.id, .owner.id'`.
   No long-lived cloud keys are stored in GitHub.

   | Role | Needs |
   |------|-------|
   | plan | Read the resources the stack manages, read state, and take its lock. On AWS, the bootstrap's role is `ReadOnlyAccess` with a deny on data: objects outside the state bucket, logs, command output. On GCP it is `roles/viewer`, which can read logs and objects in existing buckets in the project, though not in a provisioned bucket, a custom role that reads IAM policies and bucket settings, and access to create a missing state object |
   | apply | Create and change those resources, and write state |
   | release | Write the release pointer, send the Hot command, read the agent's reports, and silence the installation's alerts |

   On AWS the apply job asks for a four-hour session
   (`aws_apply_session_seconds`, default 14400). A role allows one hour unless it
   says otherwise, and AWS refuses anything longer, so apply fails at "Authenticate
   to AWS", after the reviewers approve. Raise a hand-made apply role's maximum
   session duration to 14400 seconds, as the state bootstrap's apply and release
   roles do, or set the input to what the role allows. The release role needs more
   than the default hour only when `timeout_minutes` is above 60, and that input
   can be at most 240 on AWS.

   Set `release_principal` in `installation.tfvars.json` to your own release role,
   as the example does: the stack grants the role's permissions only then. On AWS
   it grants `ssm:PutParameter` on `/hot/NAME/release`, `ssm:SendCommand` on the
   `hot-NAME-command` document and instances tagged with the installation's name,
   `logs:FilterLogEvents` on the `/hot/NAME` log group, and reading and silencing its
   CloudWatch alarms. The role has no shell, Session Manager, or SSH permissions. On
   Google Cloud it grants `roles/secretmanager.secretVersionAdder` on
   `hot-NAME-release`, `roles/pubsub.publisher` on `hot-NAME-commands`,
   `roles/logging.viewer`, `roles/monitoring.alertPolicyViewer`, and
   `roles/monitoring.snoozeEditor`. A scalable installation adds its role pointers, in
   place of the one pointer, and listing each group's instances. On Google Cloud that
   includes `roles/secretmanager.viewer` on each pointer, such as
   `hot-NAME-release-migrate`, for when it was last written.
4. **Environment.** Create a GitHub Environment named `production` with
   required reviewers. Apply and release jobs wait for their approval. Limit its
   deployment branches to `main`, turn on "Prevent self-review", and protect `main`
   with required reviews: apply and release run only from `main`, they trust the
   same environment, and the separation between them rests on the review of `main`.

To also plan and apply from a terminal with an identity you assume, such as
an `infra-provisioner` service account or role, see
[Running OpenTofu yourself](../../README.md#running-opentofu-yourself). The
state bootstrap's `operators` and `names` inputs can create that identity,
or let you assume the one GitHub uses.

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
`settings`, such as the SMTP host and sender. A `NAME_FILE` setting is refused: set
`NAME` in `settings`, or in the operator secret if it carries a credential.

To let people sign in with Google or GitHub, register the installation's own client
with the provider and add its entries here; see
[Sign-in with Google or GitHub](../../README.md#sign-in-with-google-or-github).

## Deploying a release

Run the `release` workflow from `main` with a Hot version. It verifies the
release's signed manifest, points the installation at it, and waits until the host
agent reports the release deployed or failed, for up to 30 minutes, or 60 with
`preset: scalable` (`timeout_minutes`). The first deployment also
creates the administrator named by `admin_email`; their generated password
is `ADMIN_PASSWORD` in the installation's generated secret.

## Host commands

Run the `command` workflow from `main` for `status`, `diagnostics`, `logs`,
`backup`, `restart-service`, `drain-and-stop`, `roll-back`, `test-alert`, or
`deploy-now`, which retries a failed deploy or migration by deploying the current
release pointer again. `backup`, `drain-and-stop`, and `roll-back` are for
single-machine installations. The workflow uses the release role, so it waits for the
`production` environment's approval like a release, and writes the agent's redacted report
to the job summary. Anyone who can read this repository's Actions runs can read those
summaries, and logs can contain user data. The same commands run locally with
`python3 infra/operator/selfhost.py installation/installation.tfvars.json ...`
from a Hot checkout; see [Operating an installation](../../README.md#operating-an-installation).

The workflow waits 30 minutes for `deploy-now`, `roll-back`, and `backup`, and 15
for the rest. Set `timeout_minutes` on the caller if a backup of a large local
database takes longer. Every command goes out when its job starts, after any approval,
and waits on the host, where the agent takes them one at a time in about the order
sent. GitHub holds none of them. One sent during a long deploy fails with "did not
answer" if the wait outlasts that limit, and does not run later: send it again, or raise
`timeout_minutes`. A job still waiting for approval has not gone out, so reject one that
should no longer run. See [Operating an installation](../../README.md#operating-an-installation).

The `preset` input is required: it decides which hosts a command waits for and
which commands are refused. Set it to `scalable` on a scalable installation. Left at
`single-machine`, the helper refuses the command before sending it when it can read the
scheduler's pointer. When it cannot, it fails the run after the command is out, and
hosts may still be acting on it, so check `status` before sending it again. A scalable
installation refuses `backup`, `drain-and-stop`, and `roll-back`, on either cloud: it
stops by [resizing its groups](../../README.md#stopping-a-scalable-installation). On a
scalable Google Cloud installation a command reaches only one host, so the helper
sends only `status`, `diagnostics`, and `test-alert`, and refuses the others with what
to do instead.

With private endpoints, a browser or client reaches them through a routed network in
`client_cidrs` or a tunnel; see
[Reaching a private installation](../../README.md#reaching-a-private-installation).

## Drift and durable resources

The daily scheduled run plans with `drift_check: true` and fails if anything
differs from `main`, such as a console edit, or an import or move not yet applied.
Every plan, including drift
checks, fails if it would delete or replace the data disk, a managed
database or cache, a secret, a provisioned bucket, or the log group. Set `allow_data_deletion: true`
on the caller only for a reviewed purge, and remove it afterwards. See
[State, drift, and teardown](../../README.md#state-drift-and-teardown).

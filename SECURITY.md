# Security Policy

We take security seriously. Thank you for helping keep Hot Dev and its users
safe.

## Supported Versions

Security fixes are applied to the latest published `vX.Y.Z` release on the
`stable` branch. Older minor versions are not maintained.

| Version | Supported          |
| ------- | ------------------ |
| Latest  | :white_check_mark: |
| Older   | :x:                |

If you are running from `main`, please reproduce on the latest release before
reporting.

## Reporting a Vulnerability

**Do not open a public GitHub issue, discussion, or pull request for security
problems.** Public reports give attackers a head start.

Please use one of the private channels below:

1. **Preferred:** GitHub's private vulnerability reporting. Go to the
   [Security tab](https://github.com/hot-dev/hot/security/advisories/new) of
   this repository and click "Report a vulnerability". This creates a private
   advisory only maintainers can see.
2. **Email:** `security@hot.dev`. Please encrypt sensitive details if you can.

Include as much of the following as you can:

- A clear description of the issue and its impact.
- Affected version(s) and platform(s).
- Steps to reproduce, or a proof-of-concept.
- Any suggested mitigation or fix.

## What to Expect

- We will acknowledge your report within **3 business days**.
- We will provide an initial assessment, including expected severity, within
  **7 business days**.
- We aim to ship a fix or mitigation within **90 days** of the initial report.
  Some issues may take longer; we will keep you updated.
- Once a fix is released, we will credit reporters in the release notes and the
  associated GitHub Security Advisory unless you prefer to remain anonymous.

## Scope

In scope:

- The Hot language implementation, runtime, and standard library.
- Platform components in this repository: CLI, API, web app/dashboard, event
  worker, scheduler, task worker, `hotbox`, and LSP server.
- Public Hot packages under `hot/pkg`.
- Installer scripts and release artifacts published from this repository.

Out of scope:

- Vulnerabilities in third-party services, providers, or dependencies that are
  not directly exploitable through Hot. Please report those upstream.
- Hot Dev Cloud infrastructure and hosted services. Report those to
  `security@hot.dev` and we will route them appropriately.
- Issues that require an attacker to already have full control of the host.

## Safe Harbor

We will not pursue legal action against researchers who:

- Make a good-faith effort to follow this policy.
- Avoid privacy violations, data destruction, and service disruption.
- Give us reasonable time to fix an issue before public disclosure.

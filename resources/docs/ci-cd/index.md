---
description: "Set up CI/CD for Hot projects with automated checks, tests, deploys, secrets, environments, and release workflows."
---

# CI/CD

Automate testing and deployment of your Hot projects in continuous integration pipelines.

## GitHub Actions

The [`hot-dev/setup-hot`](https://github.com/hot-dev/setup-hot) action installs the Hot CLI on GitHub Actions runners. It handles OS/architecture detection, downloads the correct installer, and verifies the installation.

### Basic Deploy

Deploy to Hot Cloud on every push to `main`:

```yaml
name: Deploy
on:
  push:
    branches: [main]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: hot-dev/setup-hot@v1
      - run: hot deploy
        env:
          HOT_API_KEY: ${{ secrets.HOT_API_KEY }}
```

### Test and Deploy

Run type checking and tests before deploying:

```yaml
name: Test and Deploy
on:
  push:
    branches: [main]

jobs:
  test-and-deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: hot-dev/setup-hot@v1
      - run: hot check
      - run: hot test
      - run: hot deploy
        env:
          HOT_API_KEY: ${{ secrets.HOT_API_KEY }}
```

### Deploy on Release

Deploy only when a GitHub release is published:

```yaml
name: Deploy on Release
on:
  release:
    types: [published]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: hot-dev/setup-hot@v1
      - run: hot deploy
        env:
          HOT_API_KEY: ${{ secrets.HOT_API_KEY }}
```

### Pin a Specific Version

Lock the Hot CLI to a specific version for reproducible builds:

```yaml
- uses: hot-dev/setup-hot@v1
  with:
    version: '1.2.3'
```

### Pass the API Key Through the Action

Instead of setting `HOT_API_KEY` on each step, pass it directly to the action:

```yaml
- uses: hot-dev/setup-hot@v1
  with:
    api-key: ${{ secrets.HOT_API_KEY }}
- run: hot deploy
```

### Action Inputs

| Input | Description | Required | Default |
|-------|-------------|----------|---------|
| `version` | Hot version to install (e.g. `1.2.3`) | No | `latest` |
| `api-key` | Hot API key. Alternative to setting `HOT_API_KEY` env var | No | |

### Supported Runners

| Runner | OS | Architecture |
|--------|-----|-------------|
| `ubuntu-latest` | Linux | x86_64 |
| `ubuntu-24.04-arm` | Linux | arm64 |
| `macos-latest` | macOS | arm64 |
| `macos-13` | macOS | x86_64 |

## Other CI Providers

For CI systems without a dedicated action, install Hot with the shell installer and run commands directly.

### GitLab CI

```yaml
deploy:
  image: ubuntu:latest
  script:
    - curl -fsSL https://get.hot.dev/install.sh | sh
    - hot check
    - hot test
    - hot deploy
  only:
    - main
```

### Generic Script

Any CI environment that runs bash can install and use Hot:

```bash
curl -fsSL https://get.hot.dev/install.sh | sh
hot check
hot test
hot deploy
```

Set `HOT_API_KEY` as a secret/environment variable in your CI provider's settings.

## CI Best Practices

### Run Checks Before Deploying

Use `hot check` and `hot test` as gates before deployment. Add `hot fmt --check` to enforce consistent formatting:

```bash
hot fmt --check    # Fails if files aren't formatted
hot check          # Type checking
hot test           # Run tests
hot deploy         # Only if everything passes
```

### Manage Secrets

Store your `HOT_API_KEY` as an encrypted secret in your CI provider — never commit it to your repository.

- **GitHub Actions** — Add `HOT_API_KEY` under **Settings → Secrets and variables → Actions**
- **GitLab CI** — Add under **Settings → CI/CD → Variables** (mask and protect it)
- **Other providers** — Use the provider's secret/environment variable management

Your `hot.hot` config reads the key automatically:

```hot
hot.remote.hot-dev.key ::env/get("HOT_API_KEY", "")
```

### Pin Versions for Stability

In production pipelines, pin the Hot CLI version to avoid unexpected changes:

```yaml
- uses: hot-dev/setup-hot@v1
  with:
    version: '1.2.3'
```

Update the pinned version intentionally when you're ready to upgrade.

---
description: "Configure Hot projects with environment variables, project settings, dependencies, deploy targets, and local development options."
---

# Hot Configuration

Configure your Hot project using the `hot.hot` configuration file.

## Creating a Configuration File

The easiest way to create a `hot.hot` file is with `hot init`:

```bash
hot init
```

This creates a minimal `hot.hot` file with sensible defaults for local development.

To see all available configuration options, use:

```bash
hot conf generate all
```

You can also generate component-specific templates for self-hosting:

```bash
hot conf generate api        # API server config
hot conf generate app        # App server config
hot conf generate worker     # Worker config
hot conf generate scheduler  # Scheduler config
```

## Overview

The `hot.hot` file is the central configuration for your Hot project. It defines:

- **Projects** - Named project configurations with source paths and dependencies
- **Dependencies** - External packages your project uses
- **Settings** - Global defaults like the active project, profile, and remote
- **Services** - Database, Redis, logging, and other infrastructure settings

## Basic Structure

A minimal `hot.hot` file (created by `hot init`) looks like:

```hot
// hot.hot - Project Configuration File
::hot::conf ns

::env ::hot::env

// Profile and Project Settings
hot.set.profile "local-dev"
hot.set.project "my-app"
hot.set.remote "hot-dev"

// Local Development Profile
hot.profile.local-dev.user.email "dev@example.com"
hot.profile.local-dev.org.slug "dev"
hot.profile.local-dev.env.name "development"

// Remote API (hot.dev)
hot.remote.hot-dev.url ::env/get("HOT_API_URL", "https://api.hot.dev")
hot.remote.hot-dev.key ::env/get("HOT_API_KEY", "")

// Project Configuration
hot.project.my-app.src.paths ["./hot/src"]
hot.project.my-app.test.paths ["./hot/test"]
hot.project.my-app.deps {}
```

That's all you need for local development. Database, logging, and other services use sensible defaults.

For production or advanced configuration, add settings as needed:

```hot
// Database Configuration (defaults to local SQLite)
hot.db.uri ::env/get("HOT_DB_URI", "sqlite:./.hot/db/hot.sqlite.db")

// Logging Configuration
hot.log.level ::env/get("HOT_LOG_LEVEL", "info")
hot.log.target ::env/get("HOT_LOG_TARGET", "stdout")

// Dependencies
hot.project.my-app.deps {
  "hot.dev/anthropic": "0.9.0",
  "hot.dev/openai": "0.9.0"
}
```

Run `hot conf generate all` to see all available options.

## Minimum Version Requirement

Use `hot.min-version` to specify the minimum Hot version required for your project:

```hot
hot.min-version "1.0.0"
```

When set, Hot will check this requirement at startup and display a clear error if the requirement is not met:

```
Version requirement not met: Hot version 1.0.0 is required, but you are running 0.11.0
This project requires Hot 1.0.0 or later.
```

This is useful for:
- **Team coordination** - Ensure all team members are on a compatible version
- **CI/CD** - Fail fast with a clear message before builds
- **Feature requirements** - When your code uses features from a specific Hot version

## Logging Configuration

| Setting | Description | Default |
|---------|-------------|---------|
| `hot.log.level` | Log level: trace, debug, info, warn, error, off | `info` |
| `hot.log.target` | Output target: stdout, file, none | `stdout` |
| `hot.log.dir` | Directory for log files (when target is file) | `.hot/log` |
| `hot.log.rotation` | File rotation: hourly, daily, none | `daily` |
| `hot.log.retention` | Number of log files to keep (0 = keep all) | `7` |

When `log.target` is set to `file`, logs are written to the configured directory with automatic rotation and cleanup based on the retention setting.

## Configuration Format

Hot configuration uses a dotted notation where each setting is a separate assignment:

```hot
// Setting a simple value
hot.log.level "info"

// Setting from environment with default
hot.api.port Int(::env/get("HOT_API_PORT", "4681"))

// Setting a list
hot.project.my-app.src.paths ["./hot/src", "./lib"]

// Setting a map (for dependencies)
hot.project.my-app.deps {
  "hot.dev/anthropic": "0.9.0"
}
```

## Sections

- **[Dependencies](/docs/configuration/dependencies)** - How to declare and manage package dependencies
- **[Projects](/docs/configuration/projects)** - Configuring multiple projects in one workspace

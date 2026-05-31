---
description: "Configure external dependencies for Hot projects, including package dependencies and runtime requirements."
---

# Dependencies

Hot uses a flexible dependency system that supports local paths, Git repositories, and the Hot package registry.

## Dependency Format

Dependencies are declared using the `deps` setting with a map of package coordinates to dependency specifications:

```hot
hot.project.my-app.deps {
  "hot.dev/package-name": "1.0.0"
}
```

### Package Coordinates

Package coordinates use the format `org/package-name`:

- `hot.dev/anthropic` - The Anthropic package from Hot Dev
- `hot.dev/aws-s3` - AWS S3 bindings
- `my-org/my-package` - A custom package

## Dependency Specifications

### Version String (Recommended)

The simplest way to specify a dependency is with a version string:

```hot
hot.project.my-app.deps {
  "hot.dev/anthropic": "1.0.0",
  "hot.dev/openai": "2.1.3"
}
```

This fetches the exact version from the Hot package registry. A version is always required—there is no "latest" resolution.

> **Note:** The `hot.dev` registry currently hosts official Hot packages. Support for publishing your own packages to `pkg.hot.dev` is coming soon! In the meantime, you can share packages via Git repositories (see [Git Dependencies](#git-dependencies) below).

### Specification Object

For more control, use a specification object. The resolver follows this priority:

1. **Local path exists** → Use local
2. **Local path doesn't exist, Git specified** → Clone from Git
3. **Only Git specified** → Clone from Git
4. **Empty spec `{}`** → Resolve from default locations

### Local Dependencies

Point to a local directory:

```hot
hot.project.my-app.deps {
  "hot.dev/my-lib": { "local": "./libs/my-lib" }
}
```

### Git Dependencies

Clone from a Git repository:

```hot
hot.project.my-app.deps {
  "hot.dev/stripe": {
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/stripe",    // Path within the repo
    "tag": "v0.1.0"              // Or use "branch": "main"
  }
}
```

### Local with Git Fallback

The recommended pattern for packages in a monorepo - prefer local during development, fall back to Git for distribution:

```hot
hot.project.my-app.deps {
  "hot.dev/aws-core": {
    "local": "../aws-core",
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/aws-core"
  }
}
```

This means:
- If `../aws-core` exists, use it (great for local development)
- If not, clone from Git (works for published packages)

### Default Resolution

An empty spec `{}` resolves from standard locations:

```hot
hot.project.my-app.deps {
  "hot.dev/anthropic": {}
}
```

Resolution order:
1. `$HOT_HOME/pkg/<package-name>`
2. `./hot/pkg/<package-name>` (development)
3. Executable-relative `resources/pkg/<package-name>`
4. System install paths (`/usr/local/share/hot/pkg/` on macOS)

## Transitive Dependencies

Hot automatically resolves transitive dependencies. If you depend on `aws-s3`, and `aws-s3` depends on `aws-core`, you don't need to declare `aws-core` yourself.

```hot
// You only need to declare aws-s3
hot.project.my-app.deps {
  "hot.dev/aws-s3": { "local": "./hot/pkg/aws-s3" }
}
// aws-core is automatically included via aws-s3's pkg.hot
```

### Project Overrides

Your project-level deps take precedence over transitive deps. This lets you use a local version of a transitive dependency:

```hot
hot.project.my-app.deps {
  "hot.dev/aws-s3": { "local": "./hot/pkg/aws-s3" },
  // Override aws-core to use your local modified version
  "hot.dev/aws-core": { "local": "./my-modified-aws-core" }
}
```

## Dependency Specification Fields

When using a specification object, these fields are available:

| Field | Type | Description |
|-------|------|-------------|
| `local` | String | Local filesystem path (relative or absolute) |
| `git` | String | Git repository URL (HTTPS or SSH) |
| `branch` | String | Git branch name (mutually exclusive with `tag`) |
| `tag` | String | Git tag or commit SHA (mutually exclusive with `branch`) |
| `path` | String | Path within the Git repository (for monorepos) |

## Examples

### Minimal Project (Registry)

```hot
hot.project.my-app.src.paths ["./src"]
hot.project.my-app.deps {
  "hot.dev/anthropic": "1.0.0"
}

hot.set.project "my-app"
```

### Local Development

```hot
hot.project.my-app.src.paths ["./src"]
hot.project.my-app.deps {
  "hot.dev/anthropic": { "local": "./hot/pkg/anthropic" }
}

hot.set.project "my-app"
```

### Full-Featured Project

```hot
// Project settings
hot.set.project "production-api"

// Source and test paths
hot.project.production-api.src.paths ["./src", "./lib"]
hot.project.production-api.test.paths ["./test"]
hot.project.production-api.test.capture true

// Dependencies
hot.project.production-api.deps {
  // Registry packages (recommended for published packages)
  "hot.dev/anthropic": "1.0.0",
  "hot.dev/openai": "2.1.3",

  // Local development packages
  "hot.dev/my-lib": { "local": "./hot/pkg/my-lib" },

  // Git-based package with specific version
  "hot.dev/stripe": {
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/stripe",
    "tag": "v0.2.0"
  },

  // Custom internal package
  "my-org/internal-utils": {
    "git": "git@github.com:my-org/hot-packages.git",
    "path": "packages/internal-utils",
    "branch": "main"
  }
}
```

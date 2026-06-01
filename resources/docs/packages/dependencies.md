---
description: "Declare package dependencies in pkg.hot and understand version, Git, local, transitive, and circular dependency behavior."
---

# Package Dependencies

Declare your package's dependencies on other packages.

## Dependency Format

Package dependencies use the same format as project dependencies:

```hot
deps: {
  "org/package-name": "1.0.0"
}
```

Or with a specification object for more control:

```hot
deps: {
  "org/package-name": { /* spec */ }
}
```

## Dependency Types

### Registry Packages

For published packages, use a version string. A version is always required—there is no "latest" resolution:

```hot
deps: {
  "hot.dev/anthropic": "1.0.0"
}
```

> **Note:** The `hot.dev` registry currently hosts official Hot packages. Support for publishing your own packages to `pkg.hot.dev` is coming soon! In the meantime, share packages via Git repositories.

### Local Sibling Packages

For packages in a monorepo that depend on each other, use local with git fallback:

```hot
deps: {
  "hot.dev/aws-core": {
    "local": "../aws-core",
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/aws-core"
  }
}
```

This pattern:
- Uses `../aws-core` during local development (fast iteration)
- Falls back to Git when the local path doesn't exist (distribution)

### Git-Only Dependencies

For external packages:

```hot
deps: {
  "other-org/their-package": {
    "git": "git@github.com:other-org/hot-packages.git",
    "path": "packages/their-package",
    "tag": "v1.0.0"
  }
}
```

## Resolution Behavior

When your package is used as a dependency:

1. **Project overrides apply** - If the user's `hot.hot` specifies a different source for any of your deps, their spec takes precedence
2. **Transitive resolution** - Your deps are automatically resolved for the user
3. **Local fallback works** - If your `local` path exists relative to where the package is, it's used

### Example: User Perspective

Your package `aws-s3` has this in its `pkg.hot`:

```hot
deps: {
  "hot.dev/aws-core": {
    "local": "../aws-core",
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/aws-core"
  }
}
```

**Scenario 1: User has both packages locally**

```hot
// User's hot.hot
deps: {
  "hot.dev/aws-s3": { "local": "./hot/pkg/aws-s3" },
  "hot.dev/aws-core": { "local": "./hot/pkg/aws-core" }
}
```

Result: Both use local paths (user's override for `aws-core`).

**Scenario 2: User only has aws-s3 locally**

```hot
// User's hot.hot
deps: {
  "hot.dev/aws-s3": { "local": "./hot/pkg/aws-s3" }
}
```

Result: `aws-s3` uses local, `aws-core` resolved via git fallback from `aws-s3`'s deps.

**Scenario 3: User uses git for everything**

```hot
// User's hot.hot
deps: {
  "hot.dev/aws-s3": {
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/aws-s3"
  }
}
```

Result: `aws-s3` cloned from git, `aws-core` also resolved from git.

## Best Practices

### 1. Use Version Strings for Published Packages

```hot
deps: {
  "hot.dev/anthropic": "1.0.0",
  "hot.dev/openai": "2.1.3"
}
```

### 2. Use Local+Git for Monorepo Packages

```hot
deps: {
  "hot.dev/sibling-package": {
    "local": "../sibling-package",
    "git": "git@github.com:your-org/pkg.git",
    "path": "path/to/sibling-package"
  }
}
```

### 3. Keep Dependencies Minimal

Only declare direct dependencies. Don't re-declare transitive dependencies unless you need to override their source.

## Circular Dependencies

Circular dependencies are not allowed. If package A depends on B, and B depends on A, resolution will fail with an error.

Structure your packages to avoid cycles:
- Extract shared code into a `core` package
- Use dependency inversion (depend on interfaces, not implementations)

# Package Manifest

The `pkg.hot` file defines your package's metadata and dependencies.

## Format

```hot
::hot::pkg ns

hot.pkg.<package-name> {
  name: "package-name",
  version: "0.1.0",
  description: "Package description",
  author: "Author Name",
  email: "author@example.com",
  url: "https://github.com/org/package",
  license: "MIT",
  deps: {
    // dependencies
  },
  src-paths: ["src/"],
  test-paths: ["test/"]
}
```

## Fields

### Required Fields

| Field | Type | Description |
|-------|------|-------------|
| `name` | String | Package name (should match directory name) |
| `version` | String | Semantic version (e.g., "1.0.0") |
| `description` | String | Brief description of the package |
| `deps` | Map | Package dependencies |
| `src-paths` | Vec | Directories containing source files |

### Optional Fields

| Field | Type | Description |
|-------|------|-------------|
| `author` | String | Package author name |
| `email` | String | Contact email |
| `url` | String | Package homepage or repository URL |
| `license` | String | License identifier (e.g., "MIT", "Apache-2.0") |
| `org` | String | Organization identifier |
| `tags` | Vec | Broad browse categories for package discovery |
| `test-paths` | Vec | Directories containing test files |
| `hot-min-version` | String | Minimum Hot version required (e.g., "1.0.0") |

## Examples

### Minimal Package

```hot
::hot::pkg ns

hot.pkg.my-utils {
  name: "my-utils",
  version: "0.1.0",
  description: "Utility functions",
  deps: {
    "hot.dev/hot-std": {}
  },
  src-paths: ["src/"]
}
```

### Full Package

```hot
::hot::pkg ns

hot.pkg.aws-s3 {
  name: "aws-s3",
  version: "0.1.0",
  description: "AWS S3 API bindings for Hot",
  author: "Hot Dev",
  email: "support@hot.dev",
  url: "https://hot.dev",
  license: "MIT",
  tags: ["cloud"],
  hot-min-version: "1.0.0",
  deps: {
    "hot.dev/hot-std": {},
    "hot.dev/aws-core": {
      "local": "../aws-core",
      "git": "git@github.com:hot-dev/hot.git",
      "path": "hot/pkg/aws-core"
    }
  },
  src-paths: ["src/"],
  test-paths: ["test/"]
}
```

## Namespace Convention

The package manifest lives in the `::hot::pkg` namespace. The variable name should be `hot.pkg.<name>` where `<name>` matches your package name.

```hot
::hot::pkg ns

hot.pkg.my-package {  // Variable name matches package
  name: "my-package", // name field matches too
  // ...
}
```

## Version Format

Use semantic versioning (SemVer):

- `1.0.0` - Initial stable release
- `1.0.1` - Patch release (bug fixes)
- `1.1.0` - Minor release (new features, backwards compatible)
- `2.0.0` - Major release (breaking changes)

For pre-release versions:
- `0.1.0` - Early development
- `1.0.0-alpha.1` - Alpha release
- `1.0.0-beta.1` - Beta release
- `1.0.0-rc.1` - Release candidate

## Minimum Hot Version

Use `hot-min-version` to specify the minimum Hot version your package requires:

```hot
hot.pkg.my-package {
  name: "my-package",
  version: "1.0.0",
  hot-min-version: "1.0.0",  // Requires Hot 1.0.0 or later
  // ...
}
```

When a user tries to install or use your package with an older Hot version, they'll see:

```
Package 'my-package' requires Hot 1.0.0: Hot version 1.0.0 is required, but you are running 0.11.0
```

This is useful when your package uses language features or standard library functions introduced in a specific Hot version.

## Tags

Use `tags` for broad package directory categories, not every capability, API
method, or keyword a package supports. Package search indexes package names,
descriptions, namespace names, function/type names, and doc summaries, so
detailed terms belong in documentation rather than the top-level category list.

Use lowercase, hyphenated IDs from the approved category set:

- `ai`
- `automation`
- `cloud`
- `database`
- `documents`
- `email`
- `hot`
- `media`
- `messaging`
- `payments`
- `protocols`

Most packages should have one tag. Use two only when both browse paths are
important, such as `["cloud", "email"]` for an AWS email package or
`["ai", "protocols"]` for an MCP package.

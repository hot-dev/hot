---
description: "Publish Hot packages with versioning, naming rules, package docs, metadata, and registry-ready structure."
---

# Publishing Packages

Share your Hot packages with others.

## Distribution Methods

### Git Repository

The most common way to distribute packages is via Git:

1. **Push to a Git repository** (GitHub, GitLab, etc.)
2. **Users add the dependency** with git coordinates:

```hot
deps: {
  "your-org/your-package": {
    "git": "git@github.com:your-org/hot-packages.git",
    "path": "packages/your-package",
    "tag": "v1.0.0"
  }
}
```

### Monorepo Structure

For multiple packages, use a monorepo:

```
hot-packages/
├── packages/
│   ├── package-a/
│   │   ├── pkg.hot
│   │   └── src/
│   ├── package-b/
│   │   ├── pkg.hot
│   │   └── src/
│   └── package-c/
│       ├── pkg.hot
│       └── src/
└── README.md
```

Users reference individual packages via `path`:

```hot
deps: {
  "your-org/package-a": {
    "git": "git@github.com:your-org/hot-packages.git",
    "path": "packages/package-a"
  },
  "your-org/package-b": {
    "git": "git@github.com:your-org/hot-packages.git",
    "path": "packages/package-b"
  }
}
```

## Versioning

### Git Tags

Use Git tags for versioning:

```bash
git tag v1.0.0
git push origin v1.0.0
```

Users can pin to specific versions:

```hot
deps: {
  "your-org/your-package": {
    "git": "...",
    "tag": "v1.0.0"
  }
}
```

### Branch-Based Development

For bleeding edge, users can track a branch:

```hot
deps: {
  "your-org/your-package": {
    "git": "...",
    "branch": "main"
  }
}
```

⚠️ **Warning**: Branch deps are updated on each resolution, which can cause unexpected changes.

## Package Checklist

Before publishing, ensure your package:

- [ ] Has a complete `pkg.hot` with all metadata
- [ ] Has a README.md explaining usage
- [ ] Has working tests (`hot test`)
- [ ] Passes checks (`hot check`)
- [ ] Has appropriate license
- [ ] Has minimal, necessary dependencies
- [ ] Uses semantic versioning

## README Template

Create a `README.md` in your package directory:

```markdown
# Package Name

Brief description of what this package does.

## Installation

Add to your `hot.hot`:

\`\`\`hot
deps: {
  "your-org/your-package": {
    "git": "git@github.com:your-org/hot-packages.git",
    "path": "packages/your-package",
    "tag": "v1.0.0"
  }
}
\`\`\`

## Usage

\`\`\`hot
// Import and use
greet ::your-package/greet

result greet("World")
// => "Hello, World!"
\`\`\`

## API Reference

### `greet(name: Str): Str`

Returns a greeting message.

## License

MIT
```

## Hot Package Registry

The Hot package registry at `pkg.hot.dev` hosts official Hot packages. Use exact version strings to specify dependencies:

```hot
deps: {
  "hot.dev/stripe": "1.0.0"
}
```

> **Note:** Support for publishing your own packages to the registry is coming soon! In the meantime, share packages via Git repositories.

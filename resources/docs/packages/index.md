---
description: "Create reusable Hot packages with namespaces, functions, types, metadata, documentation, and versioned distribution."
---

# Hot Package Creation

Create reusable Hot packages to share code across projects and with the community.

## Overview

A Hot package is a self-contained collection of Hot code with:

- A `pkg.hot` manifest file
- Source files in a `src/` directory
- Optional test files in a `test/` directory
- Dependencies on other packages

## Package Structure

```
my-package/
├── pkg.hot              # Package manifest
├── src/
│   └── my-package/
│       ├── main.hot     # Package code
│       └── utils.hot
└── test/
    └── my-package/
        └── test_main.hot
```

## Sections

- **[Package Manifest](/docs/packages/manifest)** - The `pkg.hot` file format
- **[Package Dependencies](/docs/packages/dependencies)** - Declaring package dependencies
- **[Publishing Packages](/docs/packages/publishing)** - Sharing packages with others

## Quick Start

Create a minimal package:

```bash
mkdir -p my-package/src/my-package
mkdir -p my-package/test
```

Create `my-package/pkg.hot`:

```hot
::hot::pkg ns

hot.pkg.my-package {
  name: "my-package",
  version: "0.1.0",
  description: "My awesome Hot package",
  author: "Your Name",
  email: "you@example.com",
  url: "https://github.com/you/my-package",
  license: "MIT",
  deps: {
    "hot.dev/hot-std": {}
  },
  src-paths: ["src/"],
  test-paths: ["test/"]
}
```

Create `my-package/src/my-package/main.hot`:

```hot
::my-package ns

greet
meta { doc: "Return a greeting message" }
fn (name: Str): Str {
  `Hello, ${name}!`
}
```

Now you can use your package in a project by adding it to deps:

```hot
// In your hot.hot
hot.project.my-app.deps {
  "my-org/my-package": { "local": "./my-package" }
}
```

> **Tip**: Use `hot init` to create a new project with a properly configured `hot.hot` file, then add your package to the deps.

# Projects

A Hot workspace can contain multiple projects, each with its own source paths and dependencies.

## Project Configuration

Project settings use dotted notation:

```hot
// Source paths
hot.project.my-app.src.paths ["./hot/src"]

// Test configuration
hot.project.my-app.test.paths ["./hot/test"]
hot.project.my-app.test.capture true

// Dependencies
hot.project.my-app.deps {
  "hot.dev/anthropic": { "local": "./hot/pkg/anthropic" }
}
```

## Configuration Fields

### src.paths

Defines where your Hot source files are located:

```hot
hot.project.my-app.src.paths ["./hot/src", "./lib"]
```

- List of directories containing `.hot` files
- All paths are relative to the `hot.hot` file location

### test.paths

Directories containing test files:

```hot
hot.project.my-app.test.paths ["./hot/test"]
```

### test.capture

Whether to capture stdout during tests (default: `true`):

```hot
hot.project.my-app.test.capture true
```

### deps

Package dependencies (see [Dependencies](/docs/configuration/dependencies) for full details):

```hot
hot.project.my-app.deps {
  "hot.dev/anthropic": { "local": "./hot/pkg/anthropic" }
}
```

## Multiple Projects

You can define multiple projects in one workspace:

```hot
// Development project with local packages
hot.project.dev.src.paths ["./hot/src"]
hot.project.dev.test.paths ["./hot/test"]
hot.project.dev.deps {
  "hot.dev/stripe": { "local": "./hot/pkg/stripe" }
}

// Production project with pinned versions
hot.project.prod.src.paths ["./hot/src"]
hot.project.prod.test.paths ["./hot/test"]
hot.project.prod.deps {
  "hot.dev/stripe": {
    "git": "git@github.com:hot-dev/hot.git",
    "path": "hot/pkg/stripe",
    "tag": "v1.0.0"
  }
}

// Set default project
hot.set.project "dev"
```

Switch between projects:

```bash
hot run -p prod
hot test -p dev
```

## Default Project

Set the default project with:

```hot
hot.set.project "my-project-name"
```

This project is used when no `-p/--project` flag is specified.

## Store Configuration

`::hot::store` persists data in the main Hot database and is always scoped to
the current organization and environment. Unlike `::hot::file` direct mode,
store access needs a Hot project/runtime context with a migrated database and
an active environment.

The local backend defaults to SQLite. You can select the store backend in
`hot.hot`; `HOT_STORE_TYPE` takes precedence when set:

```hot
hot.store.type ::env/get("HOT_STORE_TYPE", "sqlite") // "sqlite" or "postgres"
```

Store maps can also use embeddings for semantic search. These defaults are used
when a map requests `embedding: EmbeddingOptions.Default`:

```hot
hot.store.embedding.provider ::env/get("HOT_STORE_EMBEDDING_PROVIDER", "local")
hot.store.embedding.model ::env/get("HOT_STORE_EMBEDDING_MODEL", "bge-base-en-v1.5")
hot.store.embedding.field ::env/get("HOT_STORE_EMBEDDING_FIELD", "content")
hot.store.models.path ::env/get("HOT_STORE_MODELS_PATH", ".hot/models")
```

## Project Naming

Project names:
- Must be valid Hot identifiers
- Can contain letters, numbers, and hyphens
- Cannot start with a number
- Are case-sensitive

Good names:
- `my-app`
- `production-api`
- `dev`
- `myProject`

## Directory Structure

Hot lives alongside your existing code. `hot init` adds `hot.hot` to the project root, Hot source files go in `hot/`, and local data (cache, database, logs) goes in `.hot/`:

```
my-project/
├── src/                 # Your existing code (any language)
├── package.json         # Your existing config files
├── hot.hot              # Hot configuration (project root)
├── hot/
│   ├── src/             # Hot source files
│   │   └── my-app/
│   │       └── main.hot
│   ├── test/            # Hot test files
│   │   └── my-app/
│   │       └── test_main.hot
│   └── pkg/             # Local packages
│       ├── anthropic/
│       └── openai/
└── .hot/                # Local data (gitignored)
    ├── cache/
    └── db/
```

# Getting Started

Get up and running with Hot in minutes.

## 1. Install Hot

### macOS / Linux

```bash
curl -fsSL https://get.hot.dev/install.sh | sh
```

### Windows (PowerShell)

```powershell
irm https://get.hot.dev/install.ps1 | iex
```

### Homebrew

```bash
brew install hot-dev/hot/hot
```

---

Verify it works:

```bash
hot version
```

> **Already have Hot?** Update to the latest version with `hot update`. To install a specific release from an older `hot` binary, use `curl -fsSL https://get.hot.dev/install.sh | sh -s -- --version <version>`.

## 2. Install Editor Extension

For syntax highlighting, autocomplete, and diagnostics, install the Hot extension:

- **VS Code** — Install from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=hot-dev.hot), or search for "Hot" by `hot-dev` in the Extensions panel (Cmd+Shift+X / Ctrl+Shift+X)
- **Cursor, Windsurf & other VS Code-compatible editors** — Search for "Hot" by `hot-dev` in the Extensions panel ([also on Open VSX](https://open-vsx.org/extension/hot-dev/hot))

Or install from the command line:

```bash
code --install-extension hot-dev.hot
```

## 3. Add AI Coding Support (Optional)

If you use an AI coding assistant, add Hot language support to help it understand your code:

```bash
hot ai add              # Add AGENTS.md + skills to project
hot ai add --global     # Install skills to ~/.skills/ (available in all projects)
```

This creates an `AGENTS.md` file and a `.skills/hot-language/` directory that teach your AI assistant about Hot syntax and best practices. Works with Cursor, Claude Code, GitHub Copilot, Windsurf, and many other AI coding tools.

## 4. Initialize

Hot is designed to live alongside your existing code. Add it to an existing project or start fresh:

```bash
# Add Hot to an existing project
cd my-app
hot init

# Or start a new project
hot init my-app
cd my-app
```

The project name comes from the directory name. `hot init` adds three things to the directory:

| What | Where | Purpose |
|------|-------|---------|
| `hot.hot` | Project root | Configuration file |
| `hot/` | `hot/src/`, `hot/test/`, `hot/pkg/` | Your Hot code |
| `.hot/` | `.hot/` (gitignored) | Local data — cache, database, logs |

That's it. Your existing files are untouched:

```
my-app/
├── src/                 # Your existing code
├── package.json         # Your existing config
├── hot.hot              # Hot config (project root)
├── hot/                 # Hot code goes here
│   ├── src/
│   │   └── my-app/
│   │       └── hi.hot   # Your first Hot file (it's a tutorial!)
│   └── test/
└── .hot/                # Local data — cache, db, logs (gitignored)
```

The project name (`my-app`) becomes your root namespace. All functions live under `::my-app::*`.

## 5. Start Hot Dev

```bash
hot dev
```

This starts the development server with:
- **App** at [http://localhost:4680](http://localhost:4680) — monitor runs, events, and streams
- **Scheduler** — executes your scheduled functions
- **Worker** — processes events

Open the app and watch things happen! The `hi.hot` file includes a scheduled function that runs every minute.

> **Tip:** Use `hot dev --open` to automatically open the dashboard in your browser. You can also set `hot.dev.open true` in your `hot.hot` config to always open on start.

## 6. Run Some Code

Open another terminal and try:

```bash
# Call a function
hot eval '::my-app::hi/hello()'

# Call with arguments
hot eval '::my-app::hi/check-heat(42)'

# This one fails! (7 is divisible by 7)
hot eval '::my-app::hi/check-heat(7)'

# Trigger an event (hi.hot has a handler for this)
hot eval 'send("hot-take", {num: 42})'
```

Check the dashboard to see your runs and events.

## 7. Learn from hi.hot

Open `hot/src/my-app/hi.hot` in your editor—it's a complete tutorial covering:

- **Functions** — basic definitions, arguments, and types
- **Flows** — `cond`, `parallel`, and pipes (`|>`)
- **Result handling** — using `match` on `Result.Ok` and `Result.Err`
- **Schedules** — functions that run on a timer
- **Events** — handlers that respond to events

Edit the file and Hot Dev reloads automatically. Experiment!

## Deploy to Hot Cloud

When you're ready to go live, deploy to Hot Cloud with a single command.

### Get a Hot API Key

1. Go to [hot.dev](https://hot.dev) and sign in (or create an account)
2. Navigate to **Settings → API Keys**
3. Click **Create API Key** and copy the key
4. Set your API key in your environment:

```bash
export HOT_API_KEY=your-api-key
```

Or store it in a `.env` file in your project root.

### Deploy

```bash
hot deploy
```

This builds your project and deploys it to Hot Cloud. Your workflows, schedules, and event handlers will now run in production.

> **Automate it:** Use the [Hot GitHub Action](/docs/ci-cd) to deploy on every push to `main`.

## Next Steps

- **[Hot Chat demo](/docs/demos/hot-chat)** — run a complete AI chat product on Hot in 15 minutes
- **[Hot Language](/docs/language)** — dive deeper into syntax and concepts
- **[Standard Library](/pkg/hot-std)** — explore available functions
- **[Events & Handlers](/docs/events)** — build event-driven workflows
- **[MCP Services](/docs/mcp)** — expose functions as tools for AI agents
- **[Webhooks](/docs/webhooks)** — receive HTTP requests from external services
- **[CI/CD](/docs/ci-cd)** — automate testing and deployment with GitHub Actions

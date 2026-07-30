---
description: "Teach AI coding assistants the Hot language and agent SDKs with hot ai add, AGENTS.md, bundled skills, and LLM-readable documentation."
---

# AI Coding Assistants

Hot ships coding-agent context because conventional language assumptions do
not apply to Hot syntax or execution semantics. Install the bundled
instructions before asking an AI assistant to create or change Hot code.

This guide is about **coding with AI assistants**. To build an AI agent as part
of your product, see [Agents](/docs/agents).

## Install Project Context

From the root of a Hot project:

```bash
hot ai add
```

This installs:

- `AGENTS.md` — concise repository instructions and critical Hot syntax rules
- `.skills/hot-language/` — detailed language guidance, references, and examples
- `.skills/hot-ai-agents/` — agent, model, tool, memory, and AI SDK integration guidance

Project-local context is the best default. It travels with the repository and
lets the team review changes to the instructions alongside the code.

## Install Skills Globally

To make the bundled Hot skills available across projects:

```bash
hot ai add --global
```

Global skills are installed under `~/.skills/`. The exact skill set can vary by
Hot release.

Inspect what is installed:

```bash
hot ai list
```

Refresh installed files after upgrading Hot:

```bash
hot ai update
```

`hot ai add` and `hot ai update` use snapshots bundled with the installed Hot
release, so they work offline and stay version-aligned with the compiler.

## Install the Latest Public Skills

The public skills are also published from
[hot-dev/hot-skills](https://github.com/hot-dev/hot-skills):

```bash
npx skills add hot-dev/hot-skills
```

Use the bundled `hot ai add` path when release alignment and offline
installation matter. Use the public skills source when you intentionally want
the newest published guidance.

## Give the Assistant a Clear Task

Good coding-agent requests name the workflow behavior, existing namespace, and
validation command:

```text
Use the Hot language skill for this task.

Add an event handler in ::billing that listens for invoice:overdue,
posts a Slack notification, and retries transient failures three times.
Preserve the existing event payload shape. Run hot check and the relevant
Hot tests when finished.
```

For agent or model integrations, also ask the assistant to use the
`hot-ai-agents` skill:

```text
Use the hot-language and hot-ai-agents skills.

Add a support agent with project-specific memory and one permission-scoped
MCP tool. Follow the existing namespace and package patterns. Do not invent
package APIs; inspect the installed package docs first.
```

The explicit skill request is useful in tools that load skills on demand.
`AGENTS.md` remains passive repository context for tools that support the
standard.

## Validate Generated Hot Code

AI-generated Hot code should be treated like any other code change:

```bash
hot check
hot test
```

When working inside this repository, use the project-specific commands in its
`AGENTS.md`. Some Hot repositories run tests through Cargo:

```bash
cargo run test
```

Review generated code for the Hot-specific mistakes that general-purpose
models make most often:

- Assignment uses `name value`, not `name = value`.
- Arithmetic and comparison use functions such as `add` and `eq`, not infix
  operators.
- Conditional flows use `if(...)`, `cond`, or `match`, not conventional
  `if`/`else` blocks.
- Ordinary function bodies are serial; only explicit parallel flows run
  independent bindings concurrently.
- Expected failures use `Result.Err` patterns; `fail(...)` is for broken
  invariants.
- Event handlers with external side effects account for at-least-once delivery.
- Namespaced package functions are inspected rather than guessed.

The bundled language skill contains the complete rules and examples.

## LLM-Readable Documentation

Hot publishes two plain-text documentation resources:

- [`https://hot.dev/llms.txt`](https://hot.dev/llms.txt) — compact
  documentation index with page descriptions
- [`https://hot.dev/llms-full.txt`](https://hot.dev/llms-full.txt) — the full
  user documentation in navigation order

Use the compact index for tools that can retrieve individual pages. Use the
full document when a tool needs a single context artifact and its context
window can hold the content.

The website documentation remains the source of truth for current public
behavior. Bundled skills focus on the rules and workflows a coding agent needs
to make correct changes.

## Keep Secrets Out of Agent Context

Do not paste production API keys, model keys, customer data, `.env` contents,
or private run payloads into an assistant unless the selected tool and
environment are explicitly approved for that data.

Use placeholders in prompts and examples:

```bash
export HOT_API_KEY=your-api-key
export ANTHROPIC_API_KEY=your-provider-key
```

Store real values through your normal secret-management path. See
[Hot Configuration](/docs/configuration) and
[Authentication](/docs/authentication).

## Troubleshooting

### The assistant writes conventional syntax

Confirm `AGENTS.md` is visible from the working directory and explicitly ask
the assistant to load the `hot-language` skill.

```bash
hot ai list
```

### Instructions are from an older Hot release

Upgrade Hot, then refresh installed AI support:

```bash
hot update
hot ai update
```

### A package API is invented

Direct the assistant to the installed [Hot package docs](/pkg) and ask it to
verify the exact namespace, function, type, and context requirements before
editing code.

### The skill is unavailable in a specific tool

Keep `AGENTS.md` in the repository and provide the relevant page from
`llms.txt` or `llms-full.txt` as context. Skill discovery differs between
coding tools, while plain repository instructions and Markdown remain widely
usable.

## Next Steps

- [Getting Started](/docs/getting-started)
- [Hot Language](/docs/language)
- [Hot CLI](/docs/cli)
- [Agents](/docs/agents)
- [Hot Packages](/pkg)

---
description: "Use the Hot VS Code extension and language server for syntax highlighting, diagnostics, navigation, and editor feedback."
---

# VS Code & LSP

Hot provides first-class editor support through a Language Server Protocol (LSP) implementation and VS Code extension.

## VS Code Extension

The Hot VS Code extension provides a complete development experience:

- **Syntax highlighting** — Full Hot language support
- **Diagnostics** — The same errors and warnings `hot check` reports, as you type
- **Autocomplete** — Namespaces, functions, types, enum variants, record fields and local variables
- **Hover information** — Signatures, documentation and type info
- **Go to definition and find references** — Navigate functions, types and local variables
- **Formatting** — Format a document with `hot fmt`

### Installation

Install from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=hot-dev.hot):

1. Open VS Code
2. Go to Extensions (Cmd+Shift+X / Ctrl+Shift+X)
3. Search for "Hot" and choose the `Hot` extension from `hot-dev`
4. Click Install

Or install from the command line:

```bash
code --install-extension hot-dev.hot
```

**Cursor, Windsurf & other VS Code-compatible editors:** Search for "Hot" by `hot-dev` in the Extensions panel ([also on Open VSX](https://open-vsx.org/extension/hot-dev/hot)).

### Features

#### Syntax Highlighting

Hot files (`.hot`) are automatically recognized with full syntax highlighting:

- Declaration keywords (`fn`, `type`, `enum`, `ns`)
- Control keywords (`lazy`, `do`)
- Flow keywords (`cond`, `parallel`, `serial`, `match`, `cond-all`, `match-all`)
- Special syntax (`|>` pipe, `=>` branch, `->` coercion, `|` union)
- Metadata annotations (`meta`)
- Strings, block strings, template strings, and block template strings
- Numbers and booleans
- Comments
- Namespaces and function paths

#### Diagnostics

The language server runs the same check as `hot check --check.format json`
over the whole project, with your unsaved changes, shortly after you stop
typing:

- Syntax errors
- Undefined functions, variables and types
- Type and arity mismatches
- Invalid `meta` values, such as schedules, timeouts, event handlers and box sizes
- Warnings, such as deprecated functions and missing context values

Diagnostics cover every file in the project's source and test paths, not
just the open ones, and an edit to one file updates the diagnostics of the
files that use it. Problems with `hot.hot` itself, or project errors that
belong to no source file, appear on `hot.hot`.

Errors appear as red squiggles in the editor and in the Problems panel (Cmd+Shift+M).

#### IntelliSense

Get intelligent code completion as you type:

- Namespace paths after `::`, and the functions and types of a namespace after `::ns/`
- Local variables and parameters, the current namespace's definitions, and core library functions
- Enum variants after `Type.`
- Record fields after `value.` when the value's type is known, and inside `Type({ ... })`

Press `Ctrl+Space` to trigger autocomplete manually.

#### Go to Definition

Jump to where a function, type or local variable is defined:

- `F12` or `Cmd+Click` — Go to definition
- `Cmd+Shift+F12` — Peek definition (inline)
- `Shift+F12` — Find all references, across every file in the project

#### Formatting

**Format Document** (`Shift+Alt+F`, or format on save) runs the same
formatter as `hot fmt`. A file the formatter can't format safely is left
unchanged.

### Configuration

Configure the extension in VS Code settings:

```json
{
  "hot.lsp.enabled": true,
  "hot.lsp.commandPath": "/usr/local/bin/hot",
  "hot.lsp.extraArgs": []
}
```

| Setting | Default | Description |
|---------|---------|-------------|
| `hot.lsp.enabled` | `true` | Enable the Language Server (requires Hot CLI) |
| `hot.lsp.commandPath` | `hot` | Path to Hot CLI executable |
| `hot.lsp.extraArgs` | `[]` | Additional LSP server arguments |

## Language Server Protocol

The Hot LSP can be used with any editor that supports LSP.

### Starting the LSP Server

```bash
hot lsp
```

The server communicates over stdin/stdout using JSON-RPC. It loads the
project from the workspace folder (or root URI) the editor sends, so it does
not need to start in the project directory; with no `hot.hot` there, opening
a file inside a project loads that project. Positions use UTF-16 columns,
the LSP default.

### Neovim Setup

Add to your Neovim configuration:

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')

if not configs.hot then
  configs.hot = {
    default_config = {
      cmd = { 'hot', 'lsp' },
      filetypes = { 'hot' },
      root_dir = lspconfig.util.root_pattern('hot.hot', '.git'),
    },
  }
end

lspconfig.hot.setup{}
```

### Emacs Setup

First, define a major mode for `.hot` files:

```elisp
(define-derived-mode hot-mode prog-mode "Hot"
  "Major mode for Hot language files.")
(add-to-list 'auto-mode-alist '("\\.hot\\'" . hot-mode))
```

Then configure `lsp-mode`:

```elisp
(use-package lsp-mode
  :hook (hot-mode . lsp)
  :config
  (add-to-list 'lsp-language-id-configuration '(hot-mode . "hot"))
  (lsp-register-client
    (make-lsp-client
      :new-connection (lsp-stdio-connection '("hot" "lsp"))
      :major-modes '(hot-mode)
      :server-id 'hot-lsp)))
```

### Supported Capabilities

| Capability | Supported |
|------------|-----------|
| `textDocument/completion` | ✅ |
| `textDocument/hover` | ✅ |
| `textDocument/definition` | ✅ |
| `textDocument/references` | ✅ |
| `textDocument/formatting` | ✅ |
| `textDocument/publishDiagnostics` | ✅ |
| `textDocument/signatureHelp` | ✅ |
| `workspace/symbol` | ✅ |
| `workspace/didChangeWatchedFiles` | ✅ |
| `textDocument/rename` | ❌ Not yet supported |
| `textDocument/rangeFormatting` | ❌ Not yet supported |
| `textDocument/documentSymbol` | ❌ Not yet supported |
| `textDocument/codeAction` | ❌ Not yet supported |

## Commands

The VS Code extension provides these commands:

| Command | Description |
|---------|-------------|
| `Hot: Start Analyzer` | Start the LSP server |
| `Hot: Stop Analyzer` | Stop the LSP server |
| `Hot: Restart Analyzer` | Restart the LSP server |
| `Hot: Show Logs` | Open the output channel |
| `Hot: Create AI Hints` | Generate AI assistant hints |

## REPL Integration

The Hot REPL can be used alongside your editor for interactive development:

```bash
hot repl
```

Features:
- Tab completion
- History (up/down arrows)
- Multi-line input
- Pretty-printed output

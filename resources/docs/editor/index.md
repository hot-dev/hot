# VS Code & LSP

Hot provides first-class editor support through a Language Server Protocol (LSP) implementation and VS Code extension.

## VS Code Extension

The Hot VS Code extension provides a complete development experience:

- **Syntax highlighting** — Full Hot language support
- **Diagnostics** — Real-time error checking and warnings
- **Autocomplete** — Function, type, and namespace completion
- **Hover information** — Type info and documentation
- **Go to definition** — Navigate to function and type definitions

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

See errors and warnings in real-time as you type:

- Type mismatches
- Undefined functions or variables
- Syntax errors
- Unused variables (warnings)

Errors appear as red squiggles in the editor and in the Problems panel (Cmd+Shift+M).

#### IntelliSense

Get intelligent code completion as you type:

- Function names with signatures
- Type names and fields
- Namespace paths
- Local variables
- Core library functions

Press `Ctrl+Space` to trigger autocomplete manually.

#### Go to Definition

Jump to where a function or type is defined:

- `F12` or `Cmd+Click` — Go to definition
- `Cmd+Shift+F12` — Peek definition (inline)
- `Shift+F12` — Find all references

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

The server communicates over stdin/stdout using JSON-RPC.

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
| `textDocument/rename` | ✅ |
| `workspace/symbol` | ✅ |

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

---
description: "Use the Hot VS Code extension and language server for syntax highlighting, diagnostics, navigation, rename, and editor feedback."
---

# VS Code & LSP

Hot provides first-class editor support through a Language Server Protocol (LSP) implementation and VS Code extension.

## VS Code Extension

The Hot VS Code extension provides a complete development experience:

- **Syntax highlighting**: Full Hot language support, with semantic highlighting from the language server
- **Diagnostics**: The same errors and warnings `hot check` reports, as you type
- **Autocomplete**: Namespaces, functions, types, enum variants, record fields and local variables
- **Hover information**: Signatures, documentation and type info
- **Go to definition and find references**: Navigate functions, types and local variables
- **Outline**: The namespaces, functions, types and bindings of a file
- **Rename**: Rename a local, a parameter, a function or a type across the project
- **Formatting**: Format a document with `hot fmt`

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

- Declaration keywords (`fn`, `type`, `enum`, `ns`, and `open` after `type` or `enum`)
- The constraint keyword (`where`)
- Control keywords (`lazy`, `do`)
- Flow keywords (`cond`, `parallel`, `serial`, `match`, `cond-all`, `match-all`)
- Special syntax (`|>` pipe, `=>` branch, `->` coercion, `|` union)
- Metadata annotations (`meta`)
- Strings, block strings, template strings, and block template strings
- Numbers and booleans
- Comments
- Namespaces and function paths

The language server adds **semantic highlighting**. It knows what each name
is, which a grammar can't:

- Parameters, local bindings, functions, types, enums and enum variants
- Fields, record keys and named-argument labels, as in `Point(x: 1)`
- Namespace segments, as in `::hot::coll`
- `where`, `meta`, `ns` and `open`, only where they act as keywords
- `%`, and `%.amount` in a precondition as the parameter it names
- Names inside template expressions (`${...}`)
- Standard library functions and types, marked `defaultLibrary`

A name that doesn't resolve keeps the grammar's color. VS Code shows semantic
highlighting when the color theme enables it, which the built-in themes do.
To turn it on for any theme, set `editor.semanticHighlighting.enabled` to
`true` under `[hot]` in your settings.

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

A precondition's `%.amount` is the parameter `amount`: it is found as a
reference, and it jumps to the parameter.

#### Outline

The Outline view, the breadcrumbs and **Go to Symbol in Editor**
(`Cmd+Shift+O`) list what a file declares:

- Each namespace, with its declarations inside it
- Functions. A function written with several arities lists each one, and a test is marked `test`
- Types, with their fields, enum variants and custom constructors
- Top-level bindings, coercions (`Date -> Str`) and enrollments (`Bird -> Animal.Bird`)

The outline is read from the text as you type. It doesn't wait for the file
to parse.

#### Rename

**Rename Symbol** (`F2`) renames a name everywhere it is used:

- A local binding, a parameter, a lambda parameter or a match arm binding, within its function. A parameter's `%.name` in a precondition is renamed with it.
- A function, a type or a top-level binding your project defines, in every file of the project. This includes uses through an alias (`::util/name`) and imports.
- A punned map entry keeps its key. Renaming `name` to `title` turns `{name, email}` into `{name: title, email}`.

The server refuses a rename it can't make safely, and says why:

- A field, a named argument's label, an enum variant or a branch name. Their uses on values of unknown type can't all be found.
- A binding in a `parallel` block, or in a flow declared `All`. Its name is a key of the result.
- A name the standard library or a dependency defines, and an import written under another name.
- A new name that isn't one identifier, or that already means something where the old name is declared or used.
- Any rename while a file has a syntax error.

Before it answers, the server checks the project with the edits applied. A
rename that would add an error or a warning is not sent.

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
  "hot.lsp.extraArgs": [],
  "hot.checkForUpdates": true
}
```

| Setting | Default | Description |
|---------|---------|-------------|
| `hot.lsp.enabled` | `true` | Enable the Language Server (requires Hot CLI) |
| `hot.lsp.commandPath` | `hot` | Path to Hot CLI executable |
| `hot.lsp.extraArgs` | `[]` | Additional LSP server arguments |
| `hot.checkForUpdates` | `true` | Check for Hot CLI updates on startup, at most once every 24 hours |

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

The server supports these methods:

| Method | What it gives |
|---------|---------------|
| `textDocument/publishDiagnostics` | The project's errors and warnings, after each edit |
| `textDocument/completion` | Namespaces, definitions, fields, variants and locals |
| `textDocument/hover` | Signatures, types, constraints and documentation |
| `textDocument/signatureHelp` | The parameters of the call being written |
| `textDocument/definition` | Where a name is defined |
| `textDocument/references` | Every use of a name in the project |
| `textDocument/prepareRename`, `textDocument/rename` | A rename as a workspace edit, or the reason it is refused |
| `textDocument/documentSymbol` | The outline of a file, as a tree or a flat list |
| `workspace/symbol` | The project's definitions by name |
| `textDocument/semanticTokens/full`, `textDocument/semanticTokens/range` | Semantic tokens |
| `textDocument/formatting` | The document as `hot fmt` formats it |

Semantic tokens use the standard token types `namespace`, `type`, `struct`,
`enum`, `enumMember`, `function`, `parameter`, `variable`, `property` and
`keyword`, with the modifiers `declaration` and `defaultLibrary`. After each
check the server sends `workspace/semanticTokens/refresh` to a client that
supports it.

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

| Command | Description | Keybinding |
|---------|-------------|------------|
| `Hot: Start Analyzer` | Start the LSP server | |
| `Hot: Stop Analyzer` | Stop the LSP server | |
| `Hot: Restart Analyzer` | Restart the LSP server | |
| `Hot: Open Settings` | Open the extension's settings | |
| `Hot: Show Logs` | Open the output channel | |
| `Hot: Create AI Coding Hints` | Run `hot ai add` in the workspace folder (`AGENTS.md` and `.skills/`) | |
| `Hot: Format Document` | Format the active Hot file | |
| `Hot: Start REPL` | Open a terminal running `hot repl` | |
| `Hot: Send Selection to REPL` | Send the selection, or the current line, to the REPL terminal | `Alt+Enter` |
| `Hot: Send File to REPL` | Send the whole file to the REPL terminal | |
| `Hot: Evaluate Selection` | Evaluate the selection, or the current line, and show the result inline | `Ctrl+Enter` (`Cmd+Enter` on macOS) |
| `Hot: Evaluate Top-Level Form` | Evaluate the top-level form at the cursor and show the result inline | `Ctrl+Shift+Enter` (`Cmd+Shift+Enter` on macOS) |
| `Hot: Evaluate to Output Panel` | Evaluate the selection, or the current line, and show the result in the eval output panel | |
| `Hot: Clear Eval Results` | Clear the inline results | `Escape` |
| `Hot: Show Eval Output` | Open the eval output panel | |
| `Hot: Reset REPL Session` | Reset the session the evaluate commands run in | |
| `Hot: Update Hot CLI` | Run `hot update` in a terminal | |

The keybindings apply while the cursor is in a Hot file.

## REPL Integration

The Hot REPL can be used alongside your editor for interactive development:

```bash
hot repl
```

Features:
- History in the session (up/down arrows)
- A prompt that shows the current namespace
- Results printed as Hot values, or as JSON with `--value.format json`

The REPL evaluates one line at a time. It has no tab completion and no
multi-line input.

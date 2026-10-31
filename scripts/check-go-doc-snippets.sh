#!/usr/bin/env bash
# check-go-doc-snippets.sh - compile every ```go block in the API docs
#
# Usage:
#   ./scripts/check-go-doc-snippets.sh HOT_GO_CHECKOUT [FILE.md ...]
#
# HOT_GO_CHECKOUT is a checkout of github.com/hot-dev/hot-go. With no files,
# checks resources/docs/api/*.md. Each block becomes the body of a `main`
# function in its own package, built against that checkout with `go vet`, so
# an unused variable or a call that no longer matches the SDK fails. Blocks
# may use `client`, `ctx`, `taskID` and `runID` without declaring them, as the
# docs' fragments do.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
hot_go="${1:?usage: $0 HOT_GO_CHECKOUT [FILE.md ...]}"
shift
hot_go="$(cd "$hot_go" && pwd)"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

python3 - "$repo_root" "$hot_go" "$workdir" "$@" <<'PY'
import pathlib
import re
import subprocess
import sys

repo_root, hot_go, workdir = (pathlib.Path(p) for p in sys.argv[1:4])
files = [pathlib.Path(f) for f in sys.argv[4:]] or sorted((repo_root / "resources/docs/api").glob("*.md"))
BLOCK = re.compile(r"^```go\n(.*?)^```", re.S | re.M)

(workdir / "go.mod").write_text(
    "module docsnippets\n\ngo 1.23\n\n"
    "require github.com/hot-dev/hot-go v0.0.0\n\n"
    f"replace github.com/hot-dev/hot-go => {hot_go}\n"
)

PRELUDE = """package main

import (
\t"context"
\t"errors"
\t"fmt"
\t"log"
\t"os"
\t"time"

\thot "github.com/hot-dev/hot-go"
)

var (
\t_      = errors.As
\t_      = fmt.Println
\t_      = log.Fatal
\t_      = os.Getenv
\t_      = time.Second
\tclient *hot.Client
\tctx    = context.Background()
\ttaskID string
\trunID  string
)

func main() {
"""

origins = {}
for md in files:
    text = md.read_text()
    for match in BLOCK.finditer(text):
        line = text[: match.start()].count("\n") + 1
        name = f"s{len(origins)}"
        (workdir / name).mkdir()
        body = "".join(f"\t{l}\n" if l else "\n" for l in match.group(1).splitlines())
        (workdir / name / "main.go").write_text(PRELUDE + body + "}\n")
        origins[name] = f"{md.relative_to(repo_root) if md.is_relative_to(repo_root) else md}:{line}"

if not origins:
    sys.exit("No ```go blocks found")

failures = 0
for name, where in origins.items():
    result = subprocess.run(["go", "vet", f"./{name}"], cwd=workdir, capture_output=True, text=True)
    if result.returncode != 0:
        failures += 1
        print(f"{where}: Go block does not compile:")
        for line in (result.stdout + result.stderr).splitlines():
            if not line.startswith("#"):
                print(f"    {line}")
if failures:
    sys.exit(f"{failures} of {len(origins)} Go doc block(s) failed")
print(f"All {len(origins)} Go doc blocks compile against {hot_go}")
PY

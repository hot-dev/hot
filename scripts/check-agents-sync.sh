#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo run --locked --bin hot -- ai add

if ! git diff --quiet -- AGENTS.md; then
  echo "AGENTS.md is out of sync with resources/ai/AGENTS.md."
  echo "Run: cargo run --locked --bin hot -- ai add"
  git diff -- AGENTS.md
  exit 1
fi

echo "AGENTS.md is in sync with resources/ai/AGENTS.md"

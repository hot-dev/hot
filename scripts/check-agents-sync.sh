#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
import difflib
import hashlib
import pathlib
import sys

repo_root = pathlib.Path.cwd()
agents_path = repo_root / "AGENTS.md"
template_path = repo_root / "resources" / "ai" / "AGENTS.md"

section_start = "<!-- HOT_LANGUAGE_SECTION_START -->"
section_end = "<!-- HOT_LANGUAGE_SECTION_END -->"

template = template_path.read_text()
expected_hash = hashlib.sha256(template.encode()).hexdigest()[:12]
expected_section = f"{section_start} hash:{expected_hash}\n{template}\n{section_end}"

try:
    agents = agents_path.read_text()
except FileNotFoundError:
    print("AGENTS.md is missing.")
    print("Run: cargo run --locked --bin hot -- ai add")
    sys.exit(1)

start_idx = agents.find(section_start)
if start_idx == -1:
    print("AGENTS.md does not contain the Hot language section.")
    print("Run: cargo run --locked --bin hot -- ai add")
    sys.exit(1)

end_idx = agents.find(section_end, start_idx)
if end_idx == -1:
    print("AGENTS.md is missing the Hot language section end marker.")
    print("Run: cargo run --locked --bin hot -- ai add")
    sys.exit(1)

existing_section = agents[start_idx : end_idx + len(section_end)]
if existing_section != expected_section:
    print("AGENTS.md is out of sync with resources/ai/AGENTS.md.")
    print("Run: cargo run --locked --bin hot -- ai add")
    print()
    sys.stdout.writelines(
        difflib.unified_diff(
            existing_section.splitlines(keepends=True),
            expected_section.splitlines(keepends=True),
            fromfile="AGENTS.md",
            tofile="resources/ai/AGENTS.md",
        )
    )
    sys.exit(1)

print("AGENTS.md is in sync with resources/ai/AGENTS.md")
PY

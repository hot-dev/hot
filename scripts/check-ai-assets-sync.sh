#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mirror_repo="${1:-"$repo_root/../hot-skills"}"
skill_name="${2:-hot-language}"

python3 - "$repo_root" "$mirror_repo" "$skill_name" <<'PY'
from __future__ import annotations

import hashlib
import re
import sys
from pathlib import Path

repo_root = Path(sys.argv[1]).resolve()
mirror_repo = Path(sys.argv[2]).resolve()
skill_name = sys.argv[3]

source_dir = repo_root / "resources" / "ai" / "skills" / skill_name
mirror_dir = mirror_repo / "skills" / skill_name
manifest_path = repo_root / "resources" / "ai" / "hot-skills-mirror.toml"

if not source_dir.is_dir():
    raise SystemExit(f"Missing canonical skill directory: {source_dir}")
if not manifest_path.is_file():
    raise SystemExit(f"Missing hot-skills mirror manifest: {manifest_path}")


def iter_files(root: Path):
    for path in sorted(root.rglob("*")):
        if path.is_file():
            yield path


def tree_hash(root: Path) -> str:
    digest = hashlib.sha256()
    for path in iter_files(root):
        rel = path.relative_to(root).as_posix()
        digest.update(rel.encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def manifest_value(name: str) -> str:
    text = manifest_path.read_text(encoding="utf-8")
    match = re.search(rf'^{name}\s*=\s*"([^"]+)"\s*$', text, re.MULTILINE)
    if not match:
        raise SystemExit(f"Manifest is missing {name}: {manifest_path}")
    return match.group(1)


expected_hash = manifest_value("tree_hash")
actual_hash = tree_hash(source_dir)

if actual_hash != expected_hash:
    print("Canonical AI skill assets are out of sync with the mirror manifest.", file=sys.stderr)
    print(f"Expected: {expected_hash}", file=sys.stderr)
    print(f"Actual:   {actual_hash}", file=sys.stderr)
    print("Run: bash scripts/sync-ai-assets.sh ../hot-skills", file=sys.stderr)
    raise SystemExit(1)

if mirror_dir.is_dir():
    mirror_hash = tree_hash(mirror_dir)
    if mirror_hash != actual_hash:
        print("Local hot-skills mirror differs from canonical Hot AI skill assets.", file=sys.stderr)
        print(f"Canonical: {actual_hash}", file=sys.stderr)
        print(f"Mirror:    {mirror_hash}", file=sys.stderr)
        print("Run: bash scripts/sync-ai-assets.sh ../hot-skills", file=sys.stderr)
        raise SystemExit(1)

print("AI skill mirror is in sync")
PY

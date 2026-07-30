#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mirror_repo="${1:-"$repo_root/../hot-skills"}"
requested_skill="${2:-}"
mirror_required="${1:+true}"

python3 - "$repo_root" "$mirror_repo" "$requested_skill" "$mirror_required" <<'PY'
from __future__ import annotations

import hashlib
import re
import sys
from pathlib import Path

repo_root = Path(sys.argv[1]).resolve()
mirror_repo = Path(sys.argv[2]).resolve()
requested_skill = sys.argv[3]
mirror_required = sys.argv[4] == "true"

skills_root = repo_root / "resources" / "ai" / "skills"
manifest_path = repo_root / "resources" / "ai" / "hot-skills-mirror.toml"


def skill_names(root: Path) -> list[str]:
    return sorted(
        path.name
        for path in root.iterdir()
        if path.is_dir() and (path / "SKILL.md").is_file()
    )


def iter_files(root: Path):
    for path in sorted(root.rglob("*")):
        if path.is_file() and ".DS_Store" not in path.parts and "__pycache__" not in path.parts:
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


def parse_manifest(path: Path) -> dict[str, dict[str, str]]:
    sections: dict[str, dict[str, str]] = {}
    current: dict[str, str] | None = None
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        section = re.fullmatch(r'\[skills\."([^"]+)"\]', line)
        if section:
            current = {}
            sections[section.group(1)] = current
            continue
        value = re.fullmatch(r'([a-z_]+)\s*=\s*"([^"]*)"', line)
        if current is not None and value:
            current[value.group(1)] = value.group(2)
    return sections


def fail_sync(message: str) -> None:
    print(message, file=sys.stderr)
    print("Run: bash scripts/sync-ai-assets.sh ../hot-skills", file=sys.stderr)
    raise SystemExit(1)


if not skills_root.is_dir():
    raise SystemExit(f"Missing canonical skills directory: {skills_root}")
if not manifest_path.is_file():
    raise SystemExit(f"Missing hot-skills mirror manifest: {manifest_path}")

names = skill_names(skills_root)
sections = parse_manifest(manifest_path)
if set(names) != set(sections):
    fail_sync(
        "Canonical skill directories and mirror manifest entries differ.\n"
        f"Canonical: {', '.join(names)}\n"
        f"Manifest:  {', '.join(sorted(sections))}"
    )
if requested_skill and requested_skill not in names:
    raise SystemExit(f"Missing canonical skill directory: {skills_root / requested_skill}")

targets = [requested_skill] if requested_skill else names
mirror_available = mirror_repo.is_dir()
if mirror_required and not mirror_available:
    fail_sync(f"Local hot-skills mirror repository is missing: {mirror_repo}")

for name in targets:
    source_dir = skills_root / name
    mirror_dir = mirror_repo / "skills" / name
    section = sections[name]
    expected_source = f"resources/ai/skills/{name}"
    expected_mirror = f"skills/{name}"
    if section.get("source_path") != expected_source or section.get("mirror_path") != expected_mirror:
        fail_sync(f"Mirror manifest paths are invalid for {name}.")

    expected_hash = section.get("tree_hash")
    actual_hash = tree_hash(source_dir)
    if actual_hash != expected_hash:
        fail_sync(
            f"Canonical AI skill assets are out of sync for {name}.\n"
            f"Expected: {expected_hash}\n"
            f"Actual:   {actual_hash}"
        )

    if mirror_available:
        if not mirror_dir.is_dir():
            fail_sync(f"Local hot-skills mirror is missing {mirror_dir}.")
        mirror_hash = tree_hash(mirror_dir)
        if mirror_hash != actual_hash:
            fail_sync(
                f"Local hot-skills mirror differs for {name}.\n"
                f"Canonical: {actual_hash}\n"
                f"Mirror:    {mirror_hash}"
            )

scope = "canonical manifest and local mirror" if mirror_available else "canonical manifest"
print(f"AI skill assets are in sync ({scope}: {', '.join(targets)})")
PY

#!/usr/bin/env bash
# check-doc-hot-blocks.sh - `hot check` every ```hot block in the AI docs
#
# Usage:
#   ./scripts/check-doc-hot-blocks.sh [FILE.md|DIR ...]
#
# With no arguments, checks resources/ai/AGENTS.md and every .md file under
# resources/ai/skills. Set HOT_BIN to a built `hot` binary to skip building.
#
# Each block is checked as its own project, against this checkout's hot-std
# and the AI packages. The docs illustrate with helpers they never define
# (`fetch-user`, `send-email`), and an unresolved name stops compilation
# before the later passes run, so it would hide every other mistake in its
# block. The check therefore fills in what a block assumes and checks it
# again:
#   - an unresolved name or type becomes an `Any` stub, and an elided body,
#     `{ ... }`, refers to one. A single-letter type is not stubbed: Hot has
#     no type parameters, so `fn (x: T): T` is a mistake;
#   - an unresolved namespace alias, such as `::http`, is declared when
#     exactly one known namespace ends that way, so the function it names
#     must still exist;
#   - a required context value gets a placeholder.
# Any other diagnostic, warnings included, fails the check.
#
# A block that is not meant to compile, such as a list of signatures, is
# skipped when the line before its fence is:
#   <!-- hot-check: skip -->

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - "$@" <<'PY'
import concurrent.futures
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile

repo_root = pathlib.Path.cwd()
SKIP_MARKER = "<!-- hot-check: skip -->"
FENCE_OPEN = re.compile(r"^ {0,3}```hot\s*$")
FENCE_CLOSE = re.compile(r"^ {0,3}```\s*$")
NS_DECL = re.compile(r"^\s*(::[A-Za-z0-9_:-]+)\s+ns\b")
ELIDED_BODY = re.compile(r"\{\s*\.\.\.\s*\}")
UNRESOLVED_NAME = re.compile(r"Unresolved variable '([^':/][^']*)'")
UNRESOLVED_ALIAS = re.compile(r"Unresolved variable '(::[A-Za-z0-9_:-]+)/")
UNRESOLVED_TYPE = re.compile(r"Unresolved type '([A-Za-z][A-Za-z0-9_-]+)'")
MISSING_CTX = re.compile(r"Missing required context variable '([^']+)'")
# Packages the AI docs use, loaded as dependencies of every block.
PACKAGES = {
    "hot.dev/hot-ai": "hot/pkg/hot-ai",
    "hot.dev/hot-ai-agent": "hot/pkg/hot-ai-agent",
}
MAX_ROUNDS = 6


def doc_files(args):
    targets = args or ["resources/ai/AGENTS.md", "resources/ai/skills"]
    files = []
    for target in targets:
        path = repo_root / target
        files.extend(sorted(path.rglob("*.md")) if path.is_dir() else [path])
    return files


def extract_blocks(md_path):
    """Yield (fence line number, body lines) for each ```hot block."""
    lines = md_path.read_text().split("\n")
    i = 0
    while i < len(lines):
        if FENCE_OPEN.match(lines[i]):
            start = i
            end = i + 1
            # A fence indented four spaces is block content, such as the
            # end of a block template string.
            while end < len(lines) and not FENCE_CLOSE.match(lines[end]):
                end += 1
            previous = next((l.strip() for l in reversed(lines[:start]) if l.strip()), "")
            if previous != SKIP_MARKER:
                yield start + 1, lines[start + 1 : end]
            i = end
        i += 1


def known_namespaces():
    found = set()
    for root in ["hot/pkg/hot-std", *PACKAGES.values()]:
        for path in (repo_root / root / "src").rglob("*.hot"):
            first = next((l for l in path.read_text().splitlines() if l.strip()), "")
            if m := NS_DECL.match(first):
                found.add(m.group(1))
    return found


NAMESPACES = known_namespaces()


def alias_target(alias):
    """The one known namespace whose name ends with `alias`, if any."""
    matches = [ns for ns in NAMESPACES if ns.endswith(alias) and ns != alias]
    return matches[0] if len(matches) == 1 else None


class Block:
    def __init__(self, md_path, fence_line, body):
        self.md_path = md_path
        self.fence_line = fence_line
        self.body = [ELIDED_BODY.sub("{ elided }", line) for line in body]
        self.declares_ns = any(NS_DECL.match(line) for line in self.body)
        self.stubs = set()
        self.type_stubs = set()
        self.aliases = {}
        self.ctx_keys = set()

    @property
    def where(self):
        return f"{self.md_path.relative_to(repo_root)}:{self.fence_line}"

    def render(self):
        """Return the file text, and for each file line its body line or None."""
        added = [f"{alias} {ns}" for alias, ns in sorted(self.aliases.items())]
        added += [f"{name} type Any" for name in sorted(self.type_stubs)]
        added += [f"{name}: Any null" for name in sorted(self.stubs)]
        lines, origin = [], []
        if not self.declares_ns:
            lines += ["::doc ns", "", *added]
            origin += [None] * len(lines)
        for i, line in enumerate(self.body):
            lines.append(line)
            origin.append(i)
            # Additions follow each namespace declaration, so they are in
            # scope wherever the block uses them.
            if self.declares_ns and NS_DECL.match(line):
                lines += added
                origin += [None] * len(added)
        return "\n".join(lines) + "\n", origin

    def fill_in(self, diagnostic):
        """Supply what the diagnostic says is missing; False if nothing was."""
        code, message = diagnostic["code"], diagnostic["message"]
        if code == "missing-context" and (m := MISSING_CTX.search(message)):
            new, found = self.ctx_keys, m.group(1)
        elif code == "unresolved-variable" and (m := UNRESOLVED_ALIAS.search(message)):
            target = alias_target(m.group(1))
            if target is None or m.group(1) in self.aliases:
                return False
            self.aliases[m.group(1)] = target
            return True
        elif code == "unresolved-variable" and (m := UNRESOLVED_NAME.search(message)):
            new, found = self.stubs, m.group(1)
        elif code == "unresolved-type" and (m := UNRESOLVED_TYPE.search(message)):
            new, found = self.type_stubs, m.group(1)
        else:
            return False
        if found in new:
            return False
        new.add(found)
        return True

    def check(self, hot_bin, hot_home, workdir):
        """Return the block's remaining failures as printable lines."""
        project = workdir / f"block-{self.md_path.stem}-{self.fence_line}"
        (project / "src").mkdir(parents=True)
        (project / "hot.hot").write_text(
            "::hot::conf ns\n\n"
            'hot.set.project "doc"\n'
            'hot.project.doc.src.paths ["./src"]\n'
            "hot.project.doc.deps {\n"
            + "".join(
                f'    "{name}": {{ "local": "{repo_root / path}" }},\n'
                for name, path in PACKAGES.items()
            )
            + "}\n"
        )
        for _ in range(MAX_ROUNDS):
            text, origin = self.render()
            (project / "src" / "block.hot").write_text(text)
            entries = "".join(f'  "{key}": "PLACEHOLDER",\n' for key in sorted(self.ctx_keys))
            (project / "ctx.hot").write_text(
                f"::hot::run::ctx ns\n\n::hot::ctx/set({{\n{entries}}})\n"
            )
            diagnostics = run_check(hot_bin, hot_home, project)
            # Fill in everything this round reports before checking again.
            if not [d for d in diagnostics if self.fill_in(d)]:
                break
        failures = []
        for d in diagnostics:
            line = d["range"]["start"]["line"] if d.get("file") else None
            body_line = origin[line] if line is not None and line < len(origin) else None
            md_line = self.fence_line + 1 + (body_line or 0)
            text = summarize(d["message"]) or d["code"]
            failure = f"{self.md_path.relative_to(repo_root)}:{md_line}: {text}"
            if failure not in failures:
                failures.append(failure)
        return failures


def summarize(message):
    """The diagnostic's headline and, when there is one, its label."""
    lines = message.splitlines()
    if not lines:
        return ""
    label = next((l.split("─ ", 1)[-1].strip() for l in lines if "╰─" in l), "")
    return f"{lines[0]} ({label})" if label else lines[0]


def run_check(hot_bin, hot_home, project):
    result = subprocess.run(
        [hot_bin, "check", "--check.format", "json-min", "--ctx", "ctx.hot"],
        cwd=project,
        env=dict(os.environ, HOT_HOME=str(hot_home), NO_COLOR="1"),
        capture_output=True,
        text=True,
    )
    for line in reversed(result.stdout.strip().splitlines()):
        if line.startswith("["):
            return json.loads(line)
    raise RuntimeError(f"hot check printed no diagnostics in {project}:\n{result.stdout}{result.stderr}")


def build_hot():
    if os.environ.get("HOT_BIN"):
        return os.environ["HOT_BIN"]
    output = subprocess.run(
        ["cargo", "build", "--quiet", "--locked", "--bin", "hot", "--message-format", "json"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("target", {}).get("name") == "hot" and message.get("executable"):
            return message["executable"]
    sys.exit("cargo build did not report the hot executable")


def main():
    blocks = [
        Block(md_path, fence_line, body)
        for md_path in doc_files(sys.argv[1:])
        for fence_line, body in extract_blocks(md_path)
    ]
    hot_bin = build_hot()
    with tempfile.TemporaryDirectory() as tmp:
        workdir = pathlib.Path(tmp)
        # Check against this checkout's hot-std, not an installed one.
        hot_home = workdir / "home"
        (hot_home / "pkg").mkdir(parents=True)
        (hot_home / "pkg" / "hot-std").symlink_to(repo_root / "hot/pkg/hot-std")
        with concurrent.futures.ThreadPoolExecutor(os.cpu_count()) as pool:
            results = list(pool.map(lambda b: b.check(hot_bin, hot_home, workdir), blocks))

    failures = [f for result in results for f in result]
    if failures:
        print(f"{len(failures)} diagnostic(s) in {len(blocks)} doc block(s):")
        for failure in failures:
            print(f"  {failure}")
        sys.exit(1)
    print(f"All {len(blocks)} doc blocks pass hot check")


main()
PY

#!/usr/bin/env bash
# compat-diff.sh - find code a previous release accepted that this build rejects
#
# Usage:
#   ./scripts/ci/compat-diff.sh OLD_HOT NEW_HOT CORPUS [ALLOWLIST]
#
# OLD_HOT is the previous release's `hot` and NEW_HOT this build's. CORPUS is a
# checkout of the previous release's sources. OLD_HOT_HOME and NEW_HOT_HOME
# name the `hot` directories that hold each binary's hot-std (default: what
# each binary finds by itself; NEW_HOT_HOME is required to swap hot-std, see
# below).
#
# Both binaries check the corpus's own projects (`hot-dev`, with tests, and
# the `hot-pkg-all` package aggregate) and, as raw projects, the bundled
# skills' examples and tests. For the new binary the corpus's hot-std is
# replaced with NEW_HOT_HOME's, since a release always runs its own hot-std:
# what is compared is the previous release's application and package code.
#
# An error NEW_HOT reports that OLD_HOT doesn't, at the same file, line and
# code, is a compatibility break: code that the previous release accepted no
# longer checks. Warnings are not breaks, and diagnostics inside hot-std are
# ignored. Each break must be intended and listed in ALLOWLIST (default
# scripts/ci/compat-allowlist.txt) as "<code> <path glob>", with the glob
# matched against the path relative to CORPUS. Exits 1 when a break is not
# allowed.

set -euo pipefail

old_hot="${1:?usage: $0 OLD_HOT NEW_HOT CORPUS [ALLOWLIST]}"
new_hot="${2:?usage: $0 OLD_HOT NEW_HOT CORPUS [ALLOWLIST]}"
corpus="${3:?usage: $0 OLD_HOT NEW_HOT CORPUS [ALLOWLIST]}"
allowlist="${4:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/compat-allowlist.txt}"

python3 - "$old_hot" "$new_hot" "$corpus" "$allowlist" <<'PY'
import collections
import contextlib
import fnmatch
import json
import os
import pathlib
import subprocess
import sys

old_hot, new_hot = sys.argv[1], sys.argv[2]
corpus = pathlib.Path(sys.argv[3]).resolve()
allowlist_path = pathlib.Path(sys.argv[4])
old_home = os.environ.get("OLD_HOT_HOME")
new_home = os.environ.get("NEW_HOT_HOME")
ERROR = 1
STD = "hot/pkg/hot-std/"
ctx = corpus / "hot/ci.ctx.hot"
ctx_args = ["--ctx", str(ctx)] if ctx.is_file() else []

# (label, arguments, working directory)
CHECKS = [
    ("project hot-dev", ["check", "--with-tests", "true", *ctx_args], corpus),
    ("project hot-pkg-all", ["check", "-p", "hot-pkg-all", "--with-tests", "true", *ctx_args], corpus),
]
for skill in sorted((corpus / "resources/ai/skills").glob("*")):
    for sub in ("examples", "test"):
        path = skill / sub
        if path.is_dir() and any(path.rglob("*.hot")):
            CHECKS.append((f"skill {skill.name}/{sub}", ["check", str(path), "--check.raw"], path))


def errors(hot, home, args, cwd):
    env = dict(os.environ, NO_COLOR="1")
    if home:
        env["HOT_HOME"] = home
    result = subprocess.run([hot, *args, "--check.format", "json-min"], cwd=cwd, env=env,
                            capture_output=True, text=True)
    for line in reversed(result.stdout.strip().splitlines()):
        if line.startswith("["):
            found = set()
            for d in json.loads(line):
                if d.get("severity") != ERROR:
                    continue
                file = d.get("file") or ""
                with contextlib.suppress(ValueError):
                    file = str(pathlib.Path(file).relative_to(corpus))
                if file.startswith(STD) or not file or os.path.isabs(file):
                    continue  # hot-std, or a file outside the corpus
                line_no = d["range"]["start"]["line"] + 1 if d.get("range") else 0
                headline = (d.get("message") or "").splitlines()[0] if d.get("message") else ""
                found.add((d["code"], file, line_no, headline))
            return found
    # The check didn't get as far as diagnostics: report the target as broken.
    detail = (result.stderr or result.stdout).strip().splitlines()
    return {("check-failed", str(cwd.relative_to(corpus)) or ".", 0, detail[0] if detail else "")}


@contextlib.contextmanager
def new_std():
    """Point the corpus's hot-std at the new release's for the new binary."""
    std = corpus / "hot/pkg/hot-std"
    if not new_home:
        yield
        return
    aside = corpus / "hot/pkg/.hot-std-previous"
    std.rename(aside)
    std.symlink_to(pathlib.Path(new_home).resolve() / "pkg/hot-std")
    try:
        yield
    finally:
        std.unlink()
        aside.rename(std)


def allowed():
    rules = []
    if allowlist_path.is_file():
        for raw in allowlist_path.read_text().splitlines():
            line = raw.split("#", 1)[0].strip()
            if line:
                code, glob = line.split(None, 1)
                rules.append((code, glob.strip()))
    return rules


def is_allowed(rules, code, file):
    return any(rule_code in ("*", code) and fnmatch.fnmatch(file, glob) for rule_code, glob in rules)


old = {label: errors(old_hot, old_home, args, cwd) for label, args, cwd in CHECKS}
with new_std():
    new = {label: errors(new_hot, new_home, args, cwd) for label, args, cwd in CHECKS}

rules = allowed()
breaks = []
for label, _, _ in CHECKS:
    seen = {(code, file, line) for code, file, line, _ in old[label]}
    breaks += [(label, *d) for d in sorted(new[label]) if (d[0], d[1], d[2]) not in seen]

unexpected = [b for b in breaks if not is_allowed(rules, b[1], b[2])]
by_code = collections.Counter(b[1] for b in breaks)
print(f"Compared {len(CHECKS)} checks of {corpus}")
print(f"New errors: {len(breaks)} ({', '.join(f'{c} {n}' for c, n in by_code.most_common()) or 'none'})")
for label, code, file, line, headline in breaks:
    mark = "  " if is_allowed(rules, code, file) else "!!"
    print(f"{mark} {file}:{line}: {headline}  ({label})")
if unexpected:
    print(f"{len(unexpected)} new error(s) are not in {allowlist_path}: "
          "code the previous release accepted no longer checks.")
    sys.exit(1)
print("No unexpected compatibility breaks")
PY

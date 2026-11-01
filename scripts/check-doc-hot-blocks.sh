#!/usr/bin/env bash
# check-doc-hot-blocks.sh - `hot check` every ```hot block in the AI docs
#
# Usage:
#   ./scripts/check-doc-hot-blocks.sh [--show-stubs] [FILE.md|DIR ...]
#
# With no arguments, checks resources/ai/AGENTS.md and every .md file under
# resources/ai/skills. Set HOT_BIN to a built `hot` binary to skip building.
# --show-stubs prints, for every block, the names the check filled in.
#
# Each block is checked as its own project, against this checkout's hot-std
# and the AI packages. The docs illustrate with helpers they never define
# (`fetch-user`, `send-email`), and an unresolved name stops compilation
# before the later passes run, so it would hide every other mistake in its
# block. The check therefore fills in what a block assumes and checks it
# again:
#   - an unresolved name or type becomes an `Any` stub, and an elided body,
#     `{ ... }`, refers to one. A single-letter type is not stubbed: Hot has
#     no type parameters, so `fn (x: T): T` is a mistake. A name the block
#     declares at its top level is never stubbed either: if it is
#     unresolved, an earlier line swallowed its declaration;
#   - an unresolved namespace alias, such as `::http`, is declared when
#     exactly one known namespace ends that way, so the function it names
#     must still exist;
#   - a required context value gets a placeholder.
# Any other diagnostic, warnings included, fails the check, and a failing
# block's report lists what was filled in.
#
# A block that is not meant to compile, such as a list of signatures, is
# skipped when the line before its fence is:
#   <!-- hot-check: skip -->
# Skipped blocks are still swept: each `name fn (params): Type` signature
# line must name a function that exists with that many parameters, in the
# namespace of the nearest `::hot::...` heading (or a core function when the
# heading names none). Every `::ns/name` reference in a skipped block, and in
# the docs' inline code, must resolve when `::ns` is a namespace this check
# knows (a prose line that says "removed" is left out). A bullet such as
# "- `::hot::run` — `info`, `is-inline-run`" lists names that must exist in
# that namespace.

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
FENCE_ANY = re.compile(r"^ {0,3}```")
FENCE_CLOSE = re.compile(r"^ {0,3}```\s*$")
NS_PATH = r"::[a-z][a-z0-9-]*(?:::[a-z][a-z0-9-]*)*"
NS_DECL = re.compile(r"^\s*(::[A-Za-z0-9_:-]+)\s+ns\b")
ELIDED_BODY = re.compile(r"\{\s*\.\.\.\s*\}")
UNRESOLVED_NAME = re.compile(r"Unresolved variable '([^':/][^']*)'")
UNRESOLVED_ALIAS = re.compile(r"Unresolved variable '(::[A-Za-z0-9_:-]+)/")
UNRESOLVED_TYPE = re.compile(r"Unresolved type '([A-Za-z][A-Za-z0-9_-]+)'")
MISSING_CTX = re.compile(r"Missing required context variable '([^']+)'")
# A top-level binding or definition: a name at column 0 followed by a value.
# Arrows and pipes that start the rest of the line are uses, not bindings.
TOP_LEVEL_BINDING = re.compile(r"^([A-Za-z][A-Za-z0-9_-]*[?!]?)(?::\s*\S+)?\s+(?!->|\|>|=>|//)\S")
# A name alone on its line, declared by the `meta`/`fn`/`type` line after it.
TOP_LEVEL_NAME = re.compile(r"^([A-Za-z][A-Za-z0-9_-]*[?!]?)\s*$")
DECLARATION_CONTINUES = re.compile(r"^(meta|fn|type|enum)\b")
# Sweep patterns.
SIGNATURE = re.compile(
    r"^\s*([a-z][A-Za-z0-9_-]*[?!]?)\s+fn(?:\s+(?:cond|cond-all|match|match-all|parallel|serial))?\s*\("
)
HEADING = re.compile(r"^#{1,6}\s+(.*)$")
HEADING_NS = re.compile(r"`?(" + NS_PATH + r")`?")
NS_REF = re.compile(r"(?<![A-Za-z0-9_:/-])(" + NS_PATH + r")/([A-Za-z][A-Za-z0-9_-]*[?!]?)")
ALIAS_DECL = re.compile(r"^\s*(" + NS_PATH + r")\s+(" + NS_PATH + r")\s*(?://.*)?$")
INLINE_CODE = re.compile(r"`([^`\n]+)`")
NS_BULLET = re.compile(r"^\s*[-*]\s+`(" + NS_PATH + r")`\s+(?:—|–|-)\s+(.*)$")
REMOVED = re.compile(r"\bremoved\b", re.IGNORECASE)
BULLET_NAME = re.compile(r"^([a-z][A-Za-z0-9_-]*[?!]?)(?:\((.*)\))?$")
# Packages the AI docs use, loaded as dependencies of every block.
PACKAGES = {
    "hot.dev/hot-ai": "hot/pkg/hot-ai",
    "hot.dev/hot-ai-agent": "hot/pkg/hot-ai-agent",
    "hot.dev/anthropic": "hot/pkg/anthropic",
}
MAX_ROUNDS = 6


def parse_args(argv):
    show_stubs = "--show-stubs" in argv
    return show_stubs, [a for a in argv if a != "--show-stubs"]


def doc_files(args):
    targets = args or ["resources/ai/AGENTS.md", "resources/ai/skills"]
    files = []
    for target in targets:
        path = repo_root / target
        files.extend(sorted(path.rglob("*.md")) if path.is_dir() else [path])
    return files


def previous_line(lines, index):
    return next((l.strip() for l in reversed(lines[:index]) if l.strip()), "")


def extract_blocks(md_path, skipped=False):
    """Yield (fence line number, body lines) for each ```hot block.

    With skipped=True, yield only the blocks marked to be skipped."""
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
            if (previous_line(lines, start) == SKIP_MARKER) == skipped:
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


def declared_names(body):
    """Names a block declares at its top level."""
    names = set()
    for i, line in enumerate(body):
        if DECLARATION_CONTINUES.match(line):
            continue
        if m := TOP_LEVEL_BINDING.match(line):
            names.add(m.group(1))
        elif m := TOP_LEVEL_NAME.match(line):
            following = next((l.strip() for l in body[i + 1 :] if l.strip()), "")
            if DECLARATION_CONTINUES.match(following):
                names.add(m.group(1))
    return names


class Block:
    def __init__(self, index, md_path, fence_line, body):
        self.index = index
        self.md_path = md_path
        self.fence_line = fence_line
        self.body = [ELIDED_BODY.sub("{ elided }", line) for line in body]
        self.declares_ns = any(NS_DECL.match(line) for line in self.body)
        self.declared = declared_names(self.body)
        self.stubs = set()
        self.type_stubs = set()
        self.aliases = {}
        self.ctx_keys = set()

    @property
    def where(self):
        return f"{self.md_path.relative_to(repo_root)}:{self.fence_line}"

    def filled_in(self):
        """What the check supplied, as one printable line (empty if nothing)."""
        parts = []
        if self.stubs:
            parts.append("names " + ", ".join(sorted(self.stubs)))
        if self.type_stubs:
            parts.append("types " + ", ".join(sorted(self.type_stubs)))
        if self.aliases:
            parts.append("aliases " + ", ".join(f"{a} {ns}" for a, ns in sorted(self.aliases.items())))
        if self.ctx_keys:
            parts.append("context " + ", ".join(sorted(self.ctx_keys)))
        return "; ".join(parts)

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
        # The block declares it, so a stub would hide the line that lost it.
        if found in self.declared and new is not self.ctx_keys:
            return False
        if found in new:
            return False
        new.add(found)
        return True

    def check(self, hot_bin, hot_home, workdir):
        """Return the block's remaining failures as printable lines."""
        project = workdir / f"block-{self.index}-{self.md_path.stem}-{self.fence_line}"
        write_project(project)
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
        if failures and (filled := self.filled_in()):
            failures.append(f"{self.where}: (block checked with stubs for {filled})")
        return failures


def write_project(project, files=None):
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
    for name, text in (files or {}).items():
        (project / "src" / name).write_text(text)


def summarize(message):
    """The diagnostic's headline and, when there is one, its label."""
    lines = message.splitlines()
    if not lines:
        return ""
    label = next((l.split("─ ", 1)[-1].strip() for l in lines if "╰─" in l), "")
    return f"{lines[0]} ({label})" if label else lines[0]


def run_check(hot_bin, hot_home, project, ctx=True):
    args = [hot_bin, "check", "--check.format", "json-min"]
    if ctx:
        args += ["--ctx", "ctx.hot"]
    result = subprocess.run(
        args,
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


# --- Sweep of skipped blocks and inline references ---------------------------


def split_params(text):
    """Split a parameter list at top-level commas; None if it never closes."""
    depth, current, params = 0, "", []
    for ch in text:
        if ch in "(<{[":
            depth += 1
        elif ch in ")>}]":
            if depth == 0:
                if current.strip():
                    params.append(current.strip())
                return params
            depth -= 1
        elif ch == "," and depth == 0:
            params.append(current.strip())
            current = ""
            continue
        current += ch
    return None


def call_arity(params):
    """How many arguments to pass: rest parameters are tested with extras."""
    fixed = [p for p in params if p and not p.startswith("...")]
    variadic = len(fixed) != len([p for p in params if p])
    return len(fixed) + (4 if variadic else 0)


class Sweep:
    """Calls and references the docs claim exist, checked as one project."""

    def __init__(self):
        self.lines = []  # generated source lines
        self.origins = []  # (md location, what was claimed) per generated line

    def add(self, code, where, claim):
        self.lines.append(f"sweep-{len(self.lines)} fn () {{ {code} }}")
        self.origins.append((where, claim))

    def resolve(self, ns, aliases):
        if ns in aliases:
            return aliases[ns]
        if ns in NAMESPACES:
            return ns
        return alias_target(ns)

    def reference(self, ns, name, aliases, where, arity=None):
        target = self.resolve(ns, aliases)
        if target is None:
            # A namespace this check doesn't load (user code, other packages)
            # can't be verified, except that `::hot::` is reserved for std.
            if ns.startswith("::hot::"):
                self.add(f"{ns}/{name}", where, f"`{ns}/{name}`")
            return
        call = f"{target}/{name}" + (f"({', '.join(['a'] * arity)})" if arity is not None else "")
        self.add(call, where, f"`{ns}/{name}`")

    def scan(self, md_path):
        rel = md_path.relative_to(repo_root)
        lines = md_path.read_text().split("\n")
        heading_ns = None
        in_fence = False
        skipped = False
        aliases = {}
        for number, line in enumerate(lines, start=1):
            where = f"{rel}:{number}"
            if FENCE_ANY.match(line):
                if not in_fence:
                    in_fence = True
                    skipped = bool(FENCE_OPEN.match(line)) and previous_line(lines, number - 1) == SKIP_MARKER
                    aliases = {}
                elif FENCE_CLOSE.match(line):
                    in_fence = False
                continue
            if in_fence:
                if skipped:
                    self.scan_skipped_line(line, heading_ns, aliases, where)
                continue
            if m := HEADING.match(line):
                ns = HEADING_NS.search(m.group(1))
                heading_ns = ns.group(1) if ns and ns.group(1) in NAMESPACES else None
                continue
            self.scan_prose_line(line, where)

    def scan_skipped_line(self, line, heading_ns, aliases, where):
        code = line.split("//", 1)[0]
        if m := ALIAS_DECL.match(code):
            aliases[m.group(1)] = m.group(2)
            return
        if m := SIGNATURE.match(code):
            name = m.group(1)
            params = split_params(code[m.end() :])
            if params is not None and "{" not in code[m.end() :].split(")", 1)[-1]:
                arity = call_arity(params)
                callee = f"{heading_ns}/{name}" if heading_ns else name
                args = ", ".join(["a"] * arity)
                self.add(f"{callee}({args})", where, f"`{name}` with {arity} argument(s)")
            return
        for ref in NS_REF.finditer(code):
            self.reference(ref.group(1), ref.group(2), aliases, where)

    def scan_prose_line(self, line, where):
        if m := NS_BULLET.match(line):
            ns = m.group(1)
            if ns in NAMESPACES or ns.startswith("::hot::"):
                for span in INLINE_CODE.findall(m.group(2)):
                    if item := BULLET_NAME.match(span.strip()):
                        args = item.group(2)
                        arity = None if args is None else len(split_params(args + ")") or [])
                        self.add(
                            f"{ns}/{item.group(1)}"
                            + ("" if arity is None else f"({', '.join(['a'] * arity)})"),
                            where,
                            f"`{item.group(1)}` in `{ns}`",
                        )
            return
        # Prose may name a function to say it is gone.
        if REMOVED.search(line):
            return
        for span in INLINE_CODE.findall(line):
            for ref in NS_REF.finditer(span):
                self.reference(ref.group(1), ref.group(2), {}, where)

    def check(self, hot_bin, hot_home, workdir):
        if not self.lines:
            return []
        project = workdir / "sweep"
        header = ["::doc::sweep ns", "", "a: Any null", ""]
        write_project(project)
        lines, failures = list(self.lines), []
        # An unresolved name stops compilation before arities are checked, so
        # drop each failing line and check the rest again.
        for _ in range(MAX_ROUNDS):
            (project / "src" / "sweep.hot").write_text("\n".join(header + lines) + "\n")
            diagnostics = run_check(hot_bin, hot_home, project, ctx=False)
            if not diagnostics:
                break
            for d in diagnostics:
                line = d["range"]["start"]["line"] - len(header) if d.get("file") else -1
                if not 0 <= line < len(self.origins):
                    failures.append(f"(sweep) {summarize(d['message']) or d['code']}")
                    return failures
                where, claim = self.origins[line]
                failure = f"{where}: {claim}: {summarize(d['message']) or d['code']}"
                if failure not in failures:
                    failures.append(failure)
                lines[line] = f"sweep-{line} null"
        return failures


def main():
    show_stubs, targets = parse_args(sys.argv[1:])
    files = doc_files(targets)
    blocks = [
        Block(index, md_path, fence_line, body)
        for index, (md_path, fence_line, body) in enumerate(
            (md_path, fence_line, body)
            for md_path in files
            for fence_line, body in extract_blocks(md_path)
        )
    ]
    sweep = Sweep()
    for md_path in files:
        sweep.scan(md_path)
    hot_bin = build_hot()
    with tempfile.TemporaryDirectory() as tmp:
        workdir = pathlib.Path(tmp)
        # Check against this checkout's hot-std, not an installed one.
        hot_home = workdir / "home"
        (hot_home / "pkg").mkdir(parents=True)
        (hot_home / "pkg" / "hot-std").symlink_to(repo_root / "hot/pkg/hot-std")
        with concurrent.futures.ThreadPoolExecutor(os.cpu_count()) as pool:
            results = list(pool.map(lambda b: b.check(hot_bin, hot_home, workdir), blocks))
        sweep_failures = sweep.check(hot_bin, hot_home, workdir)

    if show_stubs:
        for block in blocks:
            if filled := block.filled_in():
                print(f"{block.where}: {filled}")
    failures = [f for result in results for f in result] + sweep_failures
    summary = f"{len(blocks)} doc block(s) and {len(sweep.lines)} signature(s) and reference(s)"
    if failures:
        print(f"{len(failures)} problem(s) in {summary}:")
        for failure in failures:
            print(f"  {failure}")
        sys.exit(1)
    print(f"All {summary} pass hot check")


main()
PY

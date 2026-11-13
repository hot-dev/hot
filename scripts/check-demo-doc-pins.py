#!/usr/bin/env python3
"""Check what the demo pages say about the hot-demos repository.

The pages under resources/docs/demos describe projects that live in another
repository, https://github.com/hot-dev/hot-demos. This check reads, from each
page, the facts it states about a demo:

  paths         files and directories: links into the repository, and the
                tree under "## Project Layout"
  packages      the `hot.dev/...` packages the demo's hot.hot depends on,
                with the version when the page gives one
  commands      the slash commands in the page's command tables
  env           environment variables, such as `HOT_API_KEY`
  npm-scripts   `npm run dev`, `npm start`

Usage:
    python3 scripts/check-demo-doc-pins.py
        Offline, for every pull request. The facts on the pages must be
        exactly the ones recorded in scripts/demo-doc-pins.json, which were
        true of hot-demos at the commit recorded there. A page that states
        something new, or stops stating something, fails until the manifest
        is written again. Each recorded package must also be a package in
        hot/pkg, and a recorded version no newer than this checkout's.

    python3 scripts/check-demo-doc-pins.py --demos ../hot-demos
        Against a checkout of hot-demos (its main branch is what the pages
        link to). Every fact on the pages must be true of that checkout, and
        the manifest must match. The nightly workflow runs this, so a change
        in hot-demos that the pages have not followed shows up within a day.

    python3 scripts/check-demo-doc-pins.py --demos ../hot-demos --write
        Check the facts against the checkout, then write the manifest.

A page named after a demo directory, such as hot-chat.md, describes that
demo. index.md is checked for its links only.
"""

import argparse
import json
import pathlib
import re
import subprocess
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
PAGES = REPO_ROOT / "resources/docs/demos"
MANIFEST = REPO_ROOT / "scripts/demo-doc-pins.json"
REPOSITORY = "https://github.com/hot-dev/hot-demos"
BRANCH = "main"

LINK = re.compile(re.escape(REPOSITORY) + r"(?:/(tree|blob)/([^/\s)#]+)/([^\s)#`]+))?")
CODE_SPAN = re.compile(r"`([^`\n]+)`")
PACKAGE = re.compile(r"^(hot\.dev/[a-z][a-z0-9-]*)(?:[@ ]v?(\d+\.\d+\.\d+))?$")
PINNED = re.compile(r'"(hot\.dev/[a-z][a-z0-9-]*)"\s*:\s*"(\d+\.\d+\.\d+)"')
COMMAND = re.compile(r"^(/[a-z][a-z0-9-]*)\b")
ENV_NAME = re.compile(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b")
NPM_SCRIPT = re.compile(r"\bnpm (?:run ([A-Za-z0-9:_-]+)|(start|test)\b)")
LAYOUT_ENTRY = re.compile(r"^( *)([^\s#]+)")
SKIPPED_DIRS = {"node_modules", ".next", ".hot", ".git", "dist", "build"}


def version_tuple(version):
    return tuple(int(part) for part in version.split("."))


def page_facts(name, text):
    """The facts a page states, and the problems in how it states them."""
    demo = None if name == "index.md" else name[: -len(".md")]
    facts = {"paths": set(), "packages": {}, "commands": set(), "env": set(), "npm-scripts": set()}
    problems = []
    fence = None
    heading = ""
    layout_stack = []
    for number, line in enumerate(text.split("\n"), start=1):
        stripped = line.strip()
        for match in LINK.finditer(line):
            _, branch, path = match.groups()
            if branch is None:
                continue
            if branch != BRANCH:
                problems.append(f"{name}:{number}: links to the `{branch}` branch, not `{BRANCH}`")
            facts["paths"].add(path.rstrip("/"))
        if stripped.startswith("```"):
            fence = None if fence is not None else stripped[3:].strip() or "text"
            layout_stack = []
            continue
        if fence is None:
            if stripped.startswith("#"):
                heading = stripped.lstrip("#").strip()
                continue
            if demo is None:
                continue
            for span in CODE_SPAN.findall(line):
                if match := PACKAGE.match(span.strip()):
                    # A version given anywhere on the page counts.
                    if match.group(2) or match.group(1) not in facts["packages"]:
                        facts["packages"][match.group(1)] = match.group(2)
                if stripped.startswith("|") and (match := COMMAND.match(span.strip())):
                    facts["commands"].add(match.group(1))
                facts["env"].update(ENV_NAME.findall(span))
        elif demo is not None:
            for package, version in PINNED.findall(line):
                facts["packages"][package] = version
            if fence in ("bash", "sh", "shell"):
                for run, bare in NPM_SCRIPT.findall(line):
                    facts["npm-scripts"].add(run or bare)
                facts["env"].update(ENV_NAME.findall(line))
            if heading == "Project Layout" and (match := LAYOUT_ENTRY.match(line)):
                depth, entry = len(match.group(1)) // 2, match.group(2).rstrip("/")
                layout_stack = layout_stack[:depth] + [entry]
                path = "/".join(layout_stack)
                # The tree's root is the demo directory itself.
                facts["paths"].add(path if path.split("/")[0] == demo else f"{demo}/{path}")
    return facts, problems


def read_pages(pages_dir=PAGES):
    facts, problems = {}, []
    for path in sorted(pages_dir.glob("*.md")):
        page, page_problems = page_facts(path.name, path.read_text(encoding="utf-8"))
        facts[f"demos/{path.name}"] = page
        problems += page_problems
    return facts, problems


def as_json(facts):
    """Facts in the manifest's shape: sorted lists, and no empty kinds."""
    out = {}
    for page, kinds in sorted(facts.items()):
        entry = {}
        for kind, values in kinds.items():
            if not values:
                continue
            entry[kind] = dict(sorted(values.items())) if isinstance(values, dict) else sorted(values)
        out[page] = entry
    return out


def demo_text(demo_dir):
    """Path -> text for the demo's own source and configuration files."""
    files = {}
    for path in sorted(demo_dir.rglob("*")):
        relative = path.relative_to(demo_dir)
        if not path.is_file() or SKIPPED_DIRS & set(relative.parts) or path.stat().st_size > 1_000_000:
            continue
        if relative.name == "package-lock.json":
            continue
        try:
            files[relative.as_posix()] = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
    return files


def verify(facts, demos_dir):
    """Problems where a page's facts are not true of the checkout."""
    problems = []
    for page, kinds in sorted(facts.items()):
        for path in sorted(kinds["paths"]):
            if not (demos_dir / path).exists():
                problems.append(f"{page}: hot-demos has no `{path}`")
        demo = pathlib.Path(page).stem
        if demo == "index":
            continue
        demo_dir = demos_dir / demo
        if not demo_dir.is_dir():
            problems.append(f"{page}: hot-demos has no `{demo}` directory for this page")
            continue
        files = demo_text(demo_dir)
        pinned = dict(PINNED.findall(files.get("hot.hot", "")))
        for package, stated in sorted(kinds["packages"].items()):
            if package not in pinned:
                problems.append(f"{page}: `{demo}/hot.hot` does not depend on `{package}`")
            elif stated and stated != pinned[package]:
                problems.append(
                    f"{page}: says `{package}` {stated}, but `{demo}/hot.hot` pins {pinned[package]}"
                )
        sources = {p: t for p, t in files.items() if p.startswith(("hot/", "src/"))}
        for command in sorted(kinds["commands"]):
            quoted = re.compile(r"""["'`]""" + re.escape(command) + r"""\b""")
            if not any(quoted.search(text) for text in sources.values()):
                problems.append(f"{page}: no source file of `{demo}` handles `{command}`")
        for name in sorted(kinds["env"]):
            if not any(re.search(rf"\b{name}\b", text) for text in files.values()):
                problems.append(f"{page}: nothing in `{demo}` reads or sets `{name}`")
        scripts = {}
        if "package.json" in files:
            scripts = json.loads(files["package.json"]).get("scripts", {})
        for script in sorted(kinds["npm-scripts"]):
            if script not in scripts:
                problems.append(f"{page}: `{demo}/package.json` has no `{script}` script")
    return problems


def repo_package_versions(repo_root=REPO_ROOT):
    versions = {}
    for manifest in (repo_root / "hot/pkg").glob("*/pkg.hot"):
        text = manifest.read_text(encoding="utf-8")
        name = re.search(r'^\s*name:\s*"([^"]+)"', text, re.MULTILINE)
        version = re.search(r'^\s*version:\s*"(\d+\.\d+\.\d+)"', text, re.MULTILINE)
        if name and version:
            versions[name.group(1)] = version.group(1)
    return versions


def compare(facts, manifest, versions):
    """Problems where the pages and the manifest disagree, or the manifest
    names a package version this checkout does not have."""
    problems = []
    stated, recorded = as_json(facts), manifest.get("facts", {})
    for page in sorted(set(stated) | set(recorded)):
        now, then = stated.get(page, {}), recorded.get(page, {})
        for kind in sorted(set(now) | set(then)):
            current, pinned = now.get(kind, []), then.get(kind, [])
            for value in sorted(set(current) - set(pinned)):
                problems.append(f"{page}: states {kind} `{value}`, which the manifest does not record")
            for value in sorted(set(pinned) - set(current)):
                problems.append(f"{page}: no longer states {kind} `{value}`, which the manifest records")
            if kind == "packages":
                for package in sorted(set(current) & set(pinned)):
                    if current[package] != pinned[package]:
                        problems.append(
                            f"{page}: states `{package}` at {current[package] or 'no version'},"
                            f" and the manifest records {pinned[package] or 'no version'}"
                        )
        for package, version in sorted(then.get("packages", {}).items()):
            if package not in versions:
                problems.append(f"{page}: `{package}` is not a package in hot/pkg")
            elif version and version_tuple(version) > version_tuple(versions[package]):
                problems.append(
                    f"{page}: the demo pins `{package}` {version}, newer than hot/pkg's {versions[package]}"
                )
    return problems


def checkout_commit(demos_dir):
    """The checkout's commit, or None when the directory is not the root of
    a git checkout (an unpacked archive, say)."""
    def git(*args):
        result = subprocess.run(["git", "-C", str(demos_dir), *args], capture_output=True, text=True)
        return result.stdout.strip() if result.returncode == 0 else None

    top = git("rev-parse", "--show-toplevel")
    if top is None or pathlib.Path(top).resolve() != demos_dir.resolve():
        return None
    return git("rev-parse", "HEAD")


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--demos", type=pathlib.Path, help="a checkout of hot-demos to check against")
    parser.add_argument("--write", action="store_true", help="write the manifest (needs --demos)")
    args = parser.parse_args(argv[1:])
    if args.write and not args.demos:
        parser.error("--write needs --demos")

    facts, problems = read_pages()
    if args.demos:
        if not args.demos.is_dir():
            print(f"{args.demos} is not a directory")
            return 2
        problems += verify(facts, args.demos)
    if args.write and not problems:
        manifest = {
            "about": "What resources/docs/demos states about hot-demos, as checked against the"
            " commit below. Written by scripts/check-demo-doc-pins.py --demos DIR --write.",
            "repository": REPOSITORY,
            "branch": BRANCH,
            "commit": checkout_commit(args.demos),
            "facts": as_json(facts),
        }
        MANIFEST.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        print(f"Wrote {MANIFEST.relative_to(REPO_ROOT)}")
    if not MANIFEST.exists():
        problems.append(f"{MANIFEST.relative_to(REPO_ROOT)} is missing")
        manifest = {}
    else:
        manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    problems += compare(facts, manifest, repo_package_versions())

    count = sum(len(values) for kinds in facts.values() for values in kinds.values())
    against = f"hot-demos at {args.demos}" if args.demos else f"the manifest ({str(manifest.get('commit'))[:12]})"
    if problems:
        print(f"{len(problems)} problem(s) in {count} fact(s) the demo pages state, against {against}:")
        for problem in problems:
            print(f"  {problem}")
        if not args.demos:
            print(
                "If the pages are right, check them against a hot-demos checkout and record them:\n"
                "  python3 scripts/check-demo-doc-pins.py --demos ../hot-demos --write"
            )
        return 1
    print(f"All {count} fact(s) the demo pages state match {against}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

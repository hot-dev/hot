#!/usr/bin/env python3
"""Check the dependency pins in Hot's packages.

Every project in hot.hot loads packages by local path, and a project's local
path replaces a package's own pin, so CI never compiles the versions the
packages pin. These checks do.

  scripts/check-pkg-pins.py
      Pin check, with no build or network. Each package's pin on another
      package in hot/pkg must equal that package's version here, or be listed
      in ALLOWED_PINS. A dependency must be a version: a local or git path
      means something else once the package is installed.

  scripts/check-pkg-pins.py smoke [--hot BIN] [--jobs N] [PKG ...]
      Real-pin smoke test. For each package in hot/pkg/pkg-publish.txt
      (except hot-std), generates a project that depends on it and runs
      `hot check`. A pin that equals a package's version here loads this
      checkout's copy, at any depth, since that version may be unpublished.
      Any other pin is downloaded from pkg.hot.dev, so old pins really
      compile. Builds `hot` unless --hot or HOT_BIN names one.

Exit codes: 0 when everything passes, 1 when a pin is wrong or a package
doesn't compile, and 2 (smoke only) when the only failures are downloads
from pkg.hot.dev that didn't complete, which says nothing about the code.
"""

import argparse
import concurrent.futures
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
PKG_DIR = PROJECT_ROOT / "hot" / "pkg"
PUBLISH_LIST = PKG_DIR / "pkg-publish.txt"
CI_CTX = PROJECT_ROOT / "hot" / "ci.ctx.hot"

# Pins on an older version of a sibling, kept on purpose. Each entry is
# (package directory, dependency, pinned version). The smoke test still
# compiles these against the published version.
ALLOWED_PINS = {
    # The one exception when this check was added.
    ("slack-admin", "hot.dev/slack", "1.1.0"),
    # TEMPORARY: shows the smoke test failing on a stale pin. Reverted next.
    ("mcp", "hot.dev/hot-ai", "1.4.0"),
}

# The message `hot` gives when a pkg.hot.dev download fails for a reason
# other than a missing version (see download_cdn_package in
# crates/hot/src/lang/project.rs). A 404 means the pin names a version that
# was never published, which is a broken pin.
FETCH_ERROR = re.compile(r"from CDN: (Network error fetching|HTTP error \d+ fetching|Failed to read package data)")


class Package:
    def __init__(self, dir_name, name, version, deps):
        self.dir_name = dir_name
        self.name = name
        self.version = version
        # dependency name -> version string, or the raw map source
        self.deps = deps


def matching_brace(text, start):
    """Index of the brace closing the one at `start`, skipping strings."""
    depth = 0
    i = start
    in_string = False
    while i < len(text):
        c = text[i]
        if in_string:
            if c == "\\":
                i += 1
            elif c == '"':
                in_string = False
        elif c == '"':
            in_string = True
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise ValueError("unbalanced braces")


def parse_pkg_hot(path):
    text = path.read_text()
    name = re.search(r'^\s*name:\s*"([^"]+)"', text, re.M)
    version = re.search(r'^\s*version:\s*"([^"]+)"', text, re.M)
    if not name or not version:
        raise ValueError(f"{path}: no name or version")
    deps = {}
    deps_match = re.search(r"^\s*deps:\s*\{", text, re.M)
    if deps_match:
        open_brace = deps_match.end() - 1
        body = text[open_brace + 1 : matching_brace(text, open_brace)]
        i = 0
        entry = re.compile(r'\s*,?\s*"([^"]+)"\s*:\s*')
        while True:
            m = entry.match(body, i)
            if not m:
                break
            dep_name = m.group(1)
            i = m.end()
            if body[i] == '"':
                end = body.index('"', i + 1)
                deps[dep_name] = body[i + 1 : end]
                i = end + 1
            elif body[i] == "{":
                end = matching_brace(body, i)
                deps[dep_name] = {"map": " ".join(body[i : end + 1].split())}
                i = end + 1
            else:
                raise ValueError(f"{path}: can't read the spec for {dep_name}")
        if body[i:].strip(" \t\n,"):
            raise ValueError(f"{path}: can't read deps after {body[i:i + 40]!r}")
    return Package(path.parent.name, name.group(1), version.group(1), deps)


def repo_packages():
    packages = {}
    for pkg_hot in sorted(PKG_DIR.glob("*/pkg.hot")):
        package = parse_pkg_hot(pkg_hot)
        packages[package.name] = package
    return packages


def published_names():
    names = []
    for line in PUBLISH_LIST.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.append(line)
    return names


def check_pins():
    packages = repo_packages()
    published = set(published_names())
    errors = []
    allowed_used = set()

    for package in packages.values():
        for dep_name, spec in package.deps.items():
            where = f"hot/pkg/{package.dir_name}/pkg.hot: {dep_name}"
            if isinstance(spec, dict):
                if dep_name == "hot.dev/hot-std" and spec["map"] == "{}":
                    continue
                if re.search(r"\bgit\s*:", spec["map"]):
                    problem = "is a git dependency, so what it means changes whenever that branch moves"
                else:
                    problem = "names no version, and its local path won't exist once the package is installed"
                errors.append(f'{where} {spec["map"]} {problem}. Pin a version instead.')
                continue
            target = packages.get(dep_name)
            if target is None:
                errors.append(f'{where} "{spec}" names a package that isn\'t in hot/pkg.')
                continue
            if package.dir_name in published and target.dir_name not in published:
                errors.append(
                    f'{where} "{spec}": {target.dir_name} isn\'t in hot/pkg/pkg-publish.txt, '
                    f"so installing {package.dir_name} would fail."
                )
            if spec == target.version:
                continue
            key = (package.dir_name, dep_name, spec)
            if key in ALLOWED_PINS:
                allowed_used.add(key)
                continue
            errors.append(
                f'{where} pins "{spec}", but hot/pkg/{target.dir_name} is {target.version}. '
                f"Pin {target.version}, or add the pin to ALLOWED_PINS in scripts/check-pkg-pins.py with the reason."
            )

    for key in sorted(ALLOWED_PINS - allowed_used):
        errors.append(
            f"ALLOWED_PINS lists {key[0]} pinning {key[1]} {key[2]}, which it no longer does. "
            "Remove the entry."
        )

    if errors:
        for error in errors:
            print(f"✗ {error}")
        print(f"\n{len(errors)} pin problem(s)")
        return 1
    print(f"✓ Package pins match the repo versions ({len(packages)} packages)")
    return 0


def build_hot():
    # Build from the checkout root: cargo run from elsewhere would miss
    # .cargo/config.toml and rebuild with other flags.
    output = subprocess.run(
        ["cargo", "build", "--quiet", "--locked", "--bin", "hot", "--message-format", "json"],
        cwd=PROJECT_ROOT,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("target", {}).get("name") == "hot" and message.get("executable"):
            return message["executable"]
    sys.exit("cargo build did not report the hot executable")


def smoke_project(work, hot_bin, package, shared_cdn):
    """Run `hot check` on a project that depends on `package` by version."""
    root = work / package.dir_name
    # A HOT_HOME of its own, so this checkout's hot-std is the one loaded and
    # no project shares a bytecode cache. The CDN cache is shared.
    home = root / "home"
    (home / "pkg").mkdir(parents=True)
    (home / "pkg" / "hot-std").symlink_to(PKG_DIR / "hot-std")
    (home / "cache").mkdir()
    (home / "cache" / "cdn").symlink_to(shared_cdn)

    project = root / "project"
    (project / "src").mkdir(parents=True)
    (project / "hot.hot").write_text(
        "::hot::conf ns\n\n"
        'hot.set.project "pin-smoke"\n'
        'hot.project.pin-smoke.src.paths ["./src"]\n'
        f'hot.project.pin-smoke.deps {{ "{package.name}": "{package.version}" }}\n'
    )
    (project / "src" / "smoke.hot").write_text("::pin-smoke ns\n\nok true\n")

    command = [hot_bin, "check", "--ctx", str(CI_CTX)]
    env = {**os.environ, "HOT_HOME": str(home), "NO_COLOR": "1"}
    result = subprocess.run(
        command, cwd=project, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )
    if result.returncode == 0:
        return "ok", result.stdout
    if FETCH_ERROR.search(result.stdout):
        return "fetch", result.stdout
    return "fail", result.stdout


def smoke(args):
    packages = {p.dir_name: p for p in repo_packages().values()}
    names = args.packages or [n for n in published_names() if n != "hot-std"]
    missing = [n for n in names if n not in packages]
    if missing:
        sys.exit(f"Not packages in hot/pkg: {', '.join(missing)}")

    # Absolute, since each check runs in its own project directory.
    hot_bin = str(Path(args.hot or os.environ.get("HOT_BIN") or build_hot()).resolve())
    work = Path(tempfile.mkdtemp(prefix="hot-pin-smoke-"))
    try:
        # Seed the CDN cache with this checkout's packages at their versions
        # here, so `hot` resolves those pins locally and downloads the rest.
        shared_cdn = work / "cdn"
        for package in packages.values():
            if package.dir_name == "hot-std":
                continue
            org, short_name = package.name.split("/", 1)
            seeded = shared_cdn / org / short_name / package.version
            seeded.parent.mkdir(parents=True, exist_ok=True)
            seeded.symlink_to(PKG_DIR / package.dir_name)

        started = time.monotonic()
        results = {}

        def run(name):
            return name, smoke_project(work, hot_bin, packages[name], shared_cdn)

        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            for name, (status, output) in pool.map(run, names):
                results[name] = (status, output)

        # A failed download may be a blip; try those once more.
        for name in [n for n, (status, _) in results.items() if status == "fetch"]:
            shutil.rmtree(work / name)
            results[name] = smoke_project(work, hot_bin, packages[name], shared_cdn)

        elapsed = time.monotonic() - started
        downloaded = sorted(
            f"{org.name}/{pkg.name}/{version.name}"
            for org in shared_cdn.iterdir()
            if org.is_dir()
            for pkg in org.iterdir()
            if pkg.is_dir()
            for version in pkg.iterdir()
            if version.is_dir() and not version.is_symlink()
        )
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)

    failed = [n for n in names if results[n][0] == "fail"]
    fetch_failed = [n for n in names if results[n][0] == "fetch"]
    for name in names:
        status, output = results[name]
        package = packages[name]
        if status == "ok":
            print(f"✓ {name} {package.version}")
        else:
            label = "couldn't download a dependency from pkg.hot.dev" if status == "fetch" else "failed"
            print(f"✗ {name} {package.version} {label}:")
            print("  " + output.strip().replace("\n", "\n  "))

    print()
    if downloaded:
        print(f"Downloaded from pkg.hot.dev: {', '.join(downloaded)}")
    print(f"Checked {len(names)} packages in {elapsed:.0f}s")
    if args.keep:
        print(f"Projects kept in {work}")
    if failed:
        print(f"{len(failed)} package(s) don't compile with their pins: {', '.join(failed)}")
        if fetch_failed:
            print(f"{len(fetch_failed)} more couldn't download from pkg.hot.dev: {', '.join(fetch_failed)}")
        return 1
    if fetch_failed:
        print(
            f"{len(fetch_failed)} package(s) couldn't download a dependency from pkg.hot.dev, "
            f"so their pins weren't checked: {', '.join(fetch_failed)}"
        )
        return 2
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command")
    smoke_parser = sub.add_parser("smoke", help="compile each published package with its real pins")
    smoke_parser.add_argument("packages", nargs="*", help="package directories to check (default: pkg-publish.txt)")
    smoke_parser.add_argument("--hot", help="hot binary to use (default: $HOT_BIN, else cargo build)")
    smoke_parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4, help="packages to check at once")
    smoke_parser.add_argument("--keep", action="store_true", help="keep the generated projects")
    args = parser.parse_args()
    if args.command == "smoke":
        return smoke(args)
    return check_pins()


if __name__ == "__main__":
    sys.exit(main())

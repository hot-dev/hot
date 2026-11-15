#!/usr/bin/env python3
"""
Run the benchmarks in every language, check that their results agree, and
compare the timings in tables.

Usage: ./run-all.py [--sys] [--hot-bin PATH] [--runs N] [--startup-runs N]
                    [--save [PATH]]

  --sys              Use the system-installed hot instead of a local build
  --hot-bin PATH     Use an explicit hot binary (or set HOT_BIN)
  --runs N           Run each language's suite N times and report medians
                     (default 1)
  --startup-runs N   Time N launches of a hello-world program per language
                     (default 10; 0 skips the startup timings)
  --save [PATH]      Write the results as JSON, by default to
                     results/hot-<version>-<timestamp>.json. Compare two saved
                     files with ./compare.py.
"""

import argparse
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
REPO_ROOT = SCRIPT_DIR.parent
WORKSPACE_ROOT = REPO_ROOT.parent
STARTUP_DIR = SCRIPT_DIR / "startup"
RESULTS_DIR = SCRIPT_DIR / "results"

# Lines like "name: cold=123ns warm=45ns (result: value)". The Hot-only files
# bench-constraints.hot, bench-fib-jit.hot and bench-large-reads.hot print
# other formats, so they stay out of the tables.
LINE_PATTERN = re.compile(r"^(.+?):\s*cold=(\d+)ns\s+warm=(\d+)ns\s+\(result:\s*(.*)\)$")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sys", action="store_true", help="use the system-installed hot")
    parser.add_argument("--hot-bin", default=os.environ.get("HOT_BIN"), help="explicit hot binary")
    parser.add_argument("--runs", type=int, default=1, help="suite runs per language (medians)")
    parser.add_argument("--startup-runs", type=int, default=10, help="hello-world launches per language")
    parser.add_argument("--save", nargs="?", const="", default=None, metavar="PATH", help="write results as JSON")
    args = parser.parse_args(argv)
    if args.runs < 1:
        parser.error("--runs must be at least 1")
    if args.startup_runs < 0:
        parser.error("--startup-runs can't be negative")
    return args


def resolve_hot_cmd(use_system_hot: bool, hot_bin: str | None) -> str:
    if hot_bin:
        hot_path = Path(hot_bin).expanduser()
        if not hot_path.exists():
            print(f"Error: {hot_path} not found.")
            sys.exit(1)
        print(f"Using: {hot_path}")
        return str(hot_path)

    if use_system_hot:
        print("Using system hot...")
        return "hot"

    candidates = [
        REPO_ROOT / "target" / "release" / "hot",
        WORKSPACE_ROOT / "hot" / "target" / "release" / "hot",
    ]
    for hot_path in candidates:
        if hot_path.exists():
            print(f"Using: {hot_path}")
            return str(hot_path)

    if shutil.which("hot"):
        print("Using system hot...")
        return "hot"

    print("Error: no hot binary found.")
    print("Build one with:")
    print("  cd .. && cargo build --release -p hot_cli")
    print("Or pass --sys, --hot-bin PATH, or HOT_BIN=PATH.")
    sys.exit(1)


def language_commands(hot_cmd: str) -> list[tuple[str, list[str], list[str] | None]]:
    """(label, suite command, hello-world command) for each available language."""
    langs: list[tuple[str, list[str], list[str] | None]] = []

    # Run from benchmarks/ so hot/hot.hot does not enable project runtime services.
    # The explicit emitter override keeps this true even if the cwd changes later.
    # `hot run` loads every file under hot/src, so this one run also executes the
    # Hot-only bench-constraints.hot, bench-fib-jit.hot and bench-large-reads.hot,
    # and loads the benchmark files bench.hot calls.
    hot_bench = str(SCRIPT_DIR / "hot" / "src" / "benchmarks" / "bench.hot")
    langs.append((
        "Hot",
        [hot_cmd, "run", "--emitter.type", "none", hot_bench],
        [hot_cmd, "run", "--emitter.type", "none", "hot/src/hello/hello.hot"],
    ))

    if shutil.which("python3"):
        langs.append(("Python", ["python3", "bench.py"], ["python3", "hello.py"]))
    else:
        print("Python: skipped, python3 not found")

    if shutil.which("node"):
        langs.append(("JS", ["node", "bench.js"], ["node", "hello.js"]))
    else:
        print("JavaScript: skipped, node not found")

    if shutil.which("tsx"):
        tsx = ["tsx"]
    elif shutil.which("npx"):
        tsx = ["npx", "tsx"]
    else:
        tsx = None
        print("TypeScript: skipped, neither tsx nor npx found")
    if tsx:
        langs.append(("TS", tsx + ["bench.ts"], tsx + ["hello.ts"]))

    return langs


def tool_version(cmd: list[str]) -> str | None:
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    text = (out.stdout or out.stderr).strip()
    return text.splitlines()[0] if text else None


def parse_benchmark_output(output: str) -> dict[str, dict]:
    results = {}
    for line in output.splitlines():
        match = LINE_PATTERN.match(line.strip())
        if match:
            name, cold, warm, result = match.groups()
            results[name] = {"cold_ns": int(cold), "warm_ns": int(warm), "result": result}
    return results


def run_suite(label: str, cmd: list[str], show_output: bool) -> dict[str, dict]:
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, cwd=SCRIPT_DIR, timeout=900)
    except FileNotFoundError:
        print(f"{label}: skipped, command not found")
        return {}
    except subprocess.TimeoutExpired:
        print(f"{label}: skipped, timed out")
        return {}
    if proc.returncode != 0:
        print(f"{label}: failed")
        print(f"  {(proc.stderr or proc.stdout or 'unknown error')[:400]}")
        return {}
    if show_output:
        print(proc.stdout.rstrip())
        print()
    return parse_benchmark_output(proc.stdout)


def merge_runs(runs: list[dict[str, dict]]) -> dict[str, dict]:
    """Median cold and warm time of each benchmark across runs."""
    merged = {}
    names = {name for run in runs for name in run}
    for name in names:
        entries = [run[name] for run in runs if name in run]
        merged[name] = {
            "cold_ns": int(statistics.median(e["cold_ns"] for e in entries)),
            "warm_ns": int(statistics.median(e["warm_ns"] for e in entries)),
            "result": entries[0]["result"],
            "runs": len(entries),
        }
    return merged


def time_startup(label: str, cmd: list[str], runs: int) -> int | None:
    """Median wall time in ns to launch a hello-world program, after one untimed launch."""
    times = []
    for i in range(runs + 1):
        start = time.perf_counter_ns()
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, cwd=STARTUP_DIR, timeout=120)
        except (FileNotFoundError, subprocess.TimeoutExpired):
            print(f"{label}: startup timing skipped")
            return None
        elapsed = time.perf_counter_ns() - start
        if proc.returncode != 0 or "hello" not in proc.stdout:
            print(f"{label}: startup program failed: {(proc.stderr or proc.stdout)[:200]}")
            return None
        if i > 0:
            times.append(elapsed)
    return int(statistics.median(times))


def format_ns(ns: int | None) -> str:
    if ns is None:
        return "-"
    ms = ns / 1_000_000
    if ms >= 1000:
        return f"{ms / 1000:.2f}s"
    if ms >= 10:
        return f"{ms:.0f}ms"
    if ms >= 1:
        return f"{ms:.2f}ms"
    if ns >= 1000:
        return f"{ns / 1000:.1f}us"
    return f"{ns}ns"


def print_table(title: str, rows: list[str], languages: list[str], cell) -> None:
    name_width = max([len(r) for r in rows] + [len(title)])
    col = 12
    header = f"{title:<{name_width}}" + "".join(f"  {lang:>{col}}" for lang in languages)
    print(header)
    print("-" * len(header))
    for row in rows:
        print(f"{row:<{name_width}}" + "".join(f"  {cell(row, lang):>{col}}" for lang in languages))
    print()


def print_report(all_results: dict[str, dict[str, dict]], startup: dict[str, int | None]) -> list[str]:
    languages = list(all_results.keys())
    names = sorted({name for results in all_results.values() for name in results})
    shared = [n for n in names if sum(n in all_results[lang] for lang in languages) > 1]
    hot_only = [n for n in names if n not in shared]

    def metric(key):
        def cell(row, lang):
            entry = all_results[lang].get(row)
            return format_ns(entry[key]) if entry else "-"
        return cell

    def relative(row, lang):
        entry = all_results[lang].get(row)
        if not entry:
            return "-"
        fastest = min(all_results[l][row]["warm_ns"] for l in languages if row in all_results[l])
        return f"{entry['warm_ns'] / fastest:.1f}x" if fastest else "-"

    width = 100
    print("=" * width)
    print(f"{'Benchmark Comparison':^{width}}")
    print("=" * width)
    print("warm: median of 5 samples after warm-up; cold: the first call alone\n")
    print_table("Warm", shared, languages, metric("warm_ns"))
    print_table("Relative warm (1.0x = fastest)", shared, languages, relative)
    print_table("Cold (first call)", shared, languages, metric("cold_ns"))
    if hot_only:
        print_table("Hot-only", hot_only, ["warm", "cold"], lambda row, col: format_ns(
            all_results.get("Hot", {}).get(row, {}).get("warm_ns" if col == "warm" else "cold_ns")))
    if startup:
        print_table("Startup (hello world)", ["median launch"], list(startup.keys()),
                    lambda row, lang: format_ns(startup[lang]))

    mismatches = []
    for name in shared:
        values = {lang: all_results[lang][name]["result"] for lang in languages if name in all_results[lang]}
        if len(set(values.values())) > 1:
            mismatches.append(f"{name}: " + ", ".join(f"{lang}={v}" for lang, v in values.items()))
    if mismatches:
        print("RESULT MISMATCHES (the languages computed different answers):")
        for m in mismatches:
            print(f"  {m}")
    else:
        print(f"Results agree across languages for all {len(shared)} shared benchmarks.")
    print("=" * width)
    return mismatches


def machine_info() -> dict:
    cpu = None
    if sys.platform == "darwin":
        cpu = tool_version(["sysctl", "-n", "machdep.cpu.brand_string"])
    elif Path("/proc/cpuinfo").exists():
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu": cpu,
        "cpu_count": os.cpu_count(),
        "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None,
    }


def main():
    args = parse_args(sys.argv[1:])
    hot_cmd = resolve_hot_cmd(args.sys, args.hot_bin)
    hot_version = tool_version([hot_cmd, "--version"]) or "unknown"
    print(hot_version)
    print()

    langs = language_commands(hot_cmd)
    all_results: dict[str, dict[str, dict]] = {}
    versions: dict[str, str | None] = {}
    for label, cmd, _hello in langs:
        versions[label] = tool_version(cmd[:-1] + ["--version"]) if label != "Hot" else hot_version
        runs = []
        for run in range(args.runs):
            if args.runs > 1:
                print(f"--- {label} run {run + 1}/{args.runs}")
            results = run_suite(label, cmd, show_output=(run == 0))
            if results:
                runs.append(results)
        if runs:
            all_results[label] = merge_runs(runs)

    startup: dict[str, int | None] = {}
    if args.startup_runs:
        for label, _cmd, hello in langs:
            if hello:
                startup[label] = time_startup(label, hello, args.startup_runs)

    mismatches = print_report(all_results, startup)

    if args.save is not None:
        version_match = re.search(r"(\d+\.\d+\.\d+)", hot_version)
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
        path = Path(args.save) if args.save else RESULTS_DIR / f"hot-{version_match.group(1) if version_match else 'unknown'}-{stamp}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        document = {
            "format": 1,
            "created_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "hot": {"command": hot_cmd, "version": hot_version},
            "versions": versions,
            "machine": machine_info(),
            "runs": args.runs,
            "startup_runs": args.startup_runs,
            "startup_ns": startup,
            "benchmarks": {
                name: {lang: all_results[lang][name] for lang in all_results if name in all_results[lang]}
                for name in sorted({n for r in all_results.values() for n in r})
            },
            "mismatches": mismatches,
        }
        path.write_text(json.dumps(document, indent=2) + "\n")
        print(f"Saved results to {path}")

    if mismatches:
        sys.exit(2)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""
Run all benchmarks and compare results in a table.
Usage: ./run-all.py [--sys] [--hot-bin PATH]
  --sys           Use system-installed hot instead of a local build
  --hot-bin PATH  Use an explicit hot binary path
"""

import os
import subprocess
import sys
import re
import shutil
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
REPO_ROOT = SCRIPT_DIR.parent
WORKSPACE_ROOT = REPO_ROOT.parent


def parse_args(argv: list[str]) -> tuple[bool, str | None]:
    use_system_hot = False
    hot_bin = os.environ.get("HOT_BIN")
    idx = 0
    while idx < len(argv):
        arg = argv[idx]
        if arg == "--sys":
            use_system_hot = True
        elif arg == "--hot-bin":
            idx += 1
            if idx >= len(argv):
                print("Error: --hot-bin requires a path.")
                sys.exit(1)
            hot_bin = argv[idx]
        else:
            print(f"Error: unknown argument: {arg}")
            sys.exit(1)
        idx += 1
    return use_system_hot, hot_bin


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
    print("  cd .. && cargo build --release -p hot")
    print("Or pass --sys, --hot-bin PATH, or HOT_BIN=PATH.")
    sys.exit(1)


def parse_benchmark_output(output: str) -> dict[str, tuple[float, str]]:
    """Parse benchmark output lines like 'name(args): 123.45ms (result: value)'"""
    results = {}
    pattern = r"^(.+?):\s*(\d+(?:\.\d+)?)ms\s*\(result:\s*(.+?)\)$"
    for line in output.splitlines():
        match = re.match(pattern, line.strip())
        if match:
            name = match.group(1)
            time_ms = float(match.group(2))
            result = match.group(3)
            results[name] = (time_ms, result)
    return results


def run_command(
    cmd: list[str],
    name: str,
    cwd=None,
    env_extra=None,
) -> dict[str, tuple[float, str]]:
    """Run a command and return parsed benchmark results."""
    try:
        env = os.environ.copy()
        if env_extra:
            env.update(env_extra)
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            cwd=cwd or SCRIPT_DIR,
            timeout=300,
            env=env,
        )
        if result.returncode != 0:
            print(f"Running {name} benchmarks...")
            print(f"  Error: {result.stderr[:200] if result.stderr else 'unknown error'}")
            return {}
        print(result.stdout.rstrip())
        print()
        return parse_benchmark_output(result.stdout)
    except FileNotFoundError:
        print(f"Running {name} benchmarks...")
        print("  Skipped: command not found")
        return {}
    except subprocess.TimeoutExpired:
        print(f"Running {name} benchmarks...")
        print("  Skipped: timeout")
        return {}


def format_time(ms: float) -> str:
    """Format milliseconds nicely."""
    if ms >= 1000:
        return f"{ms/1000:.2f}s"
    if ms >= 10:
        return f"{ms:.0f}ms"
    if ms >= 1:
        return f"{ms:.2f}ms"
    if ms > 0:
        return f"{ms:.3f}ms"
    if ms == 0:
        return "<0.001ms"
    return f"{ms:.2f}ms"


def print_comparison_table(all_results: dict[str, dict[str, tuple[float, str]]]):
    """Print a comparison table of all benchmark results."""
    all_benchmarks = set()
    for results in all_results.values():
        all_benchmarks.update(results.keys())

    if not all_benchmarks:
        print("No benchmark results to compare.")
        return

    benchmarks = sorted(all_benchmarks)
    languages = list(all_results.keys())

    name_width = max(len(b) for b in benchmarks)
    col_width = 12

    header = f"{'Benchmark':<{name_width}}"
    for lang in languages:
        header += f"  {lang:>{col_width}}"
    table_width = len(header)

    print()
    print("=" * table_width)
    title = "Benchmark Comparison"
    print(f"{title:^{table_width}}")
    print("=" * table_width)
    print(header)
    print("-" * table_width)

    for bench in benchmarks:
        row = f"{bench:<{name_width}}"
        for lang in languages:
            if bench in all_results[lang]:
                time_ms, _ = all_results[lang][bench]
                row += f"  {format_time(time_ms):>{col_width}}"
            else:
                row += f"  {'-':>{col_width}}"
        print(row)

    print("-" * table_width)

    print()
    rel_header = f"{'Relative (1.0x = fastest)':<{name_width}}"
    for lang in languages:
        rel_header += f"  {lang:>{col_width}}"
    print(rel_header)
    print("-" * table_width)

    for bench in benchmarks:
        row = f"{bench:<{name_width}}"
        times = []
        for lang in languages:
            if bench in all_results[lang]:
                times.append(all_results[lang][bench][0])
            else:
                times.append(None)

        valid_times = [t for t in times if t is not None]
        min_time = min(valid_times) if valid_times else 0

        for i, _lang in enumerate(languages):
            if times[i] is not None:
                ratio = times[i] / min_time if min_time > 0 else 0
                row += f"  {ratio:>{col_width}.1f}x"
            else:
                row += f"  {'-':>{col_width}}"
        print(row)

    print("=" * table_width)


def main():
    use_system_hot, hot_bin = parse_args(sys.argv[1:])
    hot_cmd = resolve_hot_cmd(use_system_hot, hot_bin)

    print()

    all_results = {}

    # Run from benchmarks/ so hot/hot.hot does not enable project runtime services.
    # The explicit emitter override keeps this true even if the cwd changes later.
    hot_bench = str(SCRIPT_DIR / "hot" / "src" / "benchmarks" / "bench.hot")
    results = run_command(
        [hot_cmd, "run", "--emitter.type", "none", hot_bench],
        "Hot",
        cwd=SCRIPT_DIR,
    )
    if results:
        all_results["Hot"] = results

    if shutil.which("python3"):
        results = run_command(["python3", "bench.py"], "Python")
        if results:
            all_results["Python"] = results
    else:
        print("Running Python benchmarks...")
        print("  Skipped: python3 not found")

    if shutil.which("node"):
        results = run_command(["node", "bench.js"], "JavaScript")
        if results:
            all_results["JS"] = results
    else:
        print("Running JavaScript benchmarks...")
        print("  Skipped: node not found")

    if shutil.which("npx"):
        results = run_command(["npx", "tsx", "bench.ts"], "TypeScript")
        if results:
            all_results["TS"] = results
    else:
        print("Running TypeScript benchmarks...")
        print("  Skipped: npx not found")

    print_comparison_table(all_results)


if __name__ == "__main__":
    main()

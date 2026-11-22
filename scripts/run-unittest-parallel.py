#!/usr/bin/env python3
"""Run unittest suites in parallel across CPU cores using only the stdlib.

Discovers individual test methods and distributes them in small batches across
a ProcessPoolExecutor so large TestCase classes do not serialize on one core.

Usage:
  python3 scripts/run-unittest-parallel.py -s infra/tests [-j N]
"""

import argparse
import concurrent.futures
import io
import os
import sys
import time
import unittest
from pathlib import Path

_START_DIR = ""


def _init_worker(start_dir: str) -> None:
    global _START_DIR
    _START_DIR = start_dir
    if start_dir not in sys.path:
        sys.path.insert(0, start_dir)


def collect_test_ids(start_dir: Path, pattern: str) -> list[str]:
    """Discover all individual unittest test IDs as 'module.Class.method'."""
    loader = unittest.TestLoader()
    suite = loader.discover(str(start_dir), pattern=pattern)
    ids: list[str] = []

    def walk(node):
        if isinstance(node, unittest.TestSuite):
            for child in node:
                walk(child)
        else:
            ids.append(node.id())

    walk(suite)
    return ids


def run_batch(test_ids: list[str]) -> tuple[list[str], int, int, int, int, str]:
    """Run a small batch of test IDs in a worker process."""
    loader = unittest.TestLoader()
    suite = loader.loadTestsFromNames(test_ids)
    buf = io.StringIO()
    runner = unittest.TextTestRunner(stream=buf, verbosity=1)
    result = runner.run(suite)
    return (
        test_ids,
        result.testsRun,
        len(result.skipped),
        len(result.failures),
        len(result.errors),
        buf.getvalue(),
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-s", "--start-directory", required=True, help="Directory to discover tests in")
    parser.add_argument("-p", "--pattern", default="test*.py", help="Test filename pattern")
    parser.add_argument("-j", "--jobs", type=int, default=os.cpu_count() or 4, help="Worker processes")
    parser.add_argument("--batch-size", type=int, default=4, help="Tests per task batch")
    args = parser.parse_args()

    start_dir = Path(args.start_directory).resolve()
    test_ids = collect_test_ids(start_dir, args.pattern)
    if not test_ids:
        print(f"No tests discovered in {start_dir}", file=sys.stderr)
        return 1

    batch_size = max(1, args.batch_size)
    batches = [test_ids[i : i + batch_size] for i in range(0, len(test_ids), batch_size)]
    workers = max(1, min(args.jobs, len(batches)))

    t0 = time.monotonic()
    total_run = 0
    total_skipped = 0
    total_failures = 0
    total_errors = 0
    failed_outputs: list[tuple[list[str], str]] = []

    with concurrent.futures.ProcessPoolExecutor(
        max_workers=workers,
        initializer=_init_worker,
        initargs=(str(start_dir),),
    ) as pool:
        for batch, tests_run, skipped, failures, errors, output in pool.map(run_batch, batches):
            total_run += tests_run
            total_skipped += skipped
            total_failures += failures
            total_errors += errors
            if failures or errors:
                failed_outputs.append((batch, output))

    elapsed = time.monotonic() - t0
    for batch, output in failed_outputs:
        print(f"\n=== FAIL batch ({batch[0]} ..) ===", file=sys.stderr)
        print(output, file=sys.stderr)

    status = "OK" if (total_failures == 0 and total_errors == 0) else "FAILED"
    extra = []
    if total_failures:
        extra.append(f"failures={total_failures}")
    if total_errors:
        extra.append(f"errors={total_errors}")
    if total_skipped:
        extra.append(f"skipped={total_skipped}")
    suffix = f" ({', '.join(extra)})" if extra else ""
    print(f"Ran {total_run} tests in {elapsed:.3f}s ({workers} workers)")
    print(f"{status}{suffix}")
    return 0 if (total_failures == 0 and total_errors == 0) else 1


if __name__ == "__main__":
    sys.exit(main())

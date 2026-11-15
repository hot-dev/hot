#!/usr/bin/env python3
"""
Compare two results files written by `run-all.py --save`.

Usage: ./compare.py BASELINE.json CANDIDATE.json [--lang Hot] [--threshold 15]

For each benchmark both files ran, prints the warm and cold times and the
change from BASELINE to CANDIDATE. A warm change beyond the threshold (in
percent) is marked SLOWER or FASTER. Exits 1 when any benchmark got slower
past the threshold, so a script can use it as a check.

Times are only comparable from the same machine under similar load: the
machine and load average of each file are printed first.
"""

import argparse
import json
import sys
from pathlib import Path


def load(path: str) -> dict:
    document = json.loads(Path(path).read_text())
    if document.get("format") != 1:
        sys.exit(f"{path}: not a run-all.py results file (format {document.get('format')!r})")
    return document


def format_ns(ns) -> str:
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


def change(old, new) -> str:
    if not old or new is None:
        return "-"
    return f"{(new - old) / old * 100:+.1f}%"


def describe(label: str, path: str, document: dict) -> None:
    machine = document.get("machine", {})
    load_avg = machine.get("load_average")
    load_text = f", load {load_avg[0]:.2f}" if load_avg else ""
    print(f"{label}: {path}")
    print(f"  {document['hot']['version']}, {document['created_at']}, runs {document.get('runs')}")
    print(f"  {machine.get('cpu') or machine.get('machine')}, {machine.get('platform')}{load_text}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("baseline")
    parser.add_argument("candidate")
    parser.add_argument("--lang", default="Hot", help="language to compare (default Hot)")
    parser.add_argument("--threshold", type=float, default=15.0, help="percent change to flag (default 15)")
    args = parser.parse_args()

    base = load(args.baseline)
    cand = load(args.candidate)
    describe("baseline ", args.baseline, base)
    describe("candidate", args.candidate, cand)
    if base.get("machine", {}).get("cpu") != cand.get("machine", {}).get("cpu"):
        print("  warning: the files come from different CPUs, so the times aren't comparable")
    print()

    names = sorted(
        name
        for name in set(base["benchmarks"]) & set(cand["benchmarks"])
        if args.lang in base["benchmarks"][name] and args.lang in cand["benchmarks"][name]
    )
    if not names:
        sys.exit(f"No benchmarks ran in {args.lang} in both files.")

    width = max(len(n) for n in names)
    header = f"{args.lang + ' benchmark':<{width}}  {'warm before':>11}  {'warm after':>10}  {'change':>8}  {'cold before':>11}  {'cold after':>10}  {'change':>8}"
    print(header)
    print("-" * len(header))
    slower = []
    for name in names:
        old = base["benchmarks"][name][args.lang]
        new = cand["benchmarks"][name][args.lang]
        flag = ""
        if old["warm_ns"]:
            pct = (new["warm_ns"] - old["warm_ns"]) / old["warm_ns"] * 100
            if pct > args.threshold:
                flag = "  SLOWER"
                slower.append(name)
            elif pct < -args.threshold:
                flag = "  FASTER"
        if old["result"] != new["result"]:
            flag += f"  result changed: {old['result']} -> {new['result']}"
        print(
            f"{name:<{width}}  {format_ns(old['warm_ns']):>11}  {format_ns(new['warm_ns']):>10}  "
            f"{change(old['warm_ns'], new['warm_ns']):>8}  {format_ns(old['cold_ns']):>11}  "
            f"{format_ns(new['cold_ns']):>10}  {change(old['cold_ns'], new['cold_ns']):>8}{flag}"
        )

    old_start = base.get("startup_ns", {}).get(args.lang)
    new_start = cand.get("startup_ns", {}).get(args.lang)
    if old_start and new_start:
        print()
        print(f"startup: {format_ns(old_start)} -> {format_ns(new_start)} ({change(old_start, new_start)})")

    only_base = sorted(set(base["benchmarks"]) - set(cand["benchmarks"]))
    only_cand = sorted(set(cand["benchmarks"]) - set(base["benchmarks"]))
    if only_base:
        print(f"\nOnly in baseline: {', '.join(only_base)}")
    if only_cand:
        print(f"\nOnly in candidate: {', '.join(only_cand)}")

    if slower:
        print(f"\n{len(slower)} benchmark(s) more than {args.threshold:g}% slower: {', '.join(slower)}")
        sys.exit(1)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Summarize a saved OpenTofu plan for the selfhost-plan workflow.

    plan_summary.py plan.json SUMMARY DELETIONS --durable PATTERN

Reads `tofu show -json` output and writes two files, each empty when there
is nothing to list. SUMMARY gets every change the plan makes: resource
actions, imports, moves, and root output changes, one `actions address` per
line. DELETIONS gets the deletions and replacements of durable resources,
whose addresses match PATTERN. Only actions and addresses are written:
plans can contain sensitive values.
"""
import argparse
import json
import re
import sys


def describe(change):
    """`actions address` for one resource change, or None for a no-op that
    neither imports nor moves anything."""
    actions = change["change"]["actions"]
    verbs = [] if actions == ["no-op"] else list(actions)
    if change["change"].get("importing"):
        verbs.insert(0, "import")
    address = change["address"]
    previous = change.get("previous_address")
    if previous and previous != address:
        verbs.append("move")
        address = f"{address} (from {previous})"
    return f"{'/'.join(verbs)} {address}" if verbs else None


def changes(plan):
    lines = [line for line in map(describe, plan.get("resource_changes") or []) if line]
    for name, change in sorted((plan.get("output_changes") or {}).items()):
        if change["actions"] != ["no-op"]:
            lines.append(f"{'/'.join(change['actions'])} output.{name}")
    return lines


def durable_deletions(plan, pattern):
    durable = re.compile(pattern)
    return [f"{'/'.join(c['change']['actions'])} {c['address']}" for c in plan.get("resource_changes") or []
            if "delete" in c["change"]["actions"] and durable.search(c["address"])]


def write(path, lines):
    with open(path, "w") as handle:
        handle.write("".join(line + "\n" for line in lines))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("plan")
    parser.add_argument("summary")
    parser.add_argument("deletions")
    parser.add_argument("--durable", required=True, help="address pattern of durable resources")
    args = parser.parse_args(argv)
    with open(args.plan) as handle:
        plan = json.load(handle)
    write(args.summary, changes(plan))
    write(args.deletions, durable_deletions(plan, args.durable))
    return 0


if __name__ == "__main__":
    sys.exit(main())

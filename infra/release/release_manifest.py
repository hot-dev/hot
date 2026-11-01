#!/usr/bin/env python3
"""Write the self-host release manifest for one Hot release.

The release workflow runs this after pushing the runtime image, then signs
the output with cosign. Installations deploy only manifests whose signature
verifies (see infra/runtime/render.py).
"""
import argparse
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
DIGEST = re.compile(r"^[a-z0-9][a-z0-9._:/-]*@sha256:[a-f0-9]{64}$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$")
# Bumped when installation inputs change incompatibly.
CONFIGURATION_SCHEMA = 1
# The oldest release this one upgrades from directly; the deploy agent refuses
# larger jumps. Raise it when a release drops an upgrade path.
UPGRADE_POLICY = Path(__file__).with_name("upgrade-policy.json")


def version_key(version):
    core, _, pre = version.partition("-")
    return tuple(int(part) for part in core.split(".")), pre == "", pre


def latest_migration(directory):
    numbers = [int(path.name.split("_", 1)[0]) for path in directory.glob("[0-9]*_*.sql")]
    if not numbers:
        raise ValueError(f"No migrations in {directory}")
    return max(numbers)


def build(release, hot_image, upstream, migrations_root=ROOT / "resources/db", policy=None):
    if not VERSION.fullmatch(release):
        raise ValueError(f"Release {release!r} is not a version such as 3.0.1")
    policy = json.loads(UPGRADE_POLICY.read_text()) if policy is None else policy
    minimum = policy.get("minimum_source", "")
    if not VERSION.fullmatch(minimum) or version_key(minimum) > version_key(release):
        raise ValueError(f"Upgrade policy minimum_source {minimum!r} must be a version no newer than {release}")
    images = {"hot": hot_image, **upstream}
    if set(images) != {"hot", "postgres", "valkey", "ingress"}:
        raise ValueError("Expected hot, postgres, valkey, and ingress images")
    for name, image in images.items():
        if not DIGEST.fullmatch(image):
            raise ValueError(f"{name} image {image!r} is not pinned by sha256 digest")
    postgres = latest_migration(migrations_root / "postgres/migrations")
    sqlite = latest_migration(migrations_root / "sqlite/migrations")
    if postgres != sqlite:
        raise ValueError(f"PostgreSQL ({postgres}) and SQLite ({sqlite}) migrations disagree")
    return {
        "schema_version": 1,
        "release": release,
        "module_tag": "v" + release,
        "configuration_schema": CONFIGURATION_SCHEMA,
        "migrations": {"latest": postgres},
        "upgrade": {"minimum_source": minimum},
        "images": images,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", required=True, help="Hot version, without the v")
    parser.add_argument("--hot-image", required=True, help="Runtime image pinned by digest")
    parser.add_argument("--upstream", type=Path, default=Path(__file__).with_name("upstream-images.json"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        manifest = build(args.release, args.hot_image, json.loads(args.upstream.read_text()))
        args.output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    except (OSError, ValueError) as error:
        print(f"Release manifest error: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"manifest": str(args.output), "release": manifest["release"]}))
    return 0


if __name__ == "__main__":
    sys.exit(main())

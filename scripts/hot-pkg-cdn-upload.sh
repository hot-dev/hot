#!/bin/bash
# Upload CDN packages to S3 and invalidate CloudFront cache
#
# This script is designed to run in CI after hot-pkg-cdn.sh generates the output.
# Only packages listed in hot/pkg/pkg-publish.txt are included in the upload.
#
# Required environment variables:
#   PACKAGE_CDN_BUCKET or S3_BUCKET - target S3 bucket
#
# Optional environment variables:
#   CDN_OUTPUT_DIR - defaults to "./cdn-output"
#   CLOUDFRONT_DISTRIBUTION_ID - the CloudFront distribution to invalidate
#   PACKAGE_CDN_BASE_URL - printed after upload when set
#
# Authentication:
#   CI should configure AWS credentials via OIDC before running this script.
#   Local runs may use an AWS profile or standard AWS_* environment variables.
#   The role needs s3:GetObject, s3:PutObject and s3:ListBucket on the bucket,
#   and cloudfront:CreateInvalidation on the distribution (plus
#   cloudfront:GetInvalidation with WAIT_FOR_INVALIDATION=true). HeadObject
#   needs s3:GetObject, and without s3:ListBucket S3 answers 403 instead of
#   404 for a missing key: either way the script stops rather than guess.
#
# Usage:
#   ./hot-pkg-cdn-upload.sh [cdn-output-dir]
#
# Example CI workflow:
#   ./scripts/hot-pkg-cdn.sh     # Respects pkg-publish.txt
#   ./scripts/hot-pkg-cdn-upload.sh

set -euo pipefail

CDN_OUTPUT_DIR="${1:-./cdn-output}"
S3_BUCKET="${PACKAGE_CDN_BUCKET:-${S3_BUCKET:-}}"
CLOUDFRONT_DISTRIBUTION_ID="${CLOUDFRONT_DISTRIBUTION_ID:-}"
PACKAGE_CDN_BASE_URL="${PACKAGE_CDN_BASE_URL:-}"

# Validate inputs
if [ ! -d "$CDN_OUTPUT_DIR" ]; then
    echo "Error: CDN output directory not found: $CDN_OUTPUT_DIR"
    echo "Run hot-pkg-cdn.sh first to generate the output."
    exit 1
fi

if [ -z "$S3_BUCKET" ]; then
    echo "Error: target bucket is required."
    echo "Set PACKAGE_CDN_BUCKET or S3_BUCKET before running this script."
    exit 1
fi

if [ -z "$CLOUDFRONT_DISTRIBUTION_ID" ]; then
    echo "Warning: CLOUDFRONT_DISTRIBUTION_ID not set, skipping cache invalidation"
fi

echo "=== Checking published versions ==="
# A published <package>/<version>.tar.gz is immutable: users who pin that
# version must keep getting the same package. Each build regenerates every
# tarball, so compare contents (not bytes, which carry timestamps) with the
# published copy. Identical: skip it. Different: fail, because the package
# changed without a version bump.
#
# Use one paginated S3 listing for the namespace prefix plus parallel workers
# for tarball comparisons and versions.json fetches, avoiding hundreds of
# sequential AWS CLI startups across all published packages.
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
EXCLUDES_FILE="$WORK_DIR/sync_excludes.txt"

python3 - "$CDN_OUTPUT_DIR" "$S3_BUCKET" "$WORK_DIR" "$EXCLUDES_FILE" <<'PY'
import concurrent.futures
import json
import os
import re
import subprocess
import sys
import tarfile
from pathlib import Path

cdn_root = Path(sys.argv[1]).resolve()
bucket = sys.argv[2]
work_dir = Path(sys.argv[3])
excludes_file = Path(sys.argv[4])

tarballs = sorted(cdn_root.rglob("*.tar.gz"))
versions_files = sorted(cdn_root.rglob("versions.json"))
all_keys = [
    p.relative_to(cdn_root).as_posix()
    for p in (*tarballs, *versions_files)
]
top_prefixes = sorted(
    {
        (k.rsplit("/", 2)[0] + "/") if k.count("/") >= 2 else (k.split("/", 1)[0] + "/")
        for k in all_keys
        if "/" in k
    }
)


def list_bucket_keys(prefix: str) -> set[str]:
    proc = subprocess.run(
        [
            "aws", "s3api", "list-objects-v2",
            "--bucket", bucket,
            "--prefix", prefix,
            "--query", "Contents[].Key",
            "--output", "json",
        ],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        err = (proc.stderr or proc.stdout).strip()
        print(f"Error: could not list s3://{bucket}/{prefix}: {err}", file=sys.stderr)
        if "(403)" in err or "AccessDenied" in err:
            print(
                "S3 answers 403 when the role lacks s3:ListBucket or s3:GetObject.",
                file=sys.stderr,
            )
        sys.exit(1)
    data = json.loads(proc.stdout)
    return set(data or [])


existing_keys: set[str] = set()
for prefix in top_prefixes:
    existing_keys |= list_bucket_keys(prefix)


def tar_contents(path: Path) -> dict[str, bytes | str]:
    """Read a tarball's entries (excluding docs.json) into a comparable dict."""
    entries: dict[str, bytes | str] = {}
    with tarfile.open(path, "r:gz") as tf:
        for member in tf.getmembers():
            name = member.name.lstrip("./")
            if not name or os.path.basename(name) == "docs.json":
                continue
            if member.isdir():
                entries[name] = "<dir>"
            elif member.issym():
                entries[name] = f"<symlink:{member.linkname}>"
            elif member.isfile():
                extracted = tf.extractfile(member)
                entries[name] = extracted.read() if extracted else b""
    return entries


def check_tarball(tarball: Path) -> tuple[str, str]:
    key = tarball.relative_to(cdn_root).as_posix()
    if key.startswith("releases/beta/") or key not in existing_keys:
        return ("new", key)
    safe_name = key.replace("/", "_")
    published_path = work_dir / f"pub_{safe_name}"
    proc = subprocess.run(
        ["aws", "s3", "cp", "--quiet", f"s3://{bucket}/{key}", str(published_path)],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        err = (proc.stderr or proc.stdout).strip()
        raise RuntimeError(f"could not read s3://{bucket}/{key}: {err}")
    same = tar_contents(published_path) == tar_contents(tarball)
    published_path.unlink(missing_ok=True)
    return ("same" if same else "changed", key)


# Map package prefix -> sorted published tarball versions from the single listing.
tarball_re = re.compile(r"^([^/]+)\.tar\.gz$")
published_by_prefix: dict[str, list[str]] = {}
for k in existing_keys:
    if "/" not in k:
        continue
    prefix, filename = k.rsplit("/", 1)
    m = tarball_re.match(filename)
    if m:
        published_by_prefix.setdefault(prefix, []).append(m.group(1))


def semver_key(v: str) -> list[int]:
    out = []
    for part in v.split("."):
        try:
            out.append(int(part))
        except ValueError:
            out.append(0)
    return out


def merge_versions_file(versions_file: Path) -> tuple[str, str]:
    key = versions_file.relative_to(cdn_root).as_posix()
    prefix = key.rsplit("/versions.json", 1)[0]
    remote_versions: list[str] = []
    if key in existing_keys:
        safe_name = key.replace("/", "_")
        remote_path = work_dir / f"ver_{safe_name}"
        proc = subprocess.run(
            ["aws", "s3", "cp", "--quiet", f"s3://{bucket}/{key}", str(remote_path)],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            err = (proc.stderr or proc.stdout).strip()
            raise RuntimeError(f"could not read s3://{bucket}/{key}: {err}")
        remote_versions = json.loads(remote_path.read_text()).get("versions") or []
        remote_path.unlink(missing_ok=True)

    local_versions = json.loads(versions_file.read_text()).get("versions") or []
    listed_versions = published_by_prefix.get(prefix, [])
    merged = sorted(set(remote_versions) | set(listed_versions) | set(local_versions), key=semver_key)
    latest = merged[-1] if merged else None
    versions_file.write_text(json.dumps({"latest": latest, "versions": merged}, indent=2) + "\n")
    return (key, str(latest))


max_workers = min(16, max(4, (os.cpu_count() or 4) * 2))
excludes: list[str] = []
changed: list[str] = []
with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
    for status, key in pool.map(check_tarball, tarballs):
        if status == "same":
            excludes.append(key)
        elif status == "changed":
            changed.append(key)

if changed:
    print("Error: these package versions are already published with different contents:")
    for key in changed:
        print(f"  {key}")
    print("Bump each package's version in its pkg.hot before releasing.")
    sys.exit(1)

excludes_file.write_text("\n".join(excludes) + ("\n" if excludes else ""))

with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
    for key, latest in pool.map(merge_versions_file, versions_files):
        print(f"  {key}: latest {latest}")
PY

SYNC_EXCLUDES=()
while IFS= read -r key; do
    [ -n "$key" ] && SYNC_EXCLUDES+=(--exclude "$key")
done < "$EXCLUDES_FILE"

echo "=== Uploading to S3 ==="
echo "Source: $CDN_OUTPUT_DIR"
echo "Bucket: s3://$S3_BUCKET"
echo ""

# Sync to S3. Published tarballs with unchanged contents are excluded above;
# everything else (new versions and docs) is uploaded. No --size-only: a
# regenerated file the same size as the published one must still replace it.
aws s3 sync "$CDN_OUTPUT_DIR" "s3://$S3_BUCKET" \
    --exclude ".DS_Store" \
    --exclude "*.tmp" \
    --exclude "*/versions.json" \
    "${SYNC_EXCLUDES[@]+"${SYNC_EXCLUDES[@]}"}"

# Upload every versions.json explicitly in parallel, so a merged list always
# replaces the published one.
python3 - "$CDN_OUTPUT_DIR" "$S3_BUCKET" <<'PY'
import concurrent.futures
import os
import subprocess
import sys
from pathlib import Path

cdn_root = Path(sys.argv[1]).resolve()
bucket = sys.argv[2]
versions_files = sorted(cdn_root.rglob("versions.json"))


def upload(versions_file: Path) -> None:
    key = versions_file.relative_to(cdn_root).as_posix()
    proc = subprocess.run(
        [
            "aws", "s3", "cp", "--quiet",
            str(versions_file),
            f"s3://{bucket}/{key}",
            "--content-type", "application/json",
        ],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"failed to upload {key}: {(proc.stderr or proc.stdout).strip()}")


max_workers = min(16, max(4, (os.cpu_count() or 4) * 2))
with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
    list(pool.map(upload, versions_files))
PY

echo ""
echo "✓ S3 sync complete"

# Collect paths that need invalidation
# These are the mutable files that might be cached by CloudFront
INVALIDATION_PATHS=()

# Find all versions.json files (these are updated when new versions are published)
while IFS= read -r -d '' file; do
    # Convert local path to CDN path
    rel_path="${file#"$CDN_OUTPUT_DIR"/}"
    INVALIDATION_PATHS+=("/$rel_path")
done < <(find "$CDN_OUTPUT_DIR" -name "versions.json" -print0)

# If we had latest.txt files, we'd add them here too
# while IFS= read -r -d '' file; do
#     rel_path="${file#"$CDN_OUTPUT_DIR"/}"
#     INVALIDATION_PATHS+=("/$rel_path")
# done < <(find "$CDN_OUTPUT_DIR" -name "latest.txt" -print0)

# Invalidate CloudFront cache if distribution ID is set
if [ -n "$CLOUDFRONT_DISTRIBUTION_ID" ] && [ ${#INVALIDATION_PATHS[@]} -gt 0 ]; then
    echo ""
    echo "=== Invalidating CloudFront Cache ==="
    echo "Distribution: $CLOUDFRONT_DISTRIBUTION_ID"
    echo "Paths to invalidate:"
    for path in "${INVALIDATION_PATHS[@]}"; do
        echo "  $path"
    done
    echo ""

    # Create invalidation
    INVALIDATION_ID=$(aws cloudfront create-invalidation \
        --distribution-id "$CLOUDFRONT_DISTRIBUTION_ID" \
        --paths "${INVALIDATION_PATHS[@]}" \
        --query 'Invalidation.Id' \
        --output text)

    echo "✓ Invalidation created: $INVALIDATION_ID"

    # Optionally wait for invalidation to complete
    if [ "${WAIT_FOR_INVALIDATION:-false}" = "true" ]; then
        echo "Waiting for invalidation to complete..."
        aws cloudfront wait invalidation-completed \
            --distribution-id "$CLOUDFRONT_DISTRIBUTION_ID" \
            --id "$INVALIDATION_ID"
        echo "✓ Invalidation complete"
    fi
else
    if [ ${#INVALIDATION_PATHS[@]} -eq 0 ]; then
        echo ""
        echo "No mutable files found, skipping invalidation"
    fi
fi

echo ""
echo "=== Upload Complete ==="
if [ -n "$PACKAGE_CDN_BASE_URL" ]; then
    echo "Packages available at: $PACKAGE_CDN_BASE_URL"
fi

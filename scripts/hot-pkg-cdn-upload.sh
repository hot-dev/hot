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
#   The role needs s3:GetObject, s3:PutObject and s3:ListBucket on the bucket.
#   Without s3:ListBucket, S3 answers 403 instead of 404 for a missing key,
#   and the script stops rather than guess.
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

# Succeed when the key exists and fail when S3 says it doesn't. Any other
# error (no permission, no network) stops the script: treating it as "missing"
# would re-upload published tarballs and skip the versions.json merge.
s3_key_exists() {
    local key="$1" error
    if error=$(aws s3api head-object --bucket "$S3_BUCKET" --key "$key" 2>&1 >/dev/null); then
        return 0
    fi
    if [[ "$error" == *"(404)"* || "$error" == *"Not Found"* || "$error" == *"NoSuchKey"* ]]; then
        return 1
    fi
    echo "Error: could not read s3://$S3_BUCKET/$key: $error" >&2
    if [[ "$error" == *"(403)"* ]]; then
        echo "S3 answers 403 for a missing key when the role lacks s3:ListBucket." >&2
    fi
    exit 1
}

# Every version with a published tarball under a package's CDN directory.
published_versions() {
    local prefix="$1"
    aws s3api list-objects-v2 --bucket "$S3_BUCKET" --prefix "$prefix/" \
        --query 'Contents[].Key' --output json |
        jq -r --arg prefix "$prefix/" '(. // [])[] | ltrimstr($prefix) | select(test("^[^/]+\\.tar\\.gz$")) | rtrimstr(".tar.gz")'
}

echo "=== Checking published versions ==="
# A published <package>/<version>.tar.gz is immutable: users who pin that
# version must keep getting the same package. Each build regenerates every
# tarball, so compare contents (not bytes, which carry timestamps) with the
# published copy. Identical: skip it. Different: fail, because the package
# changed without a version bump.
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
SYNC_EXCLUDES=()
CHANGED=()
while IFS= read -r -d '' tarball; do
    key="${tarball#"$CDN_OUTPUT_DIR"/}"
    case "$key" in
        releases/beta/*) continue ;; # beta builds may republish a version
    esac
    if ! s3_key_exists "$key"; then
        continue # a new version
    fi
    rm -rf "$WORK_DIR/published" "$WORK_DIR/built"
    mkdir -p "$WORK_DIR/published" "$WORK_DIR/built"
    aws s3 cp --quiet "s3://$S3_BUCKET/$key" "$WORK_DIR/published.tar.gz"
    tar -xzf "$WORK_DIR/published.tar.gz" -C "$WORK_DIR/published"
    tar -xzf "$tarball" -C "$WORK_DIR/built"
    if diff -r -q -x docs.json "$WORK_DIR/published" "$WORK_DIR/built" > /dev/null; then
        SYNC_EXCLUDES+=(--exclude "$key")
    else
        CHANGED+=("$key")
    fi
done < <(find "$CDN_OUTPUT_DIR" -name '*.tar.gz' -print0)

if [ "${#CHANGED[@]}" -gt 0 ]; then
    echo "Error: these package versions are already published with different contents:"
    for key in "${CHANGED[@]}"; do
        echo "  $key"
    done
    echo "Bump each package's version in its pkg.hot before releasing."
    exit 1
fi

# versions.json lists every published version. A build lists only the current
# one, so rebuild it from the tarballs the bucket holds, the published
# versions.json and this build. Listing the tarballs also repairs a
# versions.json that an earlier upload left behind.
while IFS= read -r -d '' versions_file; do
    key="${versions_file#"$CDN_OUTPUT_DIR"/}"
    prefix="${key%/versions.json}"
    echo '{"versions": []}' > "$WORK_DIR/versions.json"
    if s3_key_exists "$key"; then
        aws s3 cp --quiet "s3://$S3_BUCKET/$key" "$WORK_DIR/versions.json"
    fi
    published_versions "$prefix" | jq -R . | jq -s '{versions: .}' > "$WORK_DIR/tarballs.json"
    jq -s '
        def semver: split(".") | map(tonumber? // 0);
        (map(.versions // []) | add | unique | sort_by(semver)) as $all
        | {latest: ($all | last), versions: $all}
    ' "$WORK_DIR/versions.json" "$WORK_DIR/tarballs.json" "$versions_file" > "$WORK_DIR/merged.json"
    mv "$WORK_DIR/merged.json" "$versions_file"
    echo "  $key: latest $(jq -r .latest "$versions_file")"
done < <(find "$CDN_OUTPUT_DIR" -name 'versions.json' -print0)

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

# Upload every versions.json explicitly, so a merged list always replaces the
# published one.
while IFS= read -r -d '' versions_file; do
    key="${versions_file#"$CDN_OUTPUT_DIR"/}"
    aws s3 cp --quiet "$versions_file" "s3://$S3_BUCKET/$key" --content-type application/json
done < <(find "$CDN_OUTPUT_DIR" -name 'versions.json' -print0)

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

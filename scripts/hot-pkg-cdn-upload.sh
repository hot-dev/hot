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
#
# Usage:
#   ./hot-pkg-cdn-upload.sh [cdn-output-dir]
#
# Example CI workflow:
#   ./scripts/hot-pkg-cdn.sh     # Respects pkg-publish.txt
#   ./scripts/hot-pkg-cdn-upload.sh

set -e

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
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT
SYNC_EXCLUDES=()
CHANGED=()
while IFS= read -r -d '' tarball; do
    key="${tarball#$CDN_OUTPUT_DIR/}"
    case "$key" in
        releases/beta/*) continue ;; # beta builds may republish a version
    esac
    if ! aws s3api head-object --bucket "$S3_BUCKET" --key "$key" > /dev/null 2>&1; then
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
# one, so merge it with the published list rather than replace it.
while IFS= read -r -d '' versions_file; do
    key="${versions_file#$CDN_OUTPUT_DIR/}"
    if aws s3 cp --quiet "s3://$S3_BUCKET/$key" "$WORK_DIR/versions.json" 2>/dev/null; then
        jq -s '
            def semver: split(".") | map(tonumber? // 0);
            ((.[0].versions // []) + (.[1].versions // []) | unique | sort_by(semver)) as $all
            | {latest: ($all | last), versions: $all}
        ' "$WORK_DIR/versions.json" "$versions_file" > "$WORK_DIR/merged.json"
        mv "$WORK_DIR/merged.json" "$versions_file"
    fi
done < <(find "$CDN_OUTPUT_DIR" -name 'versions.json' -print0)

echo "=== Uploading to S3 ==="
echo "Source: $CDN_OUTPUT_DIR"
echo "Bucket: s3://$S3_BUCKET"
echo ""

# Sync to S3. Published tarballs with unchanged contents are excluded above;
# everything else (new versions, docs, versions.json) is uploaded.
aws s3 sync "$CDN_OUTPUT_DIR" "s3://$S3_BUCKET" \
    --size-only \
    --exclude ".DS_Store" \
    --exclude "*.tmp" \
    "${SYNC_EXCLUDES[@]}"

echo ""
echo "✓ S3 sync complete"

# Collect paths that need invalidation
# These are the mutable files that might be cached by CloudFront
INVALIDATION_PATHS=()

# Find all versions.json files (these are updated when new versions are published)
while IFS= read -r -d '' file; do
    # Convert local path to CDN path
    rel_path="${file#$CDN_OUTPUT_DIR/}"
    INVALIDATION_PATHS+=("/$rel_path")
done < <(find "$CDN_OUTPUT_DIR" -name "versions.json" -print0)

# If we had latest.txt files, we'd add them here too
# while IFS= read -r -d '' file; do
#     rel_path="${file#$CDN_OUTPUT_DIR/}"
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

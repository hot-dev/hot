#!/usr/bin/env bash
# Build the self-host runtime image from this checkout.
#
#   infra/images/build-runtime.sh [TAG]
#
# The image is linux/amd64 only. On other hosts Docker emulates amd64, which
# makes the Rust build much slower than on an amd64 machine or CI runner.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tag="${1:-hot-runtime:dev}"

"$root/scripts/capture-git-revision.sh"
docker buildx build --platform linux/amd64 --load \
    -f "$root/infra/images/containers/runtime.Dockerfile" \
    -t "$tag" "$root"

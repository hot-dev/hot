#!/usr/bin/env bash
# check-actions-pinned.sh - fail when a workflow or composite action uses an
# external action that is not pinned to a full commit SHA.
#
# Pin as `uses: owner/repo@<40-hex sha> # vX.Y.Z`; the comment names the
# release the SHA points at. Local actions (./...) and docker:// images are
# skipped. CI runs this in .github/workflows/dependency-audit.yml.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FAILED=0

# Checks both `uses:` keys and `- uses:` list items (a step's first key),
# quoted or not.
while IFS=: read -r file line content; do
    use="${content#*uses:}"
    use="${use#"${use%%[![:space:]]*}"}"
    use="${use#[\"\']}"
    [[ "$use" == ./* || "$use" == docker://* ]] && continue

    ref="${use##*@}"
    ref="${ref%%[[:space:]#\"\']*}"
    if [[ ! "$ref" =~ ^[0-9a-f]{40}$ ]]; then
        echo "${file#"$ROOT/"}:${line}: external action is not pinned to a full commit SHA: $use" >&2
        FAILED=1
    fi
done < <(grep -R -n -E '^[[:space:]]*(-[[:space:]]+)?uses:' \
    "$ROOT/.github/workflows" "$ROOT/.github/actions" || true)

exit "$FAILED"

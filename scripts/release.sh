#!/usr/bin/env bash
set -euo pipefail

### Cut a Hot release from main.
###
### Usage: scripts/release.sh VERSION [options]
###
###   --check     Run the checks that can fail before anything changes, then stop
###   --yes       Skip the confirmation before pushing
###   -h, --help  Show this help
###
### Write these two first and leave them uncommitted. Nothing can generate them:
###
###   .github/release-notes/vVERSION.md
###   An "Upgrading from Hot A to B" section in resources/docs/migrations/index.md,
###   with the anchor {#upgrading-from-A-to-B}, dots written as dashes
###
### The script then:
###
###   1. Sets resources/version.txt, runs sync-version.sh, and runs cargo build
###   2. Moves the example operator workflows to vVERSION
###   3. Adds new migrations to resources/db/RELEASED_MIGRATIONS
###   4. Commits "Hot VERSION" and pushes main
###   5. Waits for the hot workflow to pass on that commit
###   6. Tags vVERSION and pushes the tag, which starts the Release workflow
###   7. Waits for the Release workflow and checks the self-host manifest
###
### The tag waits for CI because Release refuses a commit without a green hot
### run. If a step fails or is interrupted, fix it and run the same command
### again. It picks up from where it stopped.

usage() { sed -n 's/^### \{0,1\}//p' "$0"; }
die() { echo "" >&2; echo "Error: $*" >&2; exit 1; }
step() { echo ""; echo "==> $*"; }

repo=hot-dev/hot
version=""
assume_yes=false
check_only=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check) check_only=true ;;
        --yes|-y) assume_yes=true ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
        *)
            [[ -z "$version" ]] || { echo "Unexpected argument: $1" >&2; usage >&2; exit 1; }
            version="$1"
            ;;
    esac
    shift
done

[[ -n "$version" ]] || { usage >&2; exit 1; }
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "VERSION must look like 3.0.7, without a leading v."

cd "$(dirname "$0")/.."

tag="v$version"
notes=".github/release-notes/$tag.md"
guide="resources/docs/migrations/index.md"
released="resources/db/RELEASED_MIGRATIONS"
example_dir="infra/github/example-operator-repo/.github/workflows"
# hot_app's build regenerates this tracked file, and it's never committed.
generated="resources/app/assets/css/styles.css"

for tool in git gh cargo python3 perl; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed."
done

# Prints "id status conclusion" for the newest run of a workflow on a commit.
latest_run() {
    gh run list --repo "$repo" --workflow "$1" --commit "$2" --limit 1 \
        --json databaseId,status,conclusion \
        --jq '.[] | "\(.databaseId) \(.status) \(.conclusion)"'
}

# Waits for a run other than $3 to appear, and prints its id.
wait_for_run() {
    local attempt line id
    for attempt in $(seq 1 40); do
        line="$(latest_run "$1" "$2" || true)"
        id="${line%% *}"
        if [[ -n "$id" && "$id" != "${3:-}" ]]; then
            echo "$id"
            return 0
        fi
        sleep 15
    done
    return 1
}

# gh run watch can exit early on an API error, so ask for the actual result.
run_passed() {
    gh run watch "$1" --repo "$repo" --exit-status --interval 30 >/dev/null 2>&1 || true
    [[ "$(gh run view "$1" --repo "$repo" --json conclusion --jq .conclusion)" == success ]]
}

wait_for_release() {
    local sha="$1" before="${2:-}" id
    step "Waiting for the Release workflow"
    id="$(wait_for_run release.yml "$sha" "$before")" ||
        die "No Release run appeared for $tag. Check https://github.com/$repo/actions/workflows/release.yml"
    echo "https://github.com/$repo/actions/runs/$id"
    if ! run_passed "$id"; then
        echo "" >&2
        echo "The Release run did not pass. See why with:" >&2
        echo "  gh run view $id --repo $repo --log-failed" >&2
        echo "Once the cause is fixed, or if it was a dropped download or a similar blip:" >&2
        echo "  gh run rerun $id --repo $repo --failed" >&2
        echo "Then run this command again." >&2
        exit 1
    fi
    # create-release doesn't wait for the self-host image job, so a published
    # release alone doesn't mean the run finished.
    gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name' |
        grep -qx hot-selfhost-manifest.json ||
        die "$tag has no hot-selfhost-manifest.json. Check the Attach self-host manifest job."
    echo ""
    echo "Hot $version is released: https://github.com/$repo/releases/tag/$tag"
    echo ""
    echo "Still to do, as CONTRIBUTING.md describes: the hot-skills mirror and tag, any SDKs,"
    echo "and hot-cloud's hot-public.ref."
}

step "Checking the repository"
git fetch --quiet origin
[[ "$(git rev-parse --abbrev-ref HEAD)" == main ]] || die "Release from main."
git merge-base --is-ancestor origin/main HEAD ||
    die "Local main is behind or has diverged from origin/main. Pull first."
head_sha="$(git rev-parse HEAD)"

# A tag on this commit means an earlier run got as far as pushing it.
remote_tag="$(git ls-remote --tags origin "refs/tags/$tag" | cut -f1)"
if [[ -n "$remote_tag" ]]; then
    [[ "$remote_tag" == "$head_sha" ]] ||
        die "$tag already exists on another commit ($remote_tag). Release the next patch version instead."
    $check_only && die "$tag is already tagged on this commit."
    echo "$tag is already pushed on this commit."
    wait_for_release "$head_sha"
    exit 0
fi

if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
    die "GitHub release $tag already exists. Release the next patch version instead."
fi

previous_tag="$(git tag --list 'v[0-9]*' --sort=-v:refname --merged HEAD |
    awk -v skip="$tag" '/^v[0-9]+\.[0-9]+\.[0-9]+$/ && $0 != skip && !found { print; found = 1 }')"
[[ -n "$previous_tag" ]] || die "Could not find the previous release tag."
previous="${previous_tag#v}"
if [[ "$(printf '%s\n%s\n' "$previous" "$version" | sort -V | tail -1)" != "$version" ]]; then
    die "$version is not newer than the last release, $previous."
fi

# Compare with HEAD, so a run stopped before its commit still counts as a bump.
bumped=false
if [[ "$(git show HEAD:resources/version.txt | tr -d '[:space:]')" != "$version" ]]; then
    bumped=true
fi
interrupted=false
if $bumped && [[ "$(tr -d '[:space:]' < resources/version.txt)" == "$version" ]]; then
    interrupted=true
fi

# Only the hand-written files and the generated stylesheet may be dirty, along
# with the files this script edits if an earlier run stopped before committing.
dirty=""
while IFS= read -r line; do
    path="${line:3}"
    case "$path" in
        "$notes"|"$guide"|"$generated") continue ;;
    esac
    if $interrupted; then
        case "$path" in
            resources/version.txt|Cargo.toml|Cargo.lock|"$released"|"$example_dir"/*|hot/pkg/*) continue ;;
        esac
    fi
    dirty="$dirty
  $path"
done < <(git status --porcelain --untracked-files=no)
[[ -z "$dirty" ]] || die "Commit or discard these changes first:$dirty"

step "Checking the release notes and upgrade guide"
[[ -s "$notes" ]] || die "Write $notes first. Use the sections in the last release's notes: Upgrade, Features, Fixes, Behaviour changes."
anchor="upgrading-from-${previous//./-}-to-${version//./-}"
grep -q "{#$anchor}" "$guide" || die "Add this section to $guide first:
  ## Upgrading from Hot $previous to $version {#$anchor}
State which migrations to run, or that there are none."

# Republishing a package under an existing version fails the CDN upload.
step "Checking packages changed since $previous_tag"
package_version() { grep -m1 -E '^[[:space:]]*"?version"?:' | tr -d ' ",'; }
stale=""
while read -r pkg; do
    [[ -z "$pkg" || "$pkg" == \#* || "$pkg" == hot-std ]] && continue
    git cat-file -e "$previous_tag:hot/pkg/$pkg/pkg.hot" 2>/dev/null || continue
    git diff --quiet "$previous_tag" -- "hot/pkg/$pkg" && continue
    if [[ "$(git show "$previous_tag:hot/pkg/$pkg/pkg.hot" | package_version)" == "$(package_version < "hot/pkg/$pkg/pkg.hot")" ]]; then
        stale="$stale $pkg"
    fi
done < hot/pkg/pkg-publish.txt
[[ -z "$stale" ]] || die "These packages changed without a version bump:$stale"
python3 scripts/check-pkg-pins.py

if $check_only; then
    echo ""
    echo "Ready to release $version."
    exit 0
fi

step "Setting the version to $version"
printf '%s\n' "$version" > resources/version.txt
scripts/sync-version.sh
if $bumped; then
    # Updates the workspace crate versions in Cargo.lock.
    cargo build
fi

step "Moving the example operator workflows to $tag"
perl -i -pe "s{^( +uses: hot-dev/hot/\\.github/workflows/selfhost-\\w+\\.yml@)\\S+\$}{\${1}$tag}; s{^( +hot_version: )\\S+\$}{\${1}$tag}" "$example_dir"/*.yml

step "Recording new migrations in $released"
python3 - "$released" <<'PY'
import hashlib
import pathlib
import sys

released = pathlib.Path(sys.argv[1])
lines = released.read_text().splitlines()
for backend in ("postgres", "sqlite"):
    listed = {line.split(" ", 1)[0] for line in lines}
    for path in sorted((released.parent / backend / "migrations").glob("*.sql")):
        name = f"{backend}/{path.name}"
        if name in listed:
            continue
        digest = hashlib.sha384(path.read_bytes()).hexdigest()
        last = max(i for i, line in enumerate(lines) if line.startswith(f"{backend}/"))
        lines.insert(last + 1, f"{name} {digest}")
        print(f"  added {name}")
released.write_text("\n".join(lines) + "\n")
PY

step "Running the checks CI would fail on"
scripts/check-version-sync.sh
python3 -m unittest discover -s infra/github/tests -p 'test_workflows.py'

# Named paths, so the regenerated stylesheet stays out of the commit.
git add resources/version.txt Cargo.toml Cargo.lock "$notes" "$guide" "$released" "$example_dir"
git add -u hot/pkg
if ! git diff --cached --quiet; then
    step "Committing"
    if $bumped; then
        git commit -m "Hot $version"
    else
        git commit -m "Prepare the $tag release"
    fi
fi
sha="$(git rev-parse HEAD)"

if [[ "$sha" != "$(git rev-parse origin/main)" ]]; then
    echo ""
    echo "Ready to publish Hot $version:"
    git log --format='  %h %s' origin/main..HEAD
    git show --stat --format= HEAD | sed 's/^/  /'
    if ! $assume_yes; then
        echo ""
        read -r -p "Push to main? [y/N] " reply || reply=""
        if [[ "$reply" != [yY]* ]]; then
            echo "Stopped before pushing. Run this command again to continue."
            exit 1
        fi
    fi
    step "Pushing main"
    git push origin HEAD:refs/heads/main
fi

step "Waiting for CI on $(git rev-parse --short "$sha")"
ci="$(wait_for_run hot.yml "$sha")" || die "No hot workflow run appeared for $sha."
echo "https://github.com/$repo/actions/runs/$ci"
if ! run_passed "$ci"; then
    echo "" >&2
    echo "CI failed, so $tag was not pushed and nothing is published. See why with:" >&2
    echo "  gh run view $ci --repo $repo --log-failed" >&2
    echo "For a flaky test:" >&2
    echo "  gh run rerun $ci --repo $repo --failed" >&2
    echo "For a real failure, commit the fix to main. Then run this command again." >&2
    exit 1
fi

# The previous Release run on this commit, so a new one can be told apart.
before="$(latest_run release.yml "$sha" || true)"
before="${before%% *}"

step "Tagging $tag"
if existing="$(git rev-parse -q --verify "refs/tags/$tag")"; then
    [[ "$existing" == "$sha" ]] || die "Local tag $tag points at $existing. Delete it with: git tag -d $tag"
else
    git tag "$tag" "$sha"
fi
# By name: --tags would also push any other local tag.
git push origin "refs/tags/$tag"

wait_for_release "$sha" "$before"

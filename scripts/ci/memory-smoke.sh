#!/usr/bin/env bash
# memory-smoke.sh - run the memory smoke fixtures with a built hot binary
#
# Usage:
#   ./scripts/ci/memory-smoke.sh HOT_BIN
#
# Each fixture in scripts/ci/memory-smoke/ starts with `// expect: ok` or
# `// expect: error TEXT`. Every fixture runs interpreted and with every
# function JIT-compiled on its first call (--jit.threshold 1). One that should
# pass must exit 0; one that should fail must exit 1 with TEXT in its output.
# A signal (exit 128+N: SIGSEGV, SIGABRT, an OOM kill) always fails.
#
# The nightly workflow runs this script inside a container capped at 1 GiB,
# with MALLOC_CHECK_=3 so glibc aborts on heap corruption.
#
# TODO(nightly): the review also asked for a crashing fixture with a bystander:
# start `hot worker` under the same cap with a Postgres-backed queue, send one
# event whose handler is killed (OOM or a crash) and one ordinary event, and
# assert the ordinary run completes once the worker restarts. That needs the
# worker, a queue and a supervisor in the job; this script covers the
# single-process cases.

set -uo pipefail

hot_bin="$(cd "$(dirname "${1:?usage: $0 HOT_BIN}")" && pwd)/$(basename "$1")"
fixtures="$(cd "$(dirname "${BASH_SOURCE[0]}")/memory-smoke" && pwd)"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

failures=0
for fixture in "$fixtures"/*.hot; do
    expect="$(head -1 "$fixture" | sed -n 's|^// expect: ||p')"
    for threshold in 100 1; do
        name="$(basename "$fixture") (JIT threshold $threshold)"
        log="$workdir/run.log"
        # Run a copy outside the checkout, so the checkout's hot.hot project
        # isn't loaded with the fixture.
        cp "$fixture" "$workdir/fixture.hot"
        (cd "$workdir" && "$hot_bin" run --jit.threshold "$threshold" fixture.hot) > "$log" 2>&1
        status=$?
        problem=""
        if [[ $status -ge 128 ]]; then
            problem="killed by signal $((status - 128))"
        elif [[ "$expect" == ok && $status -ne 0 ]]; then
            problem="exited $status"
        elif [[ "$expect" == error\ * ]]; then
            if [[ $status -ne 1 ]]; then
                problem="exited $status, expected a Hot error"
            elif ! grep -qF "${expect#error }" "$log"; then
                problem="failed without '${expect#error }'"
            fi
        fi
        if [[ -n "$problem" ]]; then
            echo "FAIL $name: $problem"
            tail -20 "$log" | sed 's/^/    /'
            failures=$((failures + 1))
        else
            echo "ok   $name"
        fi
    done
done
exit $((failures > 0))

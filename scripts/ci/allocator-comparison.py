#!/usr/bin/env python3
"""Compare mimalloc with the system allocator: speed, peak memory, and memory
over a long run.

    allocator-comparison.py HOT_BIN [--quick] [--out FILE]

Run from the repository root. The binary must find Hot's resources (run it
from a build directory that has them, as the workflow sets up). Prints a
Markdown report, and appends it to FILE (the workflow passes the job summary).

One binary, three ways: on the system allocator (`HOT_ALLOCATOR=system`), on
the system allocator with `MALLOC_ARENA_MAX=4` (the limit a worker sets for
itself; it only means something for glibc), and on mimalloc, the default.
Each is measured in blocks, after a run that isn't counted, and the fastest
run of each is kept, since shared runners are noisy.
"""
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.getcwd()
QUICK = "--quick" in sys.argv
ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
OUT = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else None
if OUT in ARGS:
    ARGS.remove(OUT)
HOT_BIN = os.path.abspath(ARGS[0])

CONFIGS = [
    ("system", HOT_BIN, {"HOT_ALLOCATOR": "system"}),
    ("system, 4 arenas", HOT_BIN, {"HOT_ALLOCATOR": "system", "MALLOC_ARENA_MAX": "4"}),
    ("mimalloc", HOT_BIN, {}),
]
# ru_maxrss is kilobytes on Linux and bytes on macOS.
RSS_UNIT = 1024 if sys.platform.startswith("linux") else 1


def measure(name, args, cwd, rounds):
    """`args` under each configuration: fastest time, median peak memory.

    Each configuration is measured in a block, after one run that isn't
    counted. The blocks are run twice, the second time in the opposite
    order, so that drift in the machine doesn't favor one end.
    """
    times = {label: [] for label, _, _ in CONFIGS}
    peaks = {label: [] for label, _, _ in CONFIGS}
    failed = set()
    for order in (CONFIGS, CONFIGS[::-1]):
        for label, binary, env in order:
            timed(binary, args, cwd, env)
            for _ in range(rounds):
                seconds, peak, code = timed(binary, args, cwd, env)
                if code != 0:
                    failed.add(label)
                times[label].append(seconds)
                peaks[label].append(peak)
    return {
        "name": name,
        "rows": {
            label: {
                "seconds": min(times[label]),
                "peak": statistics.median(peaks[label]),
                "failed": label in failed,
            }
            for label, _, _ in CONFIGS
        },
    }


def timed(binary, args, cwd, extra_env, timeout=1800):
    """One run, with the child's own peak memory: (seconds, peak bytes, exit code)."""
    env = dict(os.environ, HOT_TEST_VAR="set so that :hot.env/vars() is not empty", **extra_env)
    start = time.perf_counter()
    proc = subprocess.Popen(
        [binary, *args], cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    deadline = start + timeout
    while True:
        pid, status, usage = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            break
        if time.perf_counter() > deadline:
            proc.kill()
        time.sleep(0.002)
    elapsed = time.perf_counter() - start
    code = os.waitstatus_to_exitcode(status)
    proc.returncode = code
    return elapsed, usage.ru_maxrss * RSS_UNIT, code


BENCH_LINE = re.compile(r"^(.+?):\s*(\d+(?:\.\d+)?)\s*ms\b")


def benchmarks(extra_args, rounds):
    """The benchmark suite's rows, fastest of `rounds`, per configuration."""
    cwd = os.path.join(ROOT, "benchmarks")
    rows = {label: {} for label, _, _ in CONFIGS}
    for label, binary, env in CONFIGS:
        for _ in range(rounds + 1):
            out = subprocess.run(
                [binary, "run", "--emitter.type", "none", *extra_args, "hot/src/benchmarks/bench.hot"],
                cwd=cwd,
                env=dict(os.environ, **env),
                capture_output=True,
                text=True,
                timeout=1800,
            ).stdout
            for line in out.splitlines():
                match = BENCH_LINE.match(line.strip())
                if match:
                    name, ms = match.group(1), float(match.group(2))
                    rows[label][name] = min(ms, rows[label].get(name, ms))
    return rows


PROGRAMS = {
    "Map a helper over 120,000 maps": """::alloc::records ns
score fn (m: Map): Int { add(mul(get(m, "a"), 3), length(get(m, "tags"))) }
items map(range(3000), (i) { {a: i, tags: [i, i]} })
go fn (n: Int): Int { length(map(items, (m) { score(m) })) }
repeat fn (k: Int, acc: Int): Int { if(lte(k, 0), acc, repeat(sub(k, 1), add(acc, go(3000)))) }
println(repeat(40, 0))
""",
    "Build 120,000 small maps": """::alloc::literals ns
go fn (n: Int): Int { length(map(range(n), (i) { {a: i, tags: [i, i]} })) }
repeat fn (k: Int, acc: Int): Int { if(lte(k, 0), acc, repeat(sub(k, 1), add(acc, go(3000)))) }
println(repeat(40, 0))
""",
    "240,000 string appends": """::alloc::strings ns
build fn (n: Int): Int { length(reduce(range(n), (acc, x) { concat(acc, "x") }, "")) }
loop fn (i: Int, n: Int, acc: Str): Int { if(gte(i, n), length(acc), loop(add(i, 1), n, concat(acc, "y"))) }
repeat fn (k: Int, t: Int): Int { if(lte(k, 0), t, repeat(sub(k, 1), add(t, add(build(2000), loop(0, 2000, ""))))) }
println(repeat(60, 0))
""",
}

# One long process: every round, 32 `pmap` workers each build 400 records with
# strings and nested vectors and keep a third of them. The round before stays
# alive until the next one is built, so short and long lives interleave.
SOAK = """::alloc::soak ns
make-record fn (i: Int): Map {
    {id: i, name: `item-${i}`, tags: map(range(mod(i, 17)), (t) { `tag-${t}` }), score: mul(i, 3)}
}
batch fn (seed: Int): Vec {
    rows map(range(400), (i) { make-record(add(seed, i)) })
    filter(rows, (r) { eq(mod(get(r, "id"), 3), 0) })
}
round fn (n: Int): Vec { pmap(range(32), (k) { batch(add(mul(n, 1000), k)) }) }
soak fn (n: Int, held: Vec, total: Int): Int {
    if(lte(n, 0), total, soak(sub(n, 1), round(n), add(total, length(held))))
}
println(soak(ROUNDS, [], 0))
"""


def resident_bytes(pid):
    try:
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
        ).stdout.strip()
        return int(out) * 1024 if out else None
    except (OSError, ValueError):
        return None


def soak(binary, extra_env, rounds, directory):
    """Resident memory through one long run: samples every second."""
    path = os.path.join(directory, "soak.hot")
    with open(path, "w") as handle:
        handle.write(SOAK.replace("ROUNDS", str(rounds)))
    start = time.perf_counter()
    proc = subprocess.Popen(
        [binary, "run", "--emitter.type", "none", "soak.hot"],
        cwd=directory,
        env=dict(os.environ, **extra_env),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    samples = []
    while proc.poll() is None:
        value = resident_bytes(proc.pid)
        if value:
            samples.append(value)
        time.sleep(1)
    return {"seconds": time.perf_counter() - start, "samples": samples, "code": proc.returncode}


def quarter_means(samples):
    """The mean of each quarter of the run, skipping the warm-up tenth."""
    body = samples[len(samples) // 10:]
    if len(body) < 4:
        return [statistics.mean(body)] * 4 if body else [0] * 4
    size = len(body) // 4
    return [statistics.mean(body[i * size:(i + 1) * size]) for i in range(4)]


def mb(value):
    return f"{value / (1024 * 1024):.0f} MB"


def ratio(value, base):
    return f"{value / base:.2f}x" if base else "-"


def main():
    lines = ["# mimalloc vs the system allocator", ""]
    uname = os.uname()
    lines.append(
        f"`{uname.sysname} {uname.machine}`, {os.cpu_count()} CPUs. Fastest of several runs with "
        "warm caches; peak memory is the median."
    )
    lines.append("")
    raw = {"timings": [], "benchmarks": {}, "soak": {}}
    empty = tempfile.mkdtemp(prefix="alloc-empty-")
    work = tempfile.mkdtemp(prefix="alloc-work-")

    # The test projects need their databases once.
    subprocess.run(
        [HOT_BIN, "init", "-c", "hot.test.hot", "--ctx", "hot/ci.ctx.hot"],
        cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    suite = ["test", "-c", "hot.test.hot", "--ctx", "hot/ci.ctx.hot"]
    packages = ["test", "-p", "hot-pkg-all", "--emitter.type", "none"]
    cases = [
        ("`hot eval 1`", ["eval", "1"], empty, 2 if QUICK else 6),
        ("`hot test`", suite, ROOT, 1 if QUICK else 2),
        ("`hot test`, JIT off", [*suite, "--jit.mode", "disabled"], ROOT, 1 if QUICK else 2),
        ("`hot test -p hot-pkg-all`", packages, ROOT, 1 if QUICK else 2),
        ("`hot test -p hot-pkg-all`, JIT off", [*packages, "--jit.mode", "disabled"], ROOT, 1 if QUICK else 2),
    ]
    for index, (title, source) in enumerate(PROGRAMS.items()):
        directory = os.path.join(work, f"program-{index}")
        os.makedirs(directory)
        with open(os.path.join(directory, "a.hot"), "w") as handle:
            handle.write(source)
        program = ["run", "--emitter.type", "none", "a.hot"]
        cases.append((title, program, directory, 1 if QUICK else 3))
        cases.append((f"{title}, JIT off", [*program, "--jit.mode", "disabled"], directory, 1 if QUICK else 3))

    labels = [label for label, _, _ in CONFIGS]
    lines += ["## Time and peak memory", "", "| | " + " | ".join(labels) + " | mimalloc / system |", "|---|" + "---|" * (len(labels) + 1)]
    for title, args, cwd, rounds in cases:
        result = measure(title, args, cwd, rounds)
        raw["timings"].append(result)
        cells = []
        for label in labels:
            row = result["rows"][label]
            mark = " (failed)" if row["failed"] else ""
            cells.append(f"{row['seconds']:.2f} s, {mb(row['peak'])}{mark}")
        base, new = result["rows"]["system"], result["rows"]["mimalloc"]
        cells.append(f"time {ratio(new['seconds'], base['seconds'])}, memory {ratio(new['peak'], base['peak'])}")
        lines.append(f"| {title} | " + " | ".join(cells) + " |")
        print(lines[-1], flush=True)
    lines.append("")

    lines += ["## Benchmark suite", ""]
    for mode, extra in (("JIT on", []), ("JIT off", ["--jit.mode", "disabled"])):
        rows = benchmarks(extra, 2 if QUICK else 4)
        raw["benchmarks"][mode] = rows
        base, new = rows["system"], rows["mimalloc"]
        shared = [name for name in base if name in new and base[name] >= 5]
        slower = [name for name in shared if new[name] > base[name] * 1.10]
        faster = [name for name in shared if new[name] < base[name] * 0.90]
        total_base = sum(base[name] for name in shared)
        total_new = sum(new[name] for name in shared)
        lines.append(
            f"- **{mode}:** {len(shared)} rows of 5 ms or more. Total {total_base:.0f} ms system, "
            f"{sum(rows['system, 4 arenas'].get(name, 0) for name in shared):.0f} ms with 4 arenas, "
            f"{total_new:.0f} ms mimalloc ({ratio(total_new, total_base)}). "
            f"{len(faster)} more than 10% faster, {len(slower)} more than 10% slower"
            + (f": {', '.join(slower)}." if slower else ".")
        )
        print(lines[-1], flush=True)
    lines.append("")

    # Size the soak from a short run, to last about the target under each.
    target = 20 if QUICK else 240
    probe = soak(HOT_BIN, {}, 20, work)
    rounds = max(20, int(20 * target / max(probe["seconds"], 0.5)))
    lines += [
        "## Memory through a long run",
        "",
        f"One process, {rounds} rounds of 32 parallel workers building and dropping records, "
        f"sized to run about {target} s. Resident memory sampled each second; the first tenth is left out.",
        "",
        "| | Time | Quarter 1 | Quarter 2 | Quarter 3 | Quarter 4 | Peak | Last / first quarter |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for label, binary, env in CONFIGS:
        # A short run first, so the long one starts with this binary's caches.
        soak(binary, env, 5, work)
        result = soak(binary, env, rounds, work)
        raw["soak"][label] = result
        samples = result["samples"]
        quarters = quarter_means(samples)
        mark = "" if result["code"] == 0 else " (failed)"
        lines.append(
            f"| {label}{mark} | {result['seconds']:.0f} s | "
            + " | ".join(mb(q) for q in quarters)
            + f" | {mb(max(samples) if samples else 0)} | {ratio(quarters[3], quarters[0])} |"
        )
        print(lines[-1], flush=True)
    lines += [
        "",
        "A last quarter well above the first means memory was still climbing when the run ended.",
        "",
    ]

    report = "\n".join(lines) + "\n"
    print(report)
    if OUT:
        with open(OUT, "a") as handle:
            handle.write(report)
    print("RAW " + json.dumps(raw))


if __name__ == "__main__":
    main()

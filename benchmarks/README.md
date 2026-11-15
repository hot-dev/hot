# Hot Language Benchmarks

Benchmarks comparing Hot against Python, JavaScript, and TypeScript, plus
Hot-only regression guards.

## Benchmarks Included

These run in all four languages and must compute the same result in each;
`run-all.py` checks that they do.

1. **Fibonacci** - Recursive and iterative implementations
2. **Collection Operations** - Map, filter, reduce over arrays
3. **String Operations** - Concatenation, manipulation
4. **JSON Processing** - Parse and serialize flat records
5. **Prime Numbers** - Trial division
6. **Records** - `records-reshape(2000)`: build orders with a nested customer and line items, then price each one with a spread, overrides and a nested spread, in `bench-records.hot`
7. **Realistic JSON** - `json-api-roundtrip(1500)`: parse a JSON:API-style payload of about 900KB, drop cancelled orders, summarize the rest and serialize them, in `bench-apijson.hot`. Every language builds the same payload byte for byte; the result includes its length
8. **Decimal Money** - `money-dec(10000)`: unit price × quantity, a discount, and 8.25% tax, each rounded to cents half away from zero, in `bench-money.hot`. Python uses `decimal`. JavaScript has no decimal type, so `bench.js` and `bench.ts` use exact integers in units of 1e-8, which makes them much faster than a decimal library would be: compare Hot with Python here
9. **Counting into a Map** - `word-count(50000)`: split a text, count each word into a Map inside a reduce, and keep the top 10, in `bench-words.hot`
10. **Lambdas** - `closure-capture`, `fn-as-value` and `nested-hof`: lambdas the pipeline fusion doesn't cover, in `bench-lambdas.hot`

Hot-only:

1. **Typed Records** - `records-typed(2000)`: the orders of `records-reshape` constructed as typed values with `where` checks, in `bench-records.hot`
2. **JIT Lazy/Flow Safety Shapes** - JIT safety microbenchmarks
3. **Deep Recursion** - Regression guards: tail recursion through `if()` (TCO / JIT native loop) and non-tail `cond` recursion (re-entrant register correctness — check the printed result, not just the time)
4. **Constraints** - The cost of `where` checks at function boundaries, in `bench-constraints.hot`
5. **Large Reads** - Regression guard: reading a large Map, Vec or Bytes must cost the same at every size, in `bench-large-reads.hot`. Compare the lines for each `n`; they should not grow with it
6. **Pipelines After JIT Tier-Up** - Regression guard: `compiled-sum-even-squares(10000)` is the shape of `sum-even-squares`, timed after `compile-sum-even-squares` compiles its function. It should cost about the same as the warm `sum-even-squares`, and about 10x more means the pipeline stopped being fused once compiled.

## How Benchmarks Are Timed

Every runner times a benchmark the same way, with the same counts:

- **cold**: the first call, alone. In Hot this runs in the interpreter.
- **warm**: after `warmup` calls in all, the median of 5 samples, where a
  sample is the mean time of `iterations` calls. Most warm-ups pass Hot's JIT
  threshold of 100 calls, so warm times are usually compiled code.

Times are printed in whole nanoseconds:

```
records-reshape(2000): cold=13687000ns warm=9054600ns (result: 3410450)
```

Inputs that aren't part of what a benchmark measures, such as the JSON payload
and the word-count text, are built once before the timings.

## Running the Benchmarks

All commands should be run from within the `benchmarks/` directory.

The runner can use:

- `HOT_BIN=/path/to/hot` or `--hot-bin /path/to/hot`
- the local public build at `../target/release/hot`
- the system-installed `hot` with `--sys`

Build the local CLI:

```bash
cd ..
cargo build --release -p hot_cli
cd benchmarks
./run-all.py
```

The Python runner invokes Hot with `--emitter.type none` and runs from the
`benchmarks/` directory. The local `hot.hot` also pins benchmark runtime
settings (`emitter.type none`, `queue.type none`, empty `db.uri`) so project
runtime services from `../hot.hot` are not enabled during benchmarks.

### Run All

```bash
./run-all.py --sys
```

prints each language's lines, then tables of warm times, warm times relative
to the fastest language, cold times, the Hot-only benchmarks, and startup
time. It ends by saying whether the languages agree on every result, and
exits 2 when they don't.

Options:

- `--runs N` runs each language's suite N times and reports the median of each time
- `--startup-runs N` sets how many times each language launches a hello-world program from `startup/` (default 10, after one untimed launch; 0 skips it)
- `--save [PATH]` writes the results as JSON, by default to `results/hot-<version>-<timestamp>.json`. The file records the Hot version, the other languages' versions, the CPU and the load average

### Comparing Runs

```bash
./run-all.py --sys --runs 3 --save results/before.json
# switch hot versions or rebuild
./run-all.py --hot-bin ../target/release/hot --runs 3 --save results/after.json
./compare.py results/before.json results/after.json
```

`compare.py` prints the warm and cold change for each Hot benchmark (or
another language with `--lang`) and marks changes beyond `--threshold`
percent (default 15) as SLOWER or FASTER. It exits 1 when anything got slower
past the threshold.

Times move from run to run, by 10–20% on a busy machine, so compare files
from the same machine under similar load, and check the noise first: compare
two runs of the same binary, and treat changes of that size as noise.
`results/` is ignored by git.

### Single Languages

`hot run` loads every file under `hot/src`, so this command (and `run-all.py`)
also runs `bench-constraints.hot`, `bench-fib-jit.hot` and
`bench-large-reads.hot`. Their lines print before the main Hot results. The
constraints benchmark reports microseconds per call. It has no equivalent in
the other languages, so it stays out of the comparison tables.

```bash
hot run --emitter.type none hot/src/benchmarks/bench.hot
python3 bench.py
node bench.js
npx tsx bench.ts
```

## Adding a Benchmark

Write it in `bench.hot` (or a new file under `hot/src/benchmarks/` with its
own namespace, called from `bench.hot`), `bench.py`, `bench.js` and
`bench.ts`, and give every `measure` call the same name, warm-up and
iteration counts. Make the result something all four languages can print the
same way, such as an Int or a Str, so `run-all.py` can check it. A Hot-only
benchmark goes in `bench.hot` alone.

# Hot Language Benchmarks

Benchmarks comparing Hot against Python, JavaScript, and TypeScript for common operations.

## Benchmarks Included

1. **Fibonacci** - Recursive and iterative implementations
2. **Collection Operations** - Map, filter, reduce over arrays
3. **String Operations** - Concatenation, manipulation
4. **JSON Processing** - Parse and serialize
5. **Prime Numbers** - Trial division
6. **JIT Lazy/Flow Safety Shapes** - Hot-only JIT safety microbenchmarks

## Running the Benchmarks

All commands should be run from within the `benchmarks/` directory.

The runner can use:

- `HOT_BIN=/path/to/hot` or `--hot-bin /path/to/hot`
- the local public build at `../target/release/hot`
- the system-installed `hot` with `--sys`

Build the local CLI:

```bash
cd ..
cargo build --release -p hot
cd benchmarks
./run-all.py
```

The Python runner invokes Hot with `--emitter.type none` and runs from the
`benchmarks/` directory. The local `hot.hot` also pins benchmark runtime
settings (`emitter.type none`, `queue.type none`, empty `db.uri`) so project
runtime services from `../hot.hot` are not enabled during benchmarks.

### Hot

```bash
hot run --emitter.type none hot/src/benchmarks/bench.hot
```

### Python

```bash
python3 bench.py
```

### JavaScript (Node.js)

```bash
node bench.js
```

### TypeScript

```bash
npx tsx bench.ts
```

### Run All

```bash
./run-all.py
```

## Results

Results are printed to stdout showing execution time in milliseconds for each benchmark.

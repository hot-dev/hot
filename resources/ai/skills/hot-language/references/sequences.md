# Hot Sequences Reference

Hot provides two approaches for working with sequences: **eager collection functions** for immediate processing and **lazy iterators** for streaming and large data.

## Eager Collection Functions

Process entire collections at once. Best for small/medium data where you need all results.

### map / pmap

Transform each element:

```hot
numbers [1, 2, 3, 4, 5]
doubled map(numbers, (x) { mul(x, 2) })  // [2, 4, 6, 8, 10]

// pmap runs transformations in parallel (good for I/O)
results pmap(urls, (url) { ::http/get(url) })
```

### filter

Keep elements matching a predicate:

```hot
numbers [1, 2, 3, 4, 5]
evens filter(numbers, (x) { eq(mod(x, 2), 0) })  // [2, 4]
```

### reduce

Fold collection to a single value:

```hot
numbers [1, 2, 3, 4, 5]
sum reduce(numbers, (acc, x) { add(acc, x) }, 0)  // 15
```

### Other Collection Functions

```hot
first([1, 2, 3])       // 1
last([1, 2, 3])        // 3
rest([1, 2, 3])        // [2, 3]
length([1, 2, 3])      // 3
reverse([1, 2, 3])     // [3, 2, 1]
sort([3, 1, 2])        // [1, 2, 3]
concat([1, 2], [3, 4]) // [1, 2, 3, 4]
flatten([[1, 2], [3]]) // [1, 2, 3]
distinct([1, 2, 2, 3]) // [1, 2, 3]
```

### Collection Pipelines

Chain operations with the pipe operator:

```hot
result [1, 2, 3, 4, 5]
    |> map((x) { mul(x, 2) })      // [2, 4, 6, 8, 10]
    |> filter((x) { gt(x, 5) })    // [6, 8, 10]
    |> reduce((a, x) { add(a, x) }, 0)  // 24
```

## Lazy Iterators

Process on-demand. Best for large data, streaming, or when you don't need all results.

### Creating Iterators

```hot
// From collection
it Iter([1, 2, 3, 4, 5])

// From range (eager, returns Vec)
numbers range(1, 100)  // [1, 2, ..., 99]
```

Note: `Iter`, `next`, `collect`, `for-each`, `take`, and `range` are core functions available globally.

### The Next Type

Calling `next(iter)` returns a `Next` record:

```hot
it Iter([1, 2, 3])

n1 next(it)  // {value: 1, done: false}
n2 next(it)  // {value: 2, done: false}
n3 next(it)  // {value: 3, done: false}
n4 next(it)  // {value: null, done: true}
```

### Consuming Iterators

```hot
// Collect remaining values into vector
it Iter([1, 2, 3, 4, 5])
next(it)           // consume first
rest collect(it)   // [2, 3, 4, 5]

// Take first n values
it Iter([1, 2, 3, 4, 5])
first-three take(it, 3)  // [1, 2, 3]

// Execute function for each (uses TCO, stack-safe)
it Iter(items)
for-each(it, (item) { process(item) })
```

### Range Functions

Hot has two range functions:

**Eager `range` (core)** — Returns a Vec immediately. Use for most cases.

```hot
range(5)          // [0, 1, 2, 3, 4]
range(1, 6)       // [1, 2, 3, 4, 5]
range(0, 10, 2)   // [0, 2, 4, 6, 8]
range(10, 0, -1)  // [10, 9, 8, 7, 6, 5, 4, 3, 2, 1]
```

**Lazy `::hot::iter/range`** — Returns an iterator. Use for very large ranges where you don't need all values at once.

```hot
::iter ::hot::iter

// Doesn't allocate a million-element array
big-range ::iter/range(1, 1000001)

// Only processes values as needed
sum reduce(big-range, add, 0)
```

**When to use each:**

| Function | Returns | Memory | Use When |
|----------|---------|--------|----------|
| `range(1, 100)` | `Vec` | Allocates all values | Small/medium ranges, need random access |
| `::hot::iter/range(1, 1000000)` | `Iter` | On-demand | Very large ranges, streaming, memory-sensitive |

### Combining Iterators with Collections

Convert collections to iterators for lazy processing:

```hot
// Create iterator from range, take first 10 even squares
first-10-even-squared range(1, 1000)
    |> Iter
    |> (it) { filter(collect(it), (x) { eq(mod(x, 2), 0) }) }
    |> (it) { map(it, (x) { mul(x, x) }) }
    |> (it) { take(it, 10) }
```

## When to Use Each

### Use Eager Collections When:

- Data fits comfortably in memory
- You need all results
- Processing is fast
- Code clarity is priority

```hot
// Good for small lists
users filter(all-users, (u) { u.active })
```

### Use Lazy Iterators When:

- Data is large or unbounded
- You only need some results
- Streaming from external source
- Memory efficiency matters

```hot
// Sum numbers 1 to 100
sum reduce(range(1, 101), add, 0)  // 5050

// Good for streaming responses
response ::http/request-stream("GET", url, {}, "", "sse")
for-each(response.body, (event) { process(event) })
```

## Lazy Arguments and Iterators

The `for-each` function uses lazy evaluation internally to enable tail-call optimization (TCO). This makes it stack-safe for any iteration depth:

```hot
// Stack-safe iteration over collections
for-each(range(1, 1001), (n) {
    process(n)
})
```

## Map Operations

Iterate over maps with 2-arity functions:

```hot
scores {alice: 100, bob: 95, carol: 88}

// map with (key, value) callback
formatted map(scores, (name, score) { `${name}: ${score}` })
// ["alice: 100", "bob: 95", "carol: 88"]

// filter with (key, value) predicate
high-scores filter(scores, (name, score) { gte(score, 90) })
// [["alice", 100], ["bob", 95]]

// keys and vals
keys(scores)   // ["alice", "bob", "carol"]
vals(scores)   // [100, 95, 88]
```

## Summary

| Approach | When to Use | Memory | Examples |
|----------|-------------|--------|----------|
| Eager (`map`, `filter`, `reduce`, `range`) | Small/medium data, need all results | Allocates full result | `map(range(1, 100), fn)` |
| Lazy (`Iter`, `next`, `for-each`) | Streaming, partial results | On-demand | `for-each(Iter(data), fn)` |
| Parallel (`pmap`) | I/O-bound operations | Allocates full result | `pmap(urls, fetch)` |

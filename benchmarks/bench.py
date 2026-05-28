#!/usr/bin/env python3
"""Python benchmarks for comparison with Hot language."""

import time
import json
import math
from typing import Callable, Any


def fib_recursive(n: int) -> int:
    """Naive recursive fibonacci."""
    if n <= 1:
        return n
    return fib_recursive(n - 1) + fib_recursive(n - 2)


def fib_iterative(n: int) -> int:
    """Iterative fibonacci."""
    if n <= 1:
        return n
    a, b = 0, 1
    for _ in range(2, n + 1):
        a, b = b, a + b
    return b


def sum_even_squares(n: int) -> int:
    """Sum of squares of even numbers in range 1 to n."""
    return sum(x * x for x in range(1, n + 1) if x % 2 == 0)


def collection_benchmark(n: int) -> int:
    """Map-filter-reduce chain."""
    data = list(range(1, n + 1))
    mapped1 = [x * 3 for x in data]
    filtered = [x for x in mapped1 if x > 100]
    mapped2 = [x + 1 for x in filtered]
    return sum(mapped2)


def string_concat_benchmark(n: int) -> str:
    """Build a string through concatenation."""
    result = ""
    for i in range(n):
        result += f"item-{i}-"
    return result


def json_benchmark(n: int) -> int:
    """Create, serialize, and parse JSON objects."""
    data = [
        {
            "id": i,
            "name": f"User {i}",
            "active": i % 2 == 0,
            "scores": [i * 10, i * 20, i * 30],
        }
        for i in range(n)
    ]
    json_str = json.dumps(data)
    parsed = json.loads(json_str)
    return len(parsed)


def is_prime(n: int) -> bool:
    """Check if n is prime using trial division."""
    if n <= 1:
        return False
    if n <= 3:
        return True
    if n % 2 == 0:
        return False
    limit = int(math.sqrt(n)) + 1
    for d in range(3, limit, 2):
        if n % d == 0:
            return False
    return True


def count_primes(n: int) -> int:
    """Count primes up to n."""
    return sum(1 for x in range(2, n + 1) if is_prime(x))


def measure(name: str, f: Callable[[], Any]) -> dict:
    """Measure execution time of a function."""
    start = time.perf_counter()
    result = f()
    end = time.perf_counter()
    elapsed = (end - start) * 1000
    print(f"{name}: {elapsed:.2f}ms (result: {result})")
    return {"name": name, "elapsed": elapsed, "result": result}


def run_benchmarks():
    """Run all benchmarks."""
    print("=== Python Benchmarks ===")
    print()

    results = [
        measure("fib-recursive(25)", lambda: fib_recursive(25)),
        measure("fib-iterative(70)", lambda: fib_iterative(70)),
        measure("sum-even-squares(10000)", lambda: sum_even_squares(10000)),
        measure("collection-benchmark(10000)", lambda: collection_benchmark(10000)),
        measure("string-concat(1000)", lambda: len(string_concat_benchmark(1000))),
        measure("json-benchmark(1000)", lambda: json_benchmark(1000)),
        measure("count-primes(1000)", lambda: count_primes(1000)),
    ]

    print()
    print("=== Benchmarks Complete ===")
    return results


if __name__ == "__main__":
    run_benchmarks()

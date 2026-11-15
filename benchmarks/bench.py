#!/usr/bin/env python3
"""Python benchmarks for comparison with Hot language."""

import time
import json
import math
from decimal import Decimal, ROUND_HALF_UP
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


# ============================================================================
# Records: building and reshaping (bench-records.hot)
# ============================================================================


def make_orders(n: int) -> list:
    orders = []
    for i in range(n):
        cid = i % 100
        orders.append({
            "id": i,
            "customer": {
                "id": cid,
                "name": f"Customer {cid}",
                "tier": "gold" if cid % 3 == 0 else "basic",
            },
            "items": [
                {"sku": f"SKU-{i % 7}", "qty": i % 5 + 1, "cents": (i % 13 + 1) * 100},
                {"sku": f"SKU-{(i + 3) % 7}", "qty": i % 3 + 1, "cents": (i % 11 + 1) * 250},
            ],
            "status": "open",
        })
    return orders


def price_order(order: dict) -> dict:
    total = sum(item["qty"] * item["cents"] for item in order["items"])
    return {
        **order,
        "status": "priced",
        "total": total,
        "customer": {**order["customer"], "vip": order["customer"]["tier"] == "gold"},
    }


def records_reshape(n: int) -> int:
    priced = [price_order(o) for o in make_orders(n)]
    return sum(o["total"] for o in priced if o["customer"]["vip"])


# ============================================================================
# Realistic JSON round trip (bench-apijson.hot)
# ============================================================================

STATUSES = ["open", "paid", "shipped", "cancelled"]
CITIES = ["Austin", "Denver", "Portland", "Chicago", "Boston", "Seattle"]


def make_order(i: int) -> dict:
    c = i % 500
    return {
        "id": f"ord_{i}",
        "type": "order",
        "attributes": {
            "number": 100000 + i,
            "status": STATUSES[i % 4],
            "created_at": f"2026-11-{1 + i % 28:02d}T{i % 24:02d}:00:00Z",
            "currency": "USD",
            "customer": {
                "id": f"cus_{c}",
                "email": f"user{c}@example.com",
                "name": f"Customer {c}",
                "address": {
                    "line1": f"{i % 900 + 1} Main Street",
                    "city": CITIES[i % 6],
                    "postcode": str(10000 + i % 8999),
                    "country": "US",
                },
            },
            "line_items": [
                {
                    "sku": f"SKU-{(i + j) % 50}",
                    "name": f"Product {(i + j) % 50}",
                    "qty": 1 + (i + j) % 3,
                    "unit_cents": 500 + ((i * 7 + j * 13) % 100) * 25,
                    "tags": [f"tag{j}", f"batch{i % 10}"],
                }
                for j in range(1 + i % 4)
            ],
            "notes": "Leave at front desk" if i % 3 == 0 else None,
            "gift": i % 5 == 0,
        },
        "relationships": {"store": {"id": f"store_{i % 12}"}},
    }


def to_json(value) -> str:
    """Compact JSON, the same bytes Hot's to-json and JSON.stringify write."""
    return json.dumps(value, separators=(",", ":"))


def make_payload(n: int) -> str:
    return to_json({
        "data": [make_order(i) for i in range(n)],
        "meta": {"page": 1, "per_page": n, "total": n},
    })


def summarize(order: dict) -> dict:
    a = order["attributes"]
    return {
        "id": order["id"],
        "number": a["number"],
        "email": a["customer"]["email"],
        "city": a["customer"]["address"]["city"],
        "items": len(a["line_items"]),
        "total_cents": sum(li["qty"] * li["unit_cents"] for li in a["line_items"]),
        "gift": a["gift"],
    }


def json_api_roundtrip(payload: str) -> str:
    doc = json.loads(payload)
    orders = [summarize(o) for o in doc["data"] if o["attributes"]["status"] != "cancelled"]
    out = to_json({"orders": orders, "count": len(orders)})
    return f"{len(payload)}/{len(out)}/{len(orders)}"


# ============================================================================
# Decimal money arithmetic (bench-money.hot)
# ============================================================================

D_BASE = Decimal("4.99")
D_STEP = Decimal("0.25")
D_DISCOUNT = Decimal("0.9")
D_TAX = Decimal("0.0825")
D_HUNDRED = Decimal(100)
D_ONE = Decimal(1)


def to_cents(amount: Decimal) -> int:
    """Round to whole cents half away from zero, as Hot's round does."""
    return int((amount * D_HUNDRED).quantize(D_ONE, rounding=ROUND_HALF_UP))


def money_dec(n: int) -> int:
    total = 0
    for i in range(n):
        unit = D_BASE + (i % 40) * D_STEP
        qty = i % 5 + 1
        line = unit * qty
        discounted = line * D_DISCOUNT if qty >= 4 else line
        total += to_cents(discounted) + to_cents(discounted * D_TAX)
    return total


# ============================================================================
# Counting into a dict (bench-words.hot)
# ============================================================================


def make_text(n: int) -> str:
    words = []
    for i in range(n):
        k = (i * 7919) % 1000
        words.append(f"w{k * k // 2000}")
    return " ".join(words)


def word_count(text: str) -> str:
    counts: dict = {}
    for w in text.split(" "):
        counts[w] = counts.get(w, 0) + 1
    top = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:10]
    return "|".join(f"{w}:{c}" for w, c in top)


# ============================================================================
# Lambdas: captures, functions as values, nested HOFs (bench-lambdas.hot)
# ============================================================================


def closure_capture(n: int) -> int:
    factor = n % 5 + 3
    offset = 7
    scaled = list(map(lambda x: x * factor + offset, range(n)))
    return sum(scaled)


def twice(f: Callable[[int], int], x: int) -> int:
    return f(f(x))


def inc(x: int) -> int:
    return x + 1


def fn_as_value(n: int) -> int:
    total = 0
    for i in range(n):
        total += twice(inc, i)
    return total


def nested_hof(n: int) -> int:
    rows = [sum(filter(lambda c: (r + c) % 3 > 0, range(100))) for r in range(n)]
    return sum(rows)


# ============================================================================
# Harness
# ============================================================================

# Every runner (bench.hot, bench.py, bench.js, bench.ts) times a benchmark the
# same way, with the same counts:
#   cold: the first call, alone.
#   warm: after `warmup` calls in all, the median of 5 samples, where a sample
#         is the mean time of `iterations` calls.
# Times are whole nanoseconds. run-all.py reads these lines.
SAMPLE_COUNT = 5


def measure(name: str, warmup: int, iterations: int, f: Callable[[], Any]) -> dict:
    start = time.perf_counter_ns()
    result = f()
    cold = time.perf_counter_ns() - start
    for _ in range(max(warmup - 1, 0)):
        f()
    samples = []
    for _ in range(SAMPLE_COUNT):
        sample_start = time.perf_counter_ns()
        for _ in range(iterations):
            f()
        samples.append((time.perf_counter_ns() - sample_start) // iterations)
    warm = sorted(samples)[SAMPLE_COUNT // 2]
    print(f"{name}: cold={cold}ns warm={warm}ns (result: {result})")
    return {"name": name, "cold": cold, "warm": warm, "result": result}


def run_benchmarks():
    """Run all benchmarks."""
    # Inputs built once, outside the timings.
    api_payload = make_payload(1500)
    text = make_text(50000)

    print("=== Python Benchmarks ===")
    print()

    results = [
        measure("fib-recursive(25)", 2, 3, lambda: fib_recursive(25)),
        measure("fib-iterative(70)", 100, 200, lambda: fib_iterative(70)),
        measure("sum-even-squares(10000)", 20, 50, lambda: sum_even_squares(10000)),
        measure("collection-benchmark(10000)", 20, 50, lambda: collection_benchmark(10000)),
        measure("string-concat(1000)", 20, 50, lambda: len(string_concat_benchmark(1000))),
        measure("json-benchmark(1000)", 5, 10, lambda: json_benchmark(1000)),
        measure("count-primes(1000)", 1, 2, lambda: count_primes(1000)),
        measure("records-reshape(2000)", 5, 10, lambda: records_reshape(2000)),
        measure("json-api-roundtrip(1500)", 3, 5, lambda: json_api_roundtrip(api_payload)),
        measure("money-dec(10000)", 3, 5, lambda: money_dec(10000)),
        measure("word-count(50000)", 3, 5, lambda: word_count(text)),
        measure("closure-capture(10000)", 10, 20, lambda: closure_capture(10000)),
        measure("fn-as-value(10000)", 10, 20, lambda: fn_as_value(10000)),
        measure("nested-hof(100)", 10, 20, lambda: nested_hof(100)),
    ]

    print()
    print("=== Benchmarks Complete ===")
    return results


if __name__ == "__main__":
    run_benchmarks()

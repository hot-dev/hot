#!/usr/bin/env ts-node
/**
 * TypeScript benchmarks for comparison with Hot language.
 */

function fibRecursive(n: number): number {
    if (n <= 1) return n;
    return fibRecursive(n - 1) + fibRecursive(n - 2);
}

function fibIterative(n: number): number {
    if (n <= 1) return n;
    let a = 0;
    let b = 1;
    for (let i = 2; i <= n; i++) {
        [a, b] = [b, a + b];
    }
    return b;
}

function sumEvenSquares(n: number): number {
    return Array.from({ length: n }, (_, i) => i + 1)
        .filter((x) => x % 2 === 0)
        .map((x) => x * x)
        .reduce((acc, x) => acc + x, 0);
}

function collectionBenchmark(n: number): number {
    return Array.from({ length: n }, (_, i) => i + 1)
        .map((x) => x * 3)
        .filter((x) => x > 100)
        .map((x) => x + 1)
        .reduce((acc, x) => acc + x, 0);
}

function stringConcatBenchmark(n: number): string {
    let result = "";
    for (let i = 0; i < n; i++) {
        result += `item-${i}-`;
    }
    return result;
}

interface UserData {
    id: number;
    name: string;
    active: boolean;
    scores: number[];
}

function jsonBenchmark(n: number): number {
    const data: UserData[] = Array.from({ length: n }, (_, i) => ({
        id: i,
        name: `User ${i}`,
        active: i % 2 === 0,
        scores: [i * 10, i * 20, i * 30],
    }));
    const jsonStr = JSON.stringify(data);
    const parsed: UserData[] = JSON.parse(jsonStr);
    return parsed.length;
}

function isPrime(n: number): boolean {
    if (n <= 1) return false;
    if (n <= 3) return true;
    if (n % 2 === 0) return false;
    const limit = Math.floor(Math.sqrt(n)) + 1;
    for (let d = 3; d < limit; d += 2) {
        if (n % d === 0) return false;
    }
    return true;
}

function countPrimes(n: number): number {
    let count = 0;
    for (let x = 2; x <= n; x++) {
        if (isPrime(x)) count++;
    }
    return count;
}

// ============================================================================
// Records: building and reshaping (bench-records.hot)
// ============================================================================

function makeOrders(n): any[] {
    const orders: any[] = [];
    for (let i = 0; i < n; i++) {
        const cid = i % 100;
        orders.push({
            id: i,
            customer: {
                id: cid,
                name: `Customer ${cid}`,
                tier: cid % 3 === 0 ? "gold" : "basic",
            },
            items: [
                { sku: `SKU-${i % 7}`, qty: (i % 5) + 1, cents: ((i % 13) + 1) * 100 },
                { sku: `SKU-${(i + 3) % 7}`, qty: (i % 3) + 1, cents: ((i % 11) + 1) * 250 },
            ],
            status: "open",
        });
    }
    return orders;
}

function priceOrder(order: any): any {
    const total = order.items.reduce((acc: number, item: any) => acc + item.qty * item.cents, 0);
    return {
        ...order,
        status: "priced",
        total,
        customer: { ...order.customer, vip: order.customer.tier === "gold" },
    };
}

function recordsReshape(n: number): number {
    return makeOrders(n)
        .map((o) => priceOrder(o))
        .filter((o) => o.customer.vip)
        .reduce((acc: number, o) => acc + o.total, 0);
}

// ============================================================================
// Realistic JSON round trip (bench-apijson.hot)
// ============================================================================

const STATUSES = ["open", "paid", "shipped", "cancelled"];
const CITIES = ["Austin", "Denver", "Portland", "Chicago", "Boston", "Seattle"];

function twoDigits(n: number): string {
    return String(n).padStart(2, "0");
}

function makeOrder(i: number): any {
    const c = i % 500;
    const lineItems: any[] = [];
    for (let j = 0; j < 1 + (i % 4); j++) {
        const k = (i + j) % 50;
        lineItems.push({
            sku: `SKU-${k}`,
            name: `Product ${k}`,
            qty: 1 + ((i + j) % 3),
            unit_cents: 500 + ((i * 7 + j * 13) % 100) * 25,
            tags: [`tag${j}`, `batch${i % 10}`],
        });
    }
    return {
        id: `ord_${i}`,
        type: "order",
        attributes: {
            number: 100000 + i,
            status: STATUSES[i % 4],
            created_at: `2026-11-${twoDigits(1 + (i % 28))}T${twoDigits(i % 24)}:00:00Z`,
            currency: "USD",
            customer: {
                id: `cus_${c}`,
                email: `user${c}@example.com`,
                name: `Customer ${c}`,
                address: {
                    line1: `${(i % 900) + 1} Main Street`,
                    city: CITIES[i % 6],
                    postcode: String(10000 + (i % 8999)),
                    country: "US",
                },
            },
            line_items: lineItems,
            notes: i % 3 === 0 ? "Leave at front desk" : null,
            gift: i % 5 === 0,
        },
        relationships: { store: { id: `store_${i % 12}` } },
    };
}

function makePayload(n: number): string {
    const data: any[] = [];
    for (let i = 0; i < n; i++) data.push(makeOrder(i));
    return JSON.stringify({ data, meta: { page: 1, per_page: n, total: n } });
}

function summarize(order: any): any {
    const a = order.attributes;
    return {
        id: order.id,
        number: a.number,
        email: a.customer.email,
        city: a.customer.address.city,
        items: a.line_items.length,
        total_cents: a.line_items.reduce((acc: number, li: any) => acc + li.qty * li.unit_cents, 0),
        gift: a.gift,
    };
}

function jsonApiRoundtrip(payload: string): string {
    const doc = JSON.parse(payload);
    const orders = doc.data
        .filter((o: any) => o.attributes.status !== "cancelled")
        .map((o: any) => summarize(o));
    const out = JSON.stringify({ orders, count: orders.length });
    return `${payload.length}/${out.length}/${orders.length}`;
}

// ============================================================================
// Decimal money arithmetic (bench-money.hot)
// ============================================================================

// JavaScript has no decimal type, so amounts are exact integers in units of
// 1e-8 (the usual fixed-point approach). Every intermediate value fits well
// inside Number's exact integer range.
const SCALE_TO_CENTS = 1000000; // 1e-8 units per cent

function roundToCents(units: number): number {
    // Half away from zero, as Hot's round does; amounts here are positive.
    return Math.floor((2 * units + SCALE_TO_CENTS) / (2 * SCALE_TO_CENTS));
}

function moneyDec(n: number): number {
    let total = 0;
    for (let i = 0; i < n; i++) {
        const unit = 499000000 + (i % 40) * 25000000; // 4.99 + (i % 40) * 0.25
        const qty = (i % 5) + 1;
        const line = unit * qty;
        const discounted = qty >= 4 ? (line * 9) / 10 : line;
        const tax = (discounted * 825) / 10000; // 8.25%
        total += roundToCents(discounted) + roundToCents(tax);
    }
    return total;
}

// ============================================================================
// Counting into a Map (bench-words.hot)
// ============================================================================

function makeText(n: number): string {
    const words: string[] = [];
    for (let i = 0; i < n; i++) {
        const k = (i * 7919) % 1000;
        words.push(`w${Math.floor((k * k) / 2000)}`);
    }
    return words.join(" ");
}

function wordCount(text: string): string {
    const counts = new Map<string, number>();
    for (const w of text.split(" ")) {
        counts.set(w, (counts.get(w) || 0) + 1);
    }
    const top = [...counts.entries()]
        .sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0))
        .slice(0, 10);
    return top.map(([w, c]) => `${w}:${c}`).join("|");
}

// ============================================================================
// Lambdas: captures, functions as values, nested HOFs (bench-lambdas.hot)
// ============================================================================

function range(n: number): number[] {
    return Array.from({ length: n }, (_, i) => i);
}

function closureCapture(n: number): number {
    const factor = (n % 5) + 3;
    const offset = 7;
    return range(n)
        .map((x) => x * factor + offset)
        .reduce((acc, x) => acc + x, 0);
}

function twice(f: (x: number) => number, x: number): number {
    return f(f(x));
}

function inc(x: number): number {
    return x + 1;
}

function fnAsValue(n: number): number {
    let total = 0;
    for (let i = 0; i < n; i++) total += twice(inc, i);
    return total;
}

function nestedHof(n: number): number {
    return range(n)
        .map((r) =>
            range(100)
                .filter((c) => (r + c) % 3 > 0)
                .reduce((a, c) => a + c, 0),
        )
        .reduce((acc, x) => acc + x, 0);
}

// ============================================================================
// Harness
// ============================================================================

// Every runner (bench.hot, bench.py, bench.js, bench.ts) times a benchmark the
// same way, with the same counts:
//   cold: the first call, alone.
//   warm: after `warmup` calls in all, the median of 5 samples, where a sample
//         is the mean time of `iterations` calls.
// Times are whole nanoseconds. run-all.py reads these lines.
const SAMPLE_COUNT = 5;

interface BenchmarkResult {
    name: string;
    cold: bigint;
    warm: bigint;
    result: unknown;
}

function measure(name: string, warmup: number, iterations: number, f: () => unknown): BenchmarkResult {
    const start = process.hrtime.bigint();
    const result = f();
    const cold = process.hrtime.bigint() - start;
    for (let i = 0; i < warmup - 1; i++) f();
    const samples: bigint[] = [];
    for (let s = 0; s < SAMPLE_COUNT; s++) {
        const sampleStart = process.hrtime.bigint();
        for (let i = 0; i < iterations; i++) f();
        samples.push((process.hrtime.bigint() - sampleStart) / BigInt(iterations));
    }
    samples.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    const warm = samples[Math.floor(SAMPLE_COUNT / 2)];
    console.log(`${name}: cold=${cold}ns warm=${warm}ns (result: ${result})`);
    return { name, cold, warm, result };
}

function runBenchmarks(): BenchmarkResult[] {
    // Inputs built once, outside the timings.
    const apiPayload = makePayload(1500);
    const text = makeText(50000);

    console.log("=== TypeScript Benchmarks ===");
    console.log();

    const results = [
        measure("fib-recursive(25)", 2, 3, () => fibRecursive(25)),
        measure("fib-iterative(70)", 100, 200, () => fibIterative(70)),
        measure("sum-even-squares(10000)", 20, 50, () => sumEvenSquares(10000)),
        measure("collection-benchmark(10000)", 20, 50, () => collectionBenchmark(10000)),
        measure("string-concat(1000)", 20, 50, () => stringConcatBenchmark(1000).length),
        measure("json-benchmark(1000)", 5, 10, () => jsonBenchmark(1000)),
        measure("count-primes(1000)", 1, 2, () => countPrimes(1000)),
        measure("records-reshape(2000)", 5, 10, () => recordsReshape(2000)),
        measure("json-api-roundtrip(1500)", 3, 5, () => jsonApiRoundtrip(apiPayload)),
        measure("money-dec(10000)", 3, 5, () => moneyDec(10000)),
        measure("word-count(50000)", 3, 5, () => wordCount(text)),
        measure("closure-capture(10000)", 10, 20, () => closureCapture(10000)),
        measure("fn-as-value(10000)", 10, 20, () => fnAsValue(10000)),
        measure("nested-hof(100)", 10, 20, () => nestedHof(100)),
    ];

    console.log();
    console.log("=== Benchmarks Complete ===");
    return results;
}

runBenchmarks();

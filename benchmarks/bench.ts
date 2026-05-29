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

interface BenchmarkResult {
    name: string;
    iterations: number;
    elapsed: number;
    result: number;
}

function measure(name: string, iterations: number, f: () => number): BenchmarkResult {
    const start = performance.now();
    let result = 0;
    for (let i = 0; i < iterations; i++) {
        result = f();
    }
    const end = performance.now();
    const elapsed = (end - start) / iterations;
    console.log(`${name}: ${elapsed.toFixed(6)}ms (result: ${result})`);
    return {
        name,
        iterations,
        elapsed,
        result,
    };
}

function runBenchmarks(): BenchmarkResult[] {
    console.log("=== TypeScript Benchmarks ===");
    console.log();

    const results: BenchmarkResult[] = [
        measure("fib-recursive(25)", 1, () => fibRecursive(25)),
        measure("fib-iterative(70)", 100, () => fibIterative(70)),
        measure("sum-even-squares(10000)", 50, () => sumEvenSquares(10000)),
        measure("collection-benchmark(10000)", 50, () => collectionBenchmark(10000)),
        measure("string-concat(1000)", 20, () => stringConcatBenchmark(1000).length),
        measure("json-benchmark(1000)", 10, () => jsonBenchmark(1000)),
        measure("count-primes(1000)", 3, () => countPrimes(1000)),
    ];

    console.log();
    console.log("=== Benchmarks Complete ===");
    return results;
}

runBenchmarks();

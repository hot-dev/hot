# redis

Pure-Hot Redis client. Speaks the RESP protocol directly over `::hot::tcp` (with optional `::hot::tls`) — no native driver required. Authentication, database selection, RESP2/RESP3, typed helpers for the common commands, and `command` for everything else. Works with Redis and Valkey.

## Usage

```hot
conn ::redis/connect({host: "localhost", port: 6379})

::redis/set(conn, "greeting", "hello")
::redis/get(conn, "greeting")        // "hello"
::redis/incr(conn, "counter")        // 1

// Arbitrary commands with typed replies
::redis/command(conn, ["LPUSH", "queue", "a", "b"])
::redis/command(conn, ["LRANGE", "queue", "0", "-1"])  // ["b", "a"]

::redis/close(conn)
```

## Error contract

Expected failures — connection refused, TLS negotiation, AUTH/SELECT, Redis error replies — return a `Result.Err`; branch with `is-err` / `if-err`. A socket failure mid-protocol (the connection drops between command and reply) halts the run instead: the connection state is unusable and the recovery unit is the run or task, not the command.

A connection is a runtime-owned socket handle; it lives for the duration of the run or task that opened it.

## License

Apache-2.0 - see [LICENSE](LICENSE)

# pg

Pure-Hot PostgreSQL client. Speaks the PostgreSQL v3 wire protocol directly over `::hot::tcp` (with optional `::hot::tls`) — no native driver required. Trust, cleartext, MD5, and SCRAM-SHA-256 authentication; simple and parameterized queries; text-format result decoding.

For portable placeholders, typed queries, and transactions, use [`hot.dev/sql-pg`](../sql-pg) — the `::sql` facade over this client. This package is the wire-level surface.

## Usage

```hot
conn ::pg/connect({host: "localhost", port: 5432, user: "postgres",
                   password: "secret", database: "app"})

users ::pg/query(conn, "SELECT id, email FROM users WHERE active = $1", [true])

::pg/execute(conn, "INSERT INTO events (name) VALUES ($1)", ["signup"])

::pg/close(conn)
```

## Error contract

Expected failures — connection refused, TLS negotiation, authentication, SQL errors — return a `Result.Err`; branch with `is-err` / `if-err`. A socket failure mid-protocol (the connection drops between request and reply) halts the run instead: the connection state is unusable and the recovery unit is the run or task, not the query.

A connection is a runtime-owned socket handle plus backend state; it lives for the duration of the run or task that opened it.

## License

Apache-2.0 - see [LICENSE](LICENSE)

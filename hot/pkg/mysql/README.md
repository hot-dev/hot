# mysql

Pure-Hot MySQL client. Speaks the MySQL client/server protocol directly over `::hot::tcp` (with optional `::hot::tls`) — no native driver required. Supports `caching_sha2_password` (the MySQL 8 default) and `mysql_native_password` authentication. Parameterized statements always use the binary prepared-statement protocol (`?` placeholders) — MySQL's text protocol has no safe parameter binding.

For portable placeholders, typed queries, and transactions, use [`hot.dev/sql-mysql`](../sql-mysql) — the `::sql` facade over this client. This package is the wire-level surface.

## Usage

```hot
conn ::mysql/connect({host: "localhost", user: "root",
                      password: "secret", database: "app"})

rows ::mysql/query(conn, "SELECT id, name FROM users WHERE active = ?", [true])

made ::mysql/execute(conn, "INSERT INTO events (name) VALUES (?)", ["signup"])
made.last-insert-id

::mysql/close(conn)
```

## Authentication over plaintext

Over plaintext TCP, `caching_sha2_password` can only complete when the server has the account's credentials cached (fast auth). When the server demands full authentication, `connect` errs with guidance: enable `ssl: true`, or use a `mysql_native_password` account.

## Concurrency

A connection is a single TCP socket with no request pipelining and is **not** safe for concurrent use. Hot runs independent expressions in parallel — do not share one connection across parallel branches or spawned tasks; open a connection per branch instead.

Expected failures (connect, auth, SQL errors with server code and message) return a `Result.Err`; a mid-protocol socket failure halts the run.

## License

Apache-2.0 - see [LICENSE](LICENSE)

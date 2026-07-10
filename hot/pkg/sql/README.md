# sql

Typed SQL runtime for Hot. Portable placeholders, typed query values, transactions, and one result/error shape across drivers:

| Driver | Open | Placeholder | Identifiers | Notes |
|---|---|---|---|---|
| PostgreSQL | `::sql::pg/open({...})` | `$1` | ANSI `"..."` | pure-Hot wire client ([`hot.dev/pg`](../pg)) |
| MySQL | `::sql::mysql/open({...})` | `?` | backtick `` `...` `` | pure-Hot wire client ([`hot.dev/mysql`](../mysql)); params always use binary prepared statements |
| SQLite | `::sql::sqlite/open("path.db")` | `?` | ANSI `"..."` | embedded (`::hot::sqlite` natives), `file.mode`-aware — managed-storage checkout/commit in service mode |

Inspired by Rust's sqlx: SQL stays SQL — this is not an ORM.

## Quick start

```hot
db ::sql::pg/open({user: "postgres", password: "secret", database: "app"})

rows ::sql/query(db, "SELECT id, email FROM users WHERE active = ?", [true])
user ::sql/query-one(db, "SELECT * FROM users WHERE email = :email", {email: "a@b.com"})
n ::sql/scalar(db, "SELECT count(*) FROM users")

::sql/execute(db, "DELETE FROM sessions WHERE expires_at < ?", [cutoff])

::sql/close(db)
```

## Placeholders

Write `?` (positional, bind a `Vec`) or `:name` (named, bind a `Map`) — never both in one statement. The runtime compiles them to the driver's native style (`$1, $2, ...` for PostgreSQL; `?` for MySQL and SQLite) while leaving string literals, quoted identifiers, comments, and `::type` casts alone. Repeated `:name` placeholders bind one value.

## Typed queries

Declare a query once with its row type and result shape, run it anywhere:

```hot
User type { id: Int, email: Str, name: Str? }

get-user ::sql/Query({
  sql: "SELECT id, email, name FROM users WHERE id = :id",
  returns: User,
  mode: "one"          // "many" (default) | "one" | "scalar" | "execute"
})

user ::sql/run(db, get-user, {id: 42})
user.email
```

## Transactions

```hot
::sql/transaction(db, (tx) {
  if-ok(::sql/execute(tx, "UPDATE accounts SET balance = balance - ? WHERE id = ?", [100, 1]), (debited) {
    ::sql/execute(tx, "UPDATE accounts SET balance = balance + ? WHERE id = ?", [100, 2])
  })
})
```

The body's Err rolls back and propagates; anything else commits and is returned.

## Errors

Every failure is an Err carrying `::sql/SqlError` — `{driver, code, message, detail, cause}` — with the PostgreSQL SQLSTATE in `code`. Branch with `is-err` / `if-err`.

## Writing a driver

Provide a `::sql/Driver`: a `name`, a `placeholder` style (`"dollar"` or `"question"`), and four fns — `connect(options)`, `query(conn, sql, params)`, `execute(conn, sql, params)`, `close(conn)`. Everything else (compilation, binding, typed queries, transactions, error normalization) is shared runtime.

## Documentation

Full documentation available at [hot.dev/pkg/sql](https://hot.dev/pkg/sql)

## License

Apache-2.0 - see [LICENSE](LICENSE)

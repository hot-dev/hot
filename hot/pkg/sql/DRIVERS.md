# Adding drivers to hot.dev/sql

The `::sql/Driver` contract is intentionally small — `name`, a
`placeholder` style, and four functions (`connect`, `query`, `execute`,
`close`). The runtime owns placeholder compilation, binding, typed
queries, transactions, and error normalization. This note captures the
design considerations for the next two drivers so they get a deliberate
pass rather than a rushed one.

## MySQL — pure-Hot wire client (the pg approach)

The right shape is a `hot.dev/mysql` package mirroring `hot.dev/pg`:
the MySQL client/server protocol over `::hot::tcp` / `::hot::tls`, then
a thin `::sql::mysql/driver` adapter (placeholder style `"question"`,
so `?` passes through untouched).

Work items:
- Handshake v10 + `caching_sha2_password` auth (needs SHA-256 which
  hot-std has; the full-auth path needs RSA public-key encryption of the
  password over non-TLS connections — prefer requiring TLS, like modern
  servers do, to avoid that path entirely).
- Text protocol first (`COM_QUERY`), matching pg's text-format decode
  approach; column type decoding from the result-set metadata.
- Prepared statements (`COM_STMT_PREPARE`/`EXECUTE`) can come later —
  the text protocol with client-side escaping is NOT acceptable for
  parameter binding, so parameters need the binary prepared-statement
  path from day one (unlike pg, MySQL's text protocol has no parameter
  placeholders).

Estimated scope: comparable to pg (~1.5k lines + wire tests). CI needs
a mysql service container next to the existing postgres one.

## SQLite — needs a runtime native (design decision required)

SQLite is a file format + C library, not a wire protocol, so a pure-Hot
client isn't realistic. Options:

1. **Natives backed by the `sqlx-sqlite` crate already in the
   workspace** (`libsqlite3-sys` is compiled into the hot binary today
   for the engine's own storage). The catch: sqlx is async and VM
   natives are synchronous — the natives would need a `block_on`
   bridge or a dedicated blocking connection pool, and connection
   handles crossing VM/task boundaries need ownership rules like
   `::hot::tcp` sockets have.
2. **`rusqlite` (synchronous)** as a new dependency — cleaner native
   shape (no async bridge), at the cost of a second sqlite binding in
   the binary.
3. **Engine-mediated**: expose the engine's existing sqlite pool with
   per-project database files (`hot://db/...`), making sqlite a managed
   platform feature rather than a raw driver.

Option 3 is the most "Hot-like" (files under `hot://`, no host paths,
works identically in cloud runs) and is probably the right product
answer; options 1/2 are faster to ship. Either way the Hot surface is
the same: `::sql::sqlite/open({path})` returning a `::sql/Driver`-shaped
connection with placeholder style `"question"`.

Decision needed before implementation: 1/2 vs 3, and whether sqlite
files live under `hot://` storage.

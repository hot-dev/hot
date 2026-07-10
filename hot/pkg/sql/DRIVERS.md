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

## SQLite — SHIPPED (::hot::sqlite natives + ::sql::sqlite driver)

Implemented 2026-07 on the decided design: natives in `crates/hot/src/
lang/hot/sqlite.rs` against the `libsqlite3-sys` already in the binary
(same node as sqlx-sqlite's transitive pin — no new dependency, no async
bridge), surfaced as `::hot::sqlite/{open,execute,query,sync,close}` in
hot-std, wrapped by `::sql::sqlite/driver` (placeholder `"question"`,
ANSI quote-ident).

`file.mode` aware:
- `direct` — host paths, opened in place.
- `service` — checkout/commit against FileStorage: open copies the file
  to a local scratch, queries run locally, close/sync commit the bytes
  back. Conflict handling is compare-before-write on the checkout hash
  (an Err, never a clobber; the check and write are not atomic — see
  module docs). Commit cost is O(file size): fits small session-shaped
  workloads; high-frequency writers belong on pg. Possible future work:
  content-addressed scratch cache, transfer compression, page-level
  delta sync (Litestream-style) if the write-amplification cases start
  to matter.

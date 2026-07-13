# Adding drivers to hot.dev/sql

Packaging shape (decided 2026-07): `hot.dev/sql` is the runtime and
Driver contract with ZERO deps. Each driver ships as an adapter package
— `sql-pg`, `sql-mysql`, `sql-sqlite` — that defines the `::sql::<db>`
namespace and depends on `sql` plus its wire client (sqlite needs only
the runtime natives). A new driver = a new `sql-<db>` package; never
add driver deps to `sql` itself.

The `::sql/Driver` contract is intentionally small — `name`, a
`placeholder` style, and four functions (`connect`, `query`, `execute`,
`close`). The runtime owns placeholder compilation, binding, typed
queries, transactions, and error normalization. This note captures the
design considerations for the next two drivers so they get a deliberate
pass rather than a rushed one.

## MySQL — SHIPPED (hot.dev/mysql + ::sql::mysql driver)

Implemented 2026-07 on the decided design: a pure-Hot wire client
(`hot.dev/mysql`, mirroring `hot.dev/pg`) over `::hot::tcp`/`::hot::tls`
with `caching_sha2_password` (fast path over any transport; full auth
requires TLS — plaintext full-auth errs with guidance rather than
implementing the RSA path) and `mysql_native_password`. Text protocol
(`COM_QUERY`) for parameterless statements; the binary prepared-statement
protocol (`COM_STMT_PREPARE`/`EXECUTE`, params sent as VAR_STRING/BLOB,
binary row decoding incl. IEEE-754 via `::hot::bytes/to-float`) for all
parameter binding. `::sql::mysql/driver` adapter: placeholder
`"question"`, backtick `quote-ident`.

Verified against a live MySQL 8 container (TLS + full auth + both
protocols): `docker run --rm -d --name hot-mysql -p 3306:3306 -e
MYSQL_ROOT_PASSWORD=hot_test -e MYSQL_DATABASE=hot_test mysql:8`, then
`hot test --project pkg-integration-mysql`. CI still needs the container
wired next to the postgres one. Not yet done: statement caching (each
parameterized call prepares/closes), unsigned BIGINT above i64, >=16MB
packets, multi-result-sets.

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
  back. Conflict handling is an atomic
  compare-and-swap on the file record's etag (FileStorage::write_file_if:
  the record flips before the bytes are written, so exactly one
  concurrent committer proceeds — losers get an Err, never a clobber). Commit cost is O(file size): fits small session-shaped
  workloads; high-frequency writers belong on pg. Possible future work:
  content-addressed scratch cache, transfer compression, page-level
  delta sync (Litestream-style) if the write-amplification cases start
  to matter.

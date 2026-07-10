# sql-sqlite

The SQLite driver for [`hot.dev/sql`](../sql): installs the `::sql::sqlite`
adapter over the runtime's embedded `::hot::sqlite` natives — no server,
no wire protocol, zero external dependencies. `file.mode`-aware: host
paths in `direct` mode, managed-storage checkout/commit in `service`
mode with atomic conflict detection.

```hot
db ::sql::sqlite/open("data/app.db")
::sql/execute(db, "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, name TEXT)", [])
rows ::sql/query(db, "SELECT * FROM t WHERE name = :name", {name: "Ada"})
::sql/close(db)
```

# sql-pg

The PostgreSQL driver for [`hot.dev/sql`](../sql): installs the `::sql::pg`
adapter and pulls in [`hot.dev/pg`](../pg) (pure-Hot wire client) and the
sql runtime.

```hot
db ::sql::pg/open({user: "postgres", password: "secret", database: "app"})
rows ::sql/query(db, "SELECT * FROM users WHERE active = ?", [true])
```

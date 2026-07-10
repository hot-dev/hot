# sql-mysql

The MySQL driver for [`hot.dev/sql`](../sql): installs the `::sql::mysql`
adapter and pulls in [`hot.dev/mysql`](../mysql) (pure-Hot wire client)
and the sql runtime. Identifiers quote with backticks; parameters always
use MySQL's binary prepared-statement protocol.

```hot
db ::sql::mysql/open({user: "root", password: "secret", database: "app"})
rows ::sql/query(db, "SELECT * FROM users WHERE active = ?", [true])
```

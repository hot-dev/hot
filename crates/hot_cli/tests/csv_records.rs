//! End to end: what `::hot::csv` leaves in a run's records, through `hot run`
//! with a real database, under both the JIT and the interpreter.
#![cfg(unix)]

use sqlx::Row;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const SECRET: &str = "SENTINEL-CSV-SECRET-11111";
/// Needs quoting in a cell, which changes how it reads in CSV text.
const QUOTED_SECRET: &str = "SENTINEL\"CSV,SECRET-22222";
const COLUMN_PASSWORD: &str = "SENTINEL-CSV-COLUMN-33333";
const PLAIN_CELL: &str = "ROW-MARKER-44444";

const PROGRAM: &str = r#"::csvrecords ns

secrets fn (): Vec {
    a ::hot::ctx/mark-secret("SENTINEL-CSV-SECRET-11111")
    b ::hot::ctx/mark-secret("SENTINEL\"CSV,SECRET-22222")
    [a, b]
}

// Secrets in cells, in both directions and through a line of text.
masked fn (): Int {
    keys secrets()
    rows [
        {id: "ROW-MARKER-44444", token: keys[0], note: `key=${keys[0]};`},
        {id: "2", token: keys[1], note: "plain"},
    ]
    text to-csv(rows)
    parsed from-csv(text)
    tabbed from-tsv(to-tsv(rows))
    lazily collect(iter-csv(text))
    named from-csv("id,password\n1,SENTINEL-CSV-COLUMN-33333\n", {types: {id: "Int"}})
    add(length(parsed), add(length(tabbed), add(length(lazily), length(named))))
}

eager fn (): Int {
    length(from-csv("id,note\n1,ROW-MARKER-44444\n"))
}

streamed fn (): Int {
    ::hot::file/write-file("rows.csv", "id,note\n1,ROW-MARKER-44444\n2,ROW-MARKER-44444\n")
    rows iter-csv(::hot::file/read-file-stream("rows.csv"))
    reduce(rows, (count, row) { add(count, length(row.id)) }, 0)
}
"#;

const ENGINES: &[(&str, &[(&str, &str)])] = &[
    ("interpreter", &[("HOT_JIT", "off")]),
    ("jit", &[("HOT_JIT_THRESHOLD", "1")]),
];

fn run_hot(dir: &Path, entry: &str, envs: &[(&str, &str)], record: bool) -> Output {
    std::fs::write(dir.join("run.hot"), format!("::run ns\n{entry}\n")).unwrap();
    let home = dir.join(".test-hot-home");
    if !home.join("pkg").exists() {
        std::fs::create_dir_all(&home).unwrap();
        let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
        std::os::unix::fs::symlink(pkg.canonicalize().unwrap(), home.join("pkg")).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_hot"));
    command
        .args(["run", "run.hot"])
        .current_dir(dir)
        .env("HOT_HOME", &home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    if record {
        command.args(["--emitter.type", "db"]);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("run hot");
    assert!(
        output.status.success(),
        "{entry}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_project(dir: &Path) -> String {
    let uri = format!("sqlite:{}", dir.join("records.db").display());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("hot.hot"),
        format!(
            "::hot::conf ns\nhot.set.project \"records\"\nhot.project.records.src.paths [\"./src\"]\nhot.db.uri \"{uri}\"\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/csvrecords.hot"), PROGRAM).unwrap();
    uri
}

/// Every text value in the database, as `table.column: value`.
async fn all_values(uri: &str) -> Vec<String> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(uri)
        .await
        .expect("open the program's database");
    let tables: Vec<String> =
        sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get("name"))
            .collect();
    let mut values = Vec::new();
    for table in &tables {
        let columns: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA table_info(\"{table}\")"
        )))
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get("name"))
        .collect();
        for column in columns {
            let query = format!(
                "SELECT CAST(\"{column}\" AS TEXT) AS value FROM \"{table}\" WHERE \"{column}\" IS NOT NULL"
            );
            for row in sqlx::query(sqlx::AssertSqlSafe(query))
                .fetch_all(&pool)
                .await
                .unwrap()
            {
                if let Ok(value) = row.try_get::<String, _>("value") {
                    values.push(format!("{table}.{column}: {value}"));
                }
            }
        }
    }
    pool.close().await;
    values
}

async fn run_recorded(entry: &'static str, envs: &'static [(&str, &str)]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let uri = write_project(dir.path());
    let path = dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || run_hot(&path, entry, envs, true))
        .await
        .unwrap();
    all_values(&uri).await
}

#[tokio::test(flavor = "multi_thread")]
async fn cells_holding_secrets_are_masked_in_records() {
    for (engine, envs) in ENGINES {
        let values = run_recorded("::csvrecords/masked()", envs).await;
        let escaped_in_json = QUOTED_SECRET.replace('"', "\\\"");
        let doubled = QUOTED_SECRET.replace('"', "\"\"");
        let doubled_in_json = doubled.replace('"', "\\\"");
        for value in &values {
            for needle in [
                SECRET,
                QUOTED_SECRET,
                escaped_in_json.as_str(),
                doubled.as_str(),
                doubled_in_json.as_str(),
            ] {
                assert!(
                    !value.contains(needle),
                    "{engine}: {needle} was recorded in {value}"
                );
            }
        }
        let call_rows: Vec<&String> = values.iter().filter(|v| v.starts_with("call.")).collect();
        assert!(
            call_rows.iter().any(|v| v.contains(PLAIN_CELL)),
            "{engine}: the records should still hold what isn't secret"
        );
        // A `password` column is a credential field once it is a Map key.
        assert!(
            call_rows.iter().any(|v| v.starts_with("call.return_value")
                && v.contains("\"password\"")
                && !v.contains(COLUMN_PASSWORD)),
            "{engine}: a parsed credential column should be masked by name"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn parsed_rows_are_recorded_and_streamed_rows_are_not() {
    for (engine, envs) in ENGINES {
        let eager = run_recorded("::csvrecords/eager()", envs).await;
        assert!(
            eager
                .iter()
                .any(|v| v.starts_with("call.return_value") && v.contains(PLAIN_CELL)),
            "{engine}: from-csv records the rows it returns"
        );

        let streamed = run_recorded("::csvrecords/streamed()", envs).await;
        let read_back: Vec<&String> = streamed
            .iter()
            .filter(|v| {
                (v.starts_with("call.args") || v.starts_with("call.return_value"))
                    && v.contains(PLAIN_CELL)
                    && !v.contains("rows.csv")
            })
            .collect();
        assert!(
            read_back.is_empty(),
            "{engine}: iter-csv should keep rows out of call records: {read_back:?}"
        );
    }
}

#[test]
fn streams_a_file_outside_a_project() {
    for (engine, envs) in ENGINES {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rows.csv"), "id\n1\n2\n3\n").unwrap();
        let output = run_hot(
            dir.path(),
            "reduce(iter-csv(::hot::file/read-file-stream(\"rows.csv\", {chunk-size: 2})), (n, row) { add(n, 1) }, 0)",
            envs,
            false,
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "3",
            "{engine}"
        );
    }
}

/// A server that sends a CSV body in chunks that split a row and a character.
fn serve_csv() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = "id,name\n1,日本語\n2,Ada\n".as_bytes();
            let (first, rest) = body.split_at(12);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n");
            let _ = stream.write_all(first);
            let _ = stream.flush();
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = stream.write_all(rest);
        }
    });
    format!("http://{address}/rows.csv")
}

#[test]
fn reads_rows_from_a_streaming_response() {
    let url = serve_csv();
    for (engine, envs) in ENGINES {
        let dir = tempfile::tempdir().unwrap();
        let entry = format!(
            "response ::hot::http/get-stream(\"{url}\")\nto-json(collect(iter-csv(response.body)))"
        );
        let output = run_hot(dir.path(), &entry, envs, false);
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            printed.contains("日本語") && printed.contains("Ada"),
            "{engine}: {printed}"
        );
    }
}

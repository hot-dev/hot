//! End to end: handlers sharing an event, all failing, under a real `hot dev`.
//! A failed run retries on its own handler's `retry` meta, and only that
//! handler runs on the retry.
//!
//! `HOT_TEST_POSTGRES_URI` adds a PostgreSQL pass, in its own database.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Each handler fails naming itself and the attempt that ran.
const HANDLERS: &str = r#"::retries ns

twice meta {on-event: "retries:shared", retry: {attempts: 2, delay: 1}}
fn (event) { fail(`twice ${info().run.retry-attempt}`) }

never meta {on-event: "retries:shared"}
fn (event) {
    send("retries:relayed", {})
    fail(`never ${info().run.retry-attempt}`)
}

once meta {on-event: "retries:shared", retry: {attempts: 1, delay: 1}}
fn (event) { fail(`once ${info().run.retry-attempt}`) }

// What a run sends is for this event's handlers, not the sender's.
relayed meta {on-event: "retries:relayed"}
fn (event) { fail(`relayed ${info().run.retry-attempt}`) }
"#;

const EVENTS: usize = 2;

/// Where `hot dev` records its runs.
enum Database {
    Sqlite,
    Postgres(String),
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A `HOT_HOME` for one test directory, holding only the checkout's packages.
fn hot_home(dir: &Path) -> PathBuf {
    let home = dir.join(".test-hot-home");
    std::fs::create_dir_all(&home).unwrap();
    let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
    std::os::unix::fs::symlink(pkg.canonicalize().unwrap(), home.join("pkg")).unwrap();
    home
}

fn hot(dir: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hot"));
    command
        .current_dir(dir)
        .env("HOT_HOME", home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    command
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Stops `hot dev` however the test ends.
struct Dev(Child);

impl Drop for Dev {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// An event run: its id, environment and org, its failure, and its status.
type EventRun = (Uuid, Uuid, Uuid, Option<String>, i64);

/// The event runs recorded so far.
async fn event_runs(database: &Database, home: &Path) -> Vec<EventRun> {
    match database {
        Database::Sqlite => {
            let uri = format!("sqlite:{}?mode=ro", home.join("db/hot.sqlite.db").display());
            let pool = sqlx::SqlitePool::connect(&uri).await.unwrap();
            let runs = sqlx::query_as(
                r#"SELECT run_id, r.env_id, org_id, json_extract(result, '$."$val".msg'), status_id
                   FROM run r JOIN env e ON e.env_id = r.env_id WHERE run_type_id = 2"#,
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            pool.close().await;
            runs
        }
        Database::Postgres(uri) => {
            let pool = sqlx::PgPool::connect(uri).await.unwrap();
            let runs = sqlx::query_as(
                "SELECT run_id, r.env_id, org_id, result->'$val'->>'msg', status_id::bigint
                 FROM hot.run r JOIN hot.env e ON e.env_id = r.env_id WHERE run_type_id = 2",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            pool.close().await;
            runs
        }
    }
}

/// The attempts that ran for each handler, and how many runs have not settled.
async fn attempts(database: &Database, home: &Path) -> (BTreeMap<String, Vec<i64>>, usize) {
    let mut attempts: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut unsettled = 0;
    for (_, _, _, failure, status_id) in event_runs(database, home).await {
        if status_id != 3 {
            unsettled += 1;
        }
        if let Some((handler, attempt)) = failure.as_deref().and_then(|msg| msg.split_once(' ')) {
            attempts
                .entry(handler.to_string())
                .or_default()
                .push(attempt.parse().unwrap());
        }
    }
    attempts.values_mut().for_each(|attempts| attempts.sort());
    (attempts, unsettled)
}

/// Wait for the expected runs, then for any that shouldn't exist.
async fn settled_attempts(database: &Database, home: &Path, expected: &BTreeMap<String, Vec<i64>>) {
    let total: usize = expected.values().map(Vec::len).sum();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (attempts, unsettled) = attempts(database, home).await;
        let ran: usize = attempts.values().map(Vec::len).sum();
        if (ran >= total && unsettled == 0) || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Long enough for the scheduler to send a retry that shouldn't exist.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (attempts, unsettled) = attempts(database, home).await;
    assert_eq!(attempts, *expected);
    assert_eq!(unsettled, 0, "every run ends failed, with no retry pending");
}

async fn failed_runs_retry_on_their_own_handler_alone(database: Database) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    let db_conf = match &database {
        Database::Sqlite => String::new(),
        Database::Postgres(uri) => {
            format!("hot.db.uri \"{uri}\"\nhot.db.rls.enforce false\n")
        }
    };
    write(
        &project.join("hot.hot"),
        &format!(
            "::hot::conf ns\n\nhot.set.project \"retries\"\nhot.project.retries.src.paths [\"./hot/src\"]\n{db_conf}"
        ),
    );
    write(&project.join("hot/src/retries.hot"), HANDLERS);
    let home = hot_home(project);

    let log = project.join("dev.log");
    let log_file = std::fs::File::create(&log).unwrap();
    let (api_port, app_port) = (free_port(), free_port());
    let _dev = Dev(hot(project, &home)
        .args(["dev", "--scheduler.retry-interval-seconds", "1"])
        .args(["--api.port", &api_port.to_string()])
        .args(["--app.port", &app_port.to_string()])
        .stdout(log_file.try_clone().unwrap())
        .stderr(log_file)
        .spawn()
        .expect("start hot dev"));

    // The API opens once the live build's handlers are deployed.
    let deadline = Instant::now() + Duration::from_secs(120);
    while TcpStream::connect(("127.0.0.1", api_port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "hot dev did not start: {}",
            std::fs::read_to_string(&log).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let send = hot(project, &home)
        .args([
            "eval",
            &format!(r#"map(range({EVENTS}), (n) {{ send("retries:shared", {{n: n}}) }})"#),
        ])
        .output()
        .expect("send the events");
    assert!(send.status.success(), "{send:?}");

    // Each event's own runs, per handler: the first try and its retries.
    let mut expected: BTreeMap<String, Vec<i64>> = [
        ("never", vec![0, 0]),
        ("once", vec![0, 0, 1, 1]),
        ("relayed", vec![0, 0]),
        ("twice", vec![0, 0, 1, 1, 2, 2]),
    ]
    .map(|(handler, attempts)| (handler.to_string(), attempts))
    .into();
    settled_attempts(&database, &home, &expected).await;

    // Retry in the app is for the run's handler alone too.
    let runs = event_runs(&database, &home).await;
    let (run_id, env_id, org_id, ..) = runs
        .iter()
        .find(|run| run.3.as_deref() == Some("once 1"))
        .expect("a run of `once` that used its retry");
    let retry = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{app_port}/runs/{run_id}/retry?org_id={org_id}&env_id={env_id}"
        ))
        .send()
        .await
        .unwrap();
    assert!(retry.status().is_success(), "{retry:?}");
    expected.get_mut("once").unwrap().push(2);
    settled_attempts(&database, &home, &expected).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_runs_retry_on_their_own_handler_alone_on_sqlite() {
    failed_runs_retry_on_their_own_handler_alone(Database::Sqlite).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_runs_retry_on_their_own_handler_alone_on_postgres() {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL retry test: HOT_TEST_POSTGRES_URI not set");
        return;
    };
    let admin = sqlx::PgPool::connect(&admin_uri)
        .await
        .expect("connect to the fixture database");
    let name = format!("hot_event_retries_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create the test database");
    let (server, _) = admin_uri.rsplit_once('/').expect("a database in the URI");

    failed_runs_retry_on_their_own_handler_alone(Database::Postgres(format!("{server}/{name}")))
        .await;

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("drop the test database");
}

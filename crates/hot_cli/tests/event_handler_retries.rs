//! End to end: handlers sharing an event, all failing, under a real `hot dev`.
//! A failed run retries on its own handler's `retry` meta, and only that
//! handler runs on the retry.
//!
//! `HOT_TEST_POSTGRES_URI` adds a PostgreSQL pass, in its own database.
#![cfg(unix)]

use serde_json::Value;
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

#[derive(sqlx::FromRow)]
struct EventRun {
    run_id: Uuid,
    origin_run_id: Option<Uuid>,
    stream_id: Uuid,
    env_id: Uuid,
    org_id: Uuid,
    failure: Option<String>,
    status_id: i64,
    info: Option<String>,
}

/// The event runs recorded so far.
async fn event_runs(database: &Database, home: &Path) -> Vec<EventRun> {
    match database {
        Database::Sqlite => {
            let uri = format!("sqlite:{}?mode=ro", home.join("db/hot.sqlite.db").display());
            let pool = sqlx::SqlitePool::connect(&uri).await.unwrap();
            let runs = sqlx::query_as(
                r#"SELECT run_id, origin_run_id, stream_id, r.env_id, org_id,
                          json_extract(result, '$."$val".msg') AS failure, status_id, info
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
                "SELECT run_id, origin_run_id, stream_id, r.env_id, org_id,
                        result->'$val'->>'msg' AS failure, status_id::bigint AS status_id,
                        info::text AS info
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
    for run in event_runs(database, home).await {
        if run.status_id != 3 {
            unsettled += 1;
        }
        let failure = run.failure.as_deref();
        if let Some((handler, attempt)) = failure.and_then(|msg| msg.split_once(' ')) {
            attempts
                .entry(handler.to_string())
                .or_default()
                .push(attempt.parse().unwrap());
        }
    }
    attempts.values_mut().for_each(|attempts| attempts.sort());
    (attempts, unsettled)
}

/// Each retried attempt's `info.retry_run_id`, by attempt.
fn recorded_retries(runs: &[EventRun]) -> BTreeMap<Uuid, Uuid> {
    runs.iter()
        .filter_map(|run| {
            let info: Value = serde_json::from_str(run.info.as_deref()?).ok()?;
            let retry = info.get("retry_run_id")?.as_str()?.parse().ok()?;
            Some((run.run_id, retry))
        })
        .collect()
}

/// The retries a stream's subscribers are told of, by attempt, once `count`
/// have arrived.
async fn streamed_retries(
    api_port: u16,
    key: &str,
    stream_id: Uuid,
    count: usize,
) -> BTreeMap<Uuid, Uuid> {
    let mut stream = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{api_port}/v1/streams/{stream_id}/subscribe"
        ))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert!(stream.status().is_success(), "{stream:?}");
    let mut body = String::new();
    let mut retries = BTreeMap::new();
    while retries.len() < count {
        let chunk = tokio::time::timeout(Duration::from_secs(30), stream.chunk())
            .await
            .unwrap_or_else(|_| panic!("{} of {count} retries in {body}", retries.len()))
            .unwrap()
            .expect("the stream stays open");
        body.push_str(&String::from_utf8_lossy(&chunk));
        for event in body.lines().filter_map(|line| line.strip_prefix("data:")) {
            let Ok(event) = serde_json::from_str::<Value>(event) else {
                continue;
            };
            let run_id = event["run"]["run_id"].as_str();
            if let (Some(run_id), Some(retry)) = (run_id, event["retry_run_id"].as_str()) {
                retries.insert(run_id.parse().unwrap(), retry.parse().unwrap());
            }
        }
    }
    retries
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

    // Each attempt that retried names its retry, which names it back.
    let runs = event_runs(&database, &home).await;
    let recorded = recorded_retries(&runs);
    let retried = expected.values().flatten().filter(|n| **n > 0).count();
    assert_eq!(recorded.len(), retried, "one link per retry");
    for (attempt, retry) in &recorded {
        let retry = runs
            .iter()
            .find(|run| run.run_id == *retry)
            .unwrap_or_else(|| panic!("run {attempt} names a retry that never ran: {retry}"));
        assert_eq!(retry.origin_run_id, Some(*attempt));
    }

    // Subscribers are told the same.
    let key = hot(project, &home)
        .args(["key", "create"])
        .output()
        .expect("create an API key");
    assert!(key.status.success(), "{key:?}");
    let key = String::from_utf8(key.stdout).unwrap();
    let key = key.trim().lines().last().unwrap().trim();
    let stream_id = runs[0].stream_id;
    assert!(runs.iter().all(|run| run.stream_id == stream_id));
    assert_eq!(
        streamed_retries(api_port, key, stream_id, retried).await,
        recorded
    );

    // Retry in the app is for the run's handler alone too.
    let run = runs
        .iter()
        .find(|run| run.failure.as_deref() == Some("once 1"))
        .expect("a run of `once` that used its retry");
    let (run_id, env_id, org_id) = (run.run_id, run.env_id, run.org_id);
    let retry = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{app_port}/runs/{run_id}/retry?org_id={org_id}&env_id={env_id}"
        ))
        .send()
        .await
        .unwrap();
    assert!(retry.status().is_success(), "{retry:?}");
    let retry: Value = retry.json().await.unwrap();
    expected.get_mut("once").unwrap().push(2);
    settled_attempts(&database, &home, &expected).await;

    // The run the app reports is the run that ran.
    let retry: Uuid = retry["run_id"].as_str().unwrap().parse().unwrap();
    let runs = event_runs(&database, &home).await;
    let retry = runs
        .iter()
        .find(|run| run.run_id == retry)
        .unwrap_or_else(|| panic!("the app reported a run that never ran: {retry}"));
    assert_eq!(retry.origin_run_id, Some(run_id));

    // Re-run is for all the event's handlers, so no one run is the re-run, and
    // the page the app opens lists them all.
    let scope = format!("org_id={org_id}&env_id={env_id}");
    let app = format!("http://127.0.0.1:{app_port}");
    let client = reqwest::Client::new();
    let rerun = client
        .post(format!("{app}/runs/{run_id}/rerun?{scope}"))
        .send()
        .await
        .unwrap();
    assert!(rerun.status().is_success(), "{rerun:?}");
    let rerun: Value = rerun.json().await.unwrap();
    assert_eq!(rerun["run_id"], Value::Null, "{rerun}");
    for (handler, attempts) in [
        ("never", vec![0]),
        ("once", vec![0, 1]),
        ("relayed", vec![0]),
        ("twice", vec![0, 1, 2]),
    ] {
        let expected = expected.get_mut(handler).unwrap();
        expected.extend(attempts);
        expected.sort();
    }
    settled_attempts(&database, &home, &expected).await;

    let event_id = rerun["event_id"].as_str().unwrap();
    let table = client
        .get(format!("{app}/events/{event_id}/table?{scope}"))
        .send()
        .await
        .unwrap();
    assert!(table.status().is_success(), "{table:?}");
    let table = table.text().await.unwrap();
    assert_eq!(table.matches("href=\"/runs/").count(), 3, "{table}");
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

/// How many times the handler of `resends:event` has executed. Each execution
/// sends an event: a second one of the same run keeps the run's row and id.
async fn resent_event_executions(home: &Path) -> i64 {
    let uri = format!("sqlite:{}?mode=ro", home.join("db/hot.sqlite.db").display());
    let pool = sqlx::SqlitePool::connect(&uri).await.unwrap();
    let executions =
        sqlx::query_scalar("SELECT COUNT(*) FROM event WHERE event_type = 'resends:ran'")
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    executions
}

/// A publish sent again with its event's id runs the event once, even when the
/// first publish died before saying it had enqueued the event: the second
/// message is then a redelivery only the worker can recognize.
#[tokio::test(flavor = "multi_thread")]
async fn a_resent_publish_runs_its_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write(
        &project.join("hot.hot"),
        "::hot::conf ns\n\nhot.set.project \"resends\"\nhot.project.resends.src.paths [\"./hot/src\"]\n",
    );
    write(
        &project.join("hot/src/resends.hot"),
        "::resends ns\n\nhandle meta {on-event: \"resends:event\"}\nfn (event) { send(\"resends:ran\", {}) }\n",
    );
    let home = hot_home(project);

    let log = project.join("dev.log");
    let log_file = std::fs::File::create(&log).unwrap();
    let (api_port, app_port) = (free_port(), free_port());
    let _dev = Dev(hot(project, &home)
        .arg("dev")
        // The worker's line for a redelivery with nothing left to run.
        .env("HOT_LOG_LEVEL", "debug")
        .args(["--api.port", &api_port.to_string()])
        .args(["--app.port", &app_port.to_string()])
        .stdout(log_file.try_clone().unwrap())
        .stderr(log_file)
        .spawn()
        .expect("start hot dev"));
    let deadline = Instant::now() + Duration::from_secs(120);
    while TcpStream::connect(("127.0.0.1", api_port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "hot dev did not start: {}",
            std::fs::read_to_string(&log).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let key = hot(project, &home)
        .args(["key", "create"])
        .output()
        .expect("create an API key");
    assert!(key.status.success(), "{key:?}");
    let key = String::from_utf8(key.stdout).unwrap();
    let key = key.trim().lines().last().unwrap().trim().to_string();

    let event_id = Uuid::new_v4();
    let publish = async || {
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{api_port}/v1/events"))
            .bearer_auth(&key)
            .json(&serde_json::json!({
                "event_id": event_id,
                "event_type": "resends:event",
                "event_data": {}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        response.json::<Value>().await.unwrap()["data"].clone()
    };
    let wait_until = async |what: &str, done: &dyn Fn(String, i64) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log_text = std::fs::read_to_string(&log).unwrap();
            if done(log_text, resent_event_executions(&home).await) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                std::fs::read_to_string(&log).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    let first = publish().await;
    assert_eq!(first["event_id"], event_id.to_string());
    wait_until("the handler to execute", &|_, executions| executions == 1).await;

    assert_eq!(publish().await, first);

    // As a publish leaves its event when it dies between the enqueue and
    // settling the lease, before the worker's run marks the event handled.
    let uri = format!("sqlite:{}", home.join("db/hot.sqlite.db").display());
    let pool = sqlx::SqlitePool::connect(&uri).await.unwrap();
    sqlx::query(
        "UPDATE event SET handled = 0, enqueue_leased_until = '2000-01-01 00:00:00+00:00' \
         WHERE event_id = ?",
    )
    .bind(event_id)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    assert_eq!(publish().await, first);
    let acknowledged = format!("acknowledges redelivered event {event_id}");
    wait_until("the second message to be acknowledged", &|log_text, _| {
        log_text.contains(&acknowledged)
    })
    .await;
    assert_eq!(resent_event_executions(&home).await, 1);
}

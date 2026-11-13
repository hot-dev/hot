//! End-to-end queue-wait benchmark: publish to first instruction, through a
//! real `hot dev` (API, event worker, task worker and scheduler in one
//! process).
//!
//! It measures, per stage, how long an event takes from its publish to the
//! first instruction of its handler, how long a code task takes from
//! `::hot::task/start` to its first instruction, and how late `::hot::task/await`
//! returns after its task finished. Each is measured with the workers idle and
//! with handlers (or tasks) in flight. On PostgreSQL it also counts the
//! statements and client/server exchanges each event and task costs.
//!
//! Every test here is `#[ignore]`: nothing runs in a default `cargo test`.
//! `scripts/queue-wait-benchmark.sh` builds a release binary, starts throwaway
//! PostgreSQL and Valkey containers and runs all three; see its header and
//! `docs/queue-wait-benchmark.md`. By hand:
//!
//! ```text
//! cargo test -p hot_cli --test queue_wait_e2e -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The SQLite tests need nothing else. The PostgreSQL + Redis test runs when
//! `HOT_TEST_POSTGRES_URI` (a login that may create databases and roles) and
//! `HOT_REDIS_URI` (an empty database of a throwaway server) are both set.
//!
//! Knobs, all optional:
//!
//! - `HOT_BENCH_BIN`: the `hot` binary to measure (default: this build's).
//! - `HOT_BENCH_EVENTS` (200), `HOT_BENCH_TASKS` (100), `HOT_BENCH_BURST` (100),
//!   `HOT_BENCH_LONG_TASKS` (10), `HOT_BENCH_LONG_TASK_MS` (2000): sample counts.
//! - `HOT_BENCH_IN_FLIGHT` (2): handlers or tasks held in flight.
//! - `HOT_BENCH_CROSS_PROCESS` (30): events sent from `hot eval` processes
//!   (SQLite queue only). `HOT_BENCH_COLD` (3) events each sent after
//!   `HOT_BENCH_COLD_IDLE_SECONDS` (75) of idleness (PostgreSQL only).
//! - `HOT_BENCH_WORKER_THREADS` (4), `HOT_BENCH_TASK_CONCURRENCY` (4).
//! - `HOT_BENCH_DEPLOY`: `bundle` (default: `hot deploy --local` first, as a
//!   deployed project runs) or `live` (what `hot dev` serves while a project
//!   is edited). The memory queue always measures the live build, because a
//!   deploy from another process cannot reach it.
//! - `HOT_BENCH_OUT`: directory for the report (default
//!   `target/queue-wait-runs/<time>`).
//! - `HOT_BENCH_PSQL`: a `psql` command line that reaches the server (for
//!   example `docker exec -i my-postgres psql`, or `psql -h host -p 5432`). With it the PostgreSQL run uses a
//!   restricted runtime login with row-level security enforced, as production
//!   does; without it `hot dev` connects as the owner with RLS not enforced.
#![cfg(unix)]

use futures::FutureExt;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::types::chrono::{DateTime, NaiveDateTime, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// The project under measurement. Every handler's first instruction reads the
/// clock, so a run's result says when its code began.
const SOURCE: &str = r#"::qwbench ns

::time ::hot::time
::event ::hot::event
::task ::hot::task
::http ::hot::http

// An event published straight to this handler.
probe meta {on-event: "qwbench:probe"}
fn (event) {
    t0 ::time/epoch-nanos(::time/now())
    {t0: t0, seq: event.data.seq}
}

// An event sent by another process (`hot eval`).
xproc meta {on-event: "qwbench:xproc"}
fn (event) {
    t0 ::time/epoch-nanos(::time/now())
    {t0: t0, seq: event.data.seq}
}

// Holds an event executor until the harness's server answers.
hold meta {on-event: "qwbench:hold"}
fn (event) {
    ::http/get(event.data.url).status
}

// A handler that sends an event: the producer path inside a run.
relay meta {on-event: "qwbench:relay"}
fn (event) {
    t0 ::time/epoch-nanos(::time/now())
    sent ::event/send("qwbench:hop", {seq: event.data.seq})
    t1 ::time/epoch-nanos(::time/now())
    {t0: t0, send-start: t0, send-end: t1, sent: is-some(sent)}
}

hop meta {on-event: "qwbench:hop"}
fn (event) {
    t0 ::time/epoch-nanos(::time/now())
    {t0: t0, seq: event.data.seq}
}

// A code task. With a url it waits on the harness's server first.
work fn (args: Any): Map {
    t0 ::time/epoch-nanos(::time/now())
    waited cond {
        is-some(args.url) => { ::http/get(args.url).status }
        => { 0 }
    }
    t1 ::time/epoch-nanos(::time/now())
    {t0: t0, t1: t1, waited: waited}
}

// Starts a code task and awaits it.
spawn meta {on-event: "qwbench:task"}
fn (event) {
    t-call ::time/epoch-nanos(::time/now())
    info ::task/start(::qwbench/work, {seq: event.data.seq, url: event.data.url})
    t-started ::time/epoch-nanos(::time/now())
    result ::task/await(info.id)
    t-awaited ::time/epoch-nanos(::time/now())
    {
        task-id: info.id,
        start-call: t-call,
        start-returned: t-started,
        awaited: t-awaited,
        status: result.status,
    }
}

// Starts a task that holds a task slot, without awaiting it.
hold-task meta {on-event: "qwbench:hold-task"}
fn (event) {
    ::task/start(::qwbench/work, {url: event.data.url}).id
}
"#;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A `HOT_HOME` for the benchmark's database and caches. A binary built from
/// this checkout gets the checkout's packages; any other keeps the packages
/// and resources of its own installation.
fn hot_home(dir: &Path) -> PathBuf {
    let home = dir.join(".bench-hot-home");
    std::fs::create_dir_all(&home).unwrap();
    if hot_bin().starts_with(repo_root()) {
        std::os::unix::fs::symlink(repo_root().join("hot/pkg"), home.join("pkg")).unwrap();
    }
    home
}

fn hot_bin() -> PathBuf {
    std::env::var_os("HOT_BENCH_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hot")))
}

/// A `hot` command in the project, with none of the caller's `HOT_*` settings:
/// the project's `hot.hot` alone decides the backends.
fn hot(dir: &Path, home: &Path) -> Command {
    let mut command = Command::new(hot_bin());
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("HOT_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(dir)
        .env("HOT_HOME", home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    command
}

/// Stops `hot dev` however the benchmark ends.
struct Dev(Child);

impl Drop for Dev {
    fn drop(&mut self) {
        // SIGINT first, so the queue and database close cleanly.
        Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status()
            .ok();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

struct Postgres {
    /// The login that owns the benchmark database.
    admin_uri: String,
    /// The login `hot dev` runs as.
    runtime_uri: String,
    database: String,
    /// The restricted login, when one was provisioned.
    runtime_role: Option<String>,
    /// A second throwaway database, for `pg_stat_statements`.
    stats_uri: String,
    redis_uri: String,
}

enum Backend {
    Sqlite { queue: &'static str },
    Postgres(Postgres),
}

impl Backend {
    fn label(&self) -> String {
        match self {
            Backend::Sqlite { queue } => format!("sqlite-db-{queue}-queue"),
            Backend::Postgres(_) => "postgres-db-redis-queue".to_string(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Backend::Sqlite { queue } => format!("SQLite database, {queue} queue"),
            Backend::Postgres(pg) => format!(
                "PostgreSQL database, Redis queue, {}",
                if pg.runtime_role.is_some() {
                    "restricted runtime login with RLS enforced"
                } else {
                    "owner login with RLS not enforced"
                }
            ),
        }
    }

    fn conf(&self) -> String {
        match self {
            Backend::Sqlite { queue } => format!("hot.queue.type \"{queue}\"\n"),
            Backend::Postgres(pg) => format!(
                "hot.db.uri \"{}\"\nhot.db.rls.enforce {}\nhot.queue.type \"redis\"\nhot.redis.uri \"{}\"\n",
                pg.runtime_uri,
                pg.runtime_role.is_some(),
                pg.redis_uri
            ),
        }
    }
}

/// The measured database, read by the harness on its own connections.
enum Reader {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

/// Marks the harness's own statements, so the statement counts leave them out.
/// It is a common table expression nothing reads, because the statement text
/// `pg_stat_statements` keeps has its leading comment removed.
const MARK: &str = "WITH qwbench_harness AS (SELECT 1)";

impl Reader {
    /// Finished runs of an event type.
    async fn finished(&self, event_type: &str) -> i64 {
        match self {
            Reader::Sqlite(pool) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM run r JOIN event e ON e.event_id = r.event_id \
                 WHERE e.event_type = ? AND r.stop_time IS NOT NULL",
            )
            .bind(event_type)
            .fetch_one(pool)
            .await
            .unwrap_or(0),
            Reader::Postgres(pool) => sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "{MARK} SELECT COUNT(*) FROM hot.run r JOIN hot.event e ON e.event_id = r.event_id \
                 WHERE e.event_type = $1 AND r.stop_time IS NOT NULL"
            )))
            .bind(event_type)
            .fetch_one(pool)
            .await
            .unwrap_or(0),
        }
    }

    async fn runs(&self) -> Vec<RunRow> {
        type Row = (
            Uuid,
            Option<Uuid>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = match self {
            Reader::Sqlite(pool) => sqlx::query_as(
                "SELECT r.run_id, r.event_id, CAST(r.start_time AS TEXT), CAST(r.stop_time AS TEXT), \
                        r.info, r.result, e.event_type, CAST(e.event_time AS TEXT) \
                 FROM run r LEFT JOIN event e ON e.event_id = r.event_id",
            )
            .fetch_all(pool)
            .await
            .unwrap(),
            Reader::Postgres(pool) => sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{MARK} SELECT r.run_id, r.event_id, {start}, {stop}, r.info::text, r.result::text, \
                        e.event_type, {event_time} \
                 FROM hot.run r LEFT JOIN hot.event e ON e.event_id = r.event_id",
                start = pg_time("r.start_time"),
                stop = pg_time("r.stop_time"),
                event_time = pg_time("e.event_time"),
            )))
            .fetch_all(pool)
            .await
            .unwrap(),
        };
        rows.into_iter()
            .map(
                |(_run_id, event_id, start, _stop, info, result, event_type, event_time)| RunRow {
                    event_id,
                    start_us: start.as_deref().and_then(parse_time_us),
                    info: info.and_then(|text| serde_json::from_str(&text).ok()),
                    result: result.and_then(|text| serde_json::from_str(&text).ok()),
                    event_type,
                    event_time_us: event_time.as_deref().and_then(parse_time_us),
                },
            )
            .collect()
    }

    async fn tasks(&self) -> HashMap<Uuid, TaskRow> {
        type Row = (
            Uuid,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> =
            match self {
                Reader::Sqlite(pool) => sqlx::query_as(
                    "SELECT task_id, CAST(created_at AS TEXT), CAST(stop_time AS TEXT), timing, result FROM task",
                )
                .fetch_all(pool)
                .await
                .unwrap(),
                Reader::Postgres(pool) => sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "{MARK} SELECT task_id, {created}, {stop}, timing::text, result::text FROM hot.task",
                    created = pg_time("created_at"),
                    stop = pg_time("stop_time"),
                )))
                .fetch_all(pool)
                .await
                .unwrap(),
            };
        rows.into_iter()
            .map(|(task_id, created, stop, timing, result)| {
                (
                    task_id,
                    TaskRow {
                        created_us: created.as_deref().and_then(parse_time_us),
                        stop_us: stop.as_deref().and_then(parse_time_us),
                        timing: timing.and_then(|text| serde_json::from_str(&text).ok()),
                        result: result.and_then(|text| serde_json::from_str(&text).ok()),
                    },
                )
            })
            .collect()
    }
}

fn pg_time(column: &str) -> String {
    format!("to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
}

/// Microseconds since the epoch of a stored timestamp, in any of the forms
/// the two databases and the timing JSON use.
fn parse_time_us(text: &str) -> Option<i64> {
    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return Some(time.with_timezone(&Utc).timestamp_micros());
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%#z"] {
        if let Ok(time) = DateTime::parse_from_str(text, format) {
            return Some(time.with_timezone(&Utc).timestamp_micros());
        }
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(time) = NaiveDateTime::parse_from_str(text, format) {
            return Some(time.and_utc().timestamp_micros());
        }
    }
    None
}

struct RunRow {
    event_id: Option<Uuid>,
    start_us: Option<i64>,
    info: Option<Value>,
    result: Option<Value>,
    event_type: Option<String>,
    event_time_us: Option<i64>,
}

struct TaskRow {
    created_us: Option<i64>,
    stop_us: Option<i64>,
    timing: Option<Value>,
    result: Option<Value>,
}

/// The first value under `key` anywhere in a stored Hot value, whatever
/// wrapping the storage format puts around maps and numbers.
fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|inner| find(inner, key))),
        Value::Array(items) => items.iter().find_map(|inner| find(inner, key)),
        _ => None,
    }
}

fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.parse().ok(),
        Value::Object(map) => map.values().find_map(as_int),
        _ => None,
    }
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.values().find_map(as_text),
        _ => None,
    }
}

/// A clock reading a handler returned (nanoseconds), as microseconds.
fn clock_us(result: Option<&Value>, key: &str) -> Option<i64> {
    find(result?, key)
        .and_then(as_int)
        .map(|nanos| nanos / 1_000)
}

fn info_time_us(info: Option<&Value>, key: &str) -> Option<i64> {
    info?
        .get("queue_timing")?
        .get(key)?
        .as_str()
        .and_then(parse_time_us)
}

fn info_us(info: Option<&Value>, key: &str) -> Option<i64> {
    info?.get("queue_timing")?.get(key)?.as_i64()
}

// ---------------------------------------------------------------------------
// The harness's own server, which holds handlers and tasks in flight
// ---------------------------------------------------------------------------

/// Answers `GET /<ms>/<id>` after `<ms>` milliseconds and notes the id.
struct HoldServer {
    url: String,
    answered: Arc<Mutex<HashSet<String>>>,
}

impl HoldServer {
    async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let answered = Arc::new(Mutex::new(HashSet::new()));
        let counter = Arc::clone(&answered);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let path = request.split_whitespace().nth(1).unwrap_or("/");
                    let mut parts = path.trim_start_matches('/').split('/');
                    let delay_ms = parts.next().and_then(|ms| ms.parse().ok()).unwrap_or(0);
                    let id = parts.next().unwrap_or_default().to_string();
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    counter.lock().unwrap().insert(id);
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        Self { url, answered }
    }
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Api {
    client: reqwest::Client,
    url: String,
    key: String,
}

/// One publish: when it was sent and answered, and the event it made.
#[derive(Clone, Copy)]
struct Published {
    sent_us: i64,
    acked_us: i64,
    event_id: Uuid,
}

impl Api {
    async fn publish(&self, event_type: &str, data: Value) -> Published {
        let body = json!({"event_type": event_type, "event_data": data});
        let mut rate_limited = 0;
        loop {
            let sent_us = now_us();
            let response = self
                .client
                .post(&self.url)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await
                .expect("publish");
            let status = response.status();
            let answer: Value = response.json().await.unwrap_or(Value::Null);
            let acked_us = now_us();
            if status.as_u16() == 429 && rate_limited < 200 {
                rate_limited += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            assert!(
                status.is_success(),
                "publish {event_type}: {status} {answer}"
            );
            let event_id = find(&answer, "event_id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse().ok())
                .unwrap_or_else(|| panic!("publish answered without an event id: {answer}"));
            return Published {
                sent_us,
                acked_us,
                event_id,
            };
        }
    }
}

/// A small deterministic generator, for the gaps between publishes. A fixed
/// gap would hold one phase against the workers' poll intervals and hide them.
struct Jitter(u64);

impl Jitter {
    fn gap(&mut self) -> Duration {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        Duration::from_micros(20_000 + (self.0 >> 33) % 60_000)
    }
}

async fn wait_until(what: &str, timeout: Duration, mut done: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Keeps `count` handlers (or tasks) waiting on the hold server until dropped
/// and stopped: each asks for `hold_ms`, and is replaced when it is answered.
struct InFlight {
    stop: Arc<AtomicBool>,
    keepers: Vec<tokio::task::JoinHandle<()>>,
}

impl InFlight {
    fn start(api: &Api, server: &HoldServer, event_type: &'static str, count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let hold_ms = env_usize("HOT_BENCH_HOLD_MS", 3_000);
        let keepers = (0..count)
            .map(|keeper| {
                let (api, stop) = (api.clone(), Arc::clone(&stop));
                let (url, answered) = (server.url.clone(), Arc::clone(&server.answered));
                tokio::spawn(async move {
                    // Stagger the holds so they never all end together.
                    let mut ms = hold_ms * (keeper + 1) / count.max(1);
                    while !stop.load(Ordering::Relaxed) {
                        let id = Uuid::now_v7().simple().to_string();
                        api.publish(event_type, json!({"url": format!("{url}/{ms}/{id}")}))
                            .await;
                        ms = hold_ms;
                        // Replace this hold once the server has answered it.
                        let deadline = Instant::now() + Duration::from_millis(hold_ms as u64 * 3);
                        while !answered.lock().unwrap().contains(&id)
                            && !stop.load(Ordering::Relaxed)
                            && Instant::now() < deadline
                        {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }
                })
            })
            .collect();
        Self { stop, keepers }
    }

    async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for keeper in self.keepers {
            keeper.await.ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Statistics and the report
// ---------------------------------------------------------------------------

fn percentile(sorted: &[i64], percentile: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn ms(micros: i64) -> String {
    format!("{:.2}", micros as f64 / 1_000.0)
}

/// Named samples in microseconds, in the order their stages happen.
#[derive(Default)]
struct Stages {
    order: Vec<&'static str>,
    samples: BTreeMap<&'static str, Vec<i64>>,
}

impl Stages {
    fn add(&mut self, stage: &'static str, from: Option<i64>, to: Option<i64>) {
        if !self.order.contains(&stage) {
            self.order.push(stage);
        }
        if let (Some(from), Some(to)) = (from, to) {
            self.samples.entry(stage).or_default().push(to - from);
        }
    }

    fn add_value(&mut self, stage: &'static str, value: Option<i64>) {
        self.add(stage, value.map(|_| 0), value);
    }

    fn table(&self, title: &str, note: &str) -> (String, Value) {
        let mut text = format!("### {title}\n\n");
        if !note.is_empty() {
            let _ = writeln!(text, "{note}\n");
        }
        text.push_str("| stage | n | p50 ms | p95 ms | p99 ms | mean ms | max ms |\n");
        text.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
        let mut data = serde_json::Map::new();
        for stage in &self.order {
            let Some(samples) = self.samples.get(stage) else {
                let _ = writeln!(text, "| {stage} | 0 | | | | | |");
                continue;
            };
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            let mean = sorted.iter().sum::<i64>() / sorted.len() as i64;
            let (p50, p95, p99) = (
                percentile(&sorted, 0.50),
                percentile(&sorted, 0.95),
                percentile(&sorted, 0.99),
            );
            let max = *sorted.last().unwrap();
            let _ = writeln!(
                text,
                "| {stage} | {} | {} | {} | {} | {} | {} |",
                sorted.len(),
                ms(p50),
                ms(p95),
                ms(p99),
                ms(mean),
                ms(max)
            );
            data.insert(
                stage.to_string(),
                json!({"n": sorted.len(), "p50_us": p50, "p95_us": p95, "p99_us": p99,
                       "mean_us": mean, "max_us": max}),
            );
        }
        text.push('\n');
        (text, Value::Object(data))
    }
}

struct Report {
    text: String,
    data: serde_json::Map<String, Value>,
}

impl Report {
    fn section(&mut self, key: &str, title: &str, note: &str, stages: &Stages) {
        let (text, data) = stages.table(title, note);
        print!("{text}");
        self.text.push_str(&text);
        self.data.insert(key.to_string(), data);
    }

    fn note(&mut self, text: &str) {
        println!("{text}");
        self.text.push_str(text);
        self.text.push('\n');
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL statement counts
// ---------------------------------------------------------------------------

/// Calls and execution time per statement, from `pg_stat_statements`.
type Statements = HashMap<String, (i64, f64)>;

struct StatementCounter {
    /// A connection to the statistics database, not the measured one: the
    /// extension's views in the measured database would be owner-executed
    /// views its restricted runtime login could reach, which Hot refuses.
    pool: sqlx::PgPool,
    /// The measured database.
    database: String,
}

impl StatementCounter {
    /// `None` when the server does not load `pg_stat_statements`.
    async fn new(stats_uri: &str, database: &str) -> Option<Self> {
        let pool = sqlx::PgPool::connect(stats_uri).await.ok()?;
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .execute(&pool)
            .await
            .ok()?;
        let counter = Self {
            pool,
            database: database.to_string(),
        };
        counter.try_snapshot().await.ok().map(|_| counter)
    }

    /// Forget this database's statements so far (the migrations and the
    /// warm-up), so the entries the measurements need are not evicted.
    async fn reset(&self) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "{MARK} SELECT pg_stat_statements_reset(0, \
             (SELECT oid FROM pg_database WHERE datname = $1), 0)"
        )))
        .bind(&self.database)
        .execute(&self.pool)
        .await
        .ok();
    }

    async fn try_snapshot(&self) -> Result<Statements, sqlx::Error> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{MARK} SELECT query, SUM(calls)::bigint AS calls, SUM(total_exec_time)::float8 AS ms \
             FROM pg_stat_statements \
             WHERE dbid = (SELECT oid FROM pg_database WHERE datname = $1) \
               AND toplevel AND query NOT LIKE '%qwbench_harness%' \
             GROUP BY query"
        )))
        .bind(&self.database)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.get("query"), (row.get("calls"), row.get("ms"))))
            .collect())
    }

    async fn snapshot(&self) -> Statements {
        self.try_snapshot().await.unwrap_or_default()
    }
}

/// What a window of work cost, less what the idle services issue anyway.
struct StatementCost {
    /// Statements per item, by statement.
    per_item: Vec<(String, f64, f64)>,
    statements: f64,
    exchanges: f64,
    transactions: f64,
    server_ms: f64,
}

fn statement_cost(
    before: &Statements,
    after: &Statements,
    seconds: f64,
    idle_per_second: &HashMap<String, f64>,
    items: usize,
) -> StatementCost {
    let mut per_item = Vec::new();
    let (mut statements, mut scoped, mut transactions, mut server_ms) = (0.0, 0.0, 0.0, 0.0);
    for (query, (calls, ms)) in after {
        let (calls_before, ms_before) = before.get(query).copied().unwrap_or((0, 0.0));
        let idle = idle_per_second.get(query).copied().unwrap_or(0.0) * seconds;
        let calls = ((calls - calls_before) as f64 - idle).max(0.0) / items as f64;
        if calls < 0.005 {
            continue;
        }
        let ms = (ms - ms_before).max(0.0) / items as f64;
        statements += calls;
        server_ms += ms;
        let upper = query.to_ascii_uppercase();
        if upper.contains("APPLY_SCOPE") {
            scoped += calls;
        }
        if upper.trim_start().starts_with("COMMIT") {
            transactions += calls;
        }
        per_item.push((query.clone(), calls, ms));
    }
    per_item.sort_by(|a, b| b.1.total_cmp(&a.1));
    StatementCost {
        per_item,
        statements,
        // A scoped transaction opens with `BEGIN; SELECT hot.apply_scope(...)`
        // in one message, so those two statements are one exchange.
        exchanges: statements - scoped,
        transactions,
        server_ms,
    }
}

fn one_line(query: &str) -> String {
    let line = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = line.chars().take(110).collect();
    if short.len() < line.len() {
        short.push('…');
    }
    short.replace('|', "\\|")
}

// ---------------------------------------------------------------------------
// The benchmark
// ---------------------------------------------------------------------------

struct Bench {
    api: Api,
    reader: Reader,
    server: HoldServer,
    counter: Option<StatementCounter>,
    idle_per_second: HashMap<String, f64>,
    jitter: Jitter,
    /// Wait this long before each publish, in place of the short random gap
    /// after it.
    idle_before: Option<Duration>,
    report: Report,
    out: PathBuf,
    label: String,
}

impl Bench {
    /// Publish `count` events one at a time, each after the last one's runs
    /// ended and a short random gap. Returns the publishes and, on
    /// PostgreSQL, what they cost in statements.
    async fn sequence(
        &mut self,
        event_type: &'static str,
        finishes: &'static str,
        count: usize,
        data: impl Fn(usize) -> Value,
    ) -> (Vec<Published>, Option<StatementCost>) {
        let before = match &self.counter {
            Some(counter) => Some(counter.snapshot().await),
            None => None,
        };
        let started = Instant::now();
        let mut published = Vec::with_capacity(count);
        for seq in 0..count {
            if let Some(idle) = self.idle_before {
                tokio::time::sleep(idle).await;
            }
            let done = self.reader.finished(finishes).await;
            published.push(self.api.publish(event_type, data(seq)).await);
            let reader = &self.reader;
            wait_until(finishes, Duration::from_secs(120), async || {
                reader.finished(finishes).await > done
            })
            .await;
            if self.idle_before.is_none() {
                tokio::time::sleep(self.jitter.gap()).await;
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        let cost = match (&self.counter, before) {
            (Some(counter), Some(before)) => Some(statement_cost(
                &before,
                &counter.snapshot().await,
                seconds,
                &self.idle_per_second,
                count,
            )),
            _ => None,
        };
        (published, cost)
    }

    fn statements(&mut self, key: &str, what: &str, cost: Option<StatementCost>) {
        let Some(cost) = cost else { return };
        let mut text = format!(
            "PostgreSQL per {what}: {:.1} statements in {:.1} client/server exchanges, {:.1} transactions, {:.2} ms of server execution time (idle background statements subtracted).\n\n",
            cost.statements, cost.exchanges, cost.transactions, cost.server_ms
        );
        text.push_str(
            "| statements per item | server ms per item | statement |\n|---:|---:|---|\n",
        );
        for (query, calls, ms) in cost.per_item.iter().take(12) {
            let _ = writeln!(text, "| {calls:.2} | {ms:.3} | `{}` |", one_line(query));
        }
        self.report.note(&text);
        let mut all = String::from("statements_per_item\tserver_ms_per_item\tstatement\n");
        for (query, calls, ms) in &cost.per_item {
            let _ = writeln!(all, "{calls:.3}\t{ms:.4}\t{}", one_line(query));
        }
        write(
            &self
                .out
                .join(format!("{}.statements.{key}.tsv", self.label)),
            &all,
        );
        self.report.data.insert(
            format!("{key}_postgres"),
            json!({"statements": cost.statements, "exchanges": cost.exchanges,
                   "transactions": cost.transactions, "server_ms": cost.server_ms}),
        );
    }

    /// The stages of one event's path, from its publish to its handler's
    /// first instruction.
    fn event_stages(stages: &mut Stages, sent_us: Option<i64>, run: &RunRow) {
        let info = run.info.as_ref();
        let claimed = info_time_us(info, "claimed_at");
        // A memory queue records no enqueue time, only how long the wait was.
        let enqueued = info_time_us(info, "enqueued_at")
            .or_else(|| Some(claimed? - info_us(info, "queue_wait_us")?));
        let first = clock_us(run.result.as_ref(), "t0");

        stages.add("total: publish to first instruction", sent_us, first);
        stages.add(
            "total from the event row: event_time to first instruction",
            run.event_time_us,
            first,
        );
        stages.add(
            "1 producer: publish call to event row",
            sent_us,
            run.event_time_us,
        );
        stages.add(
            "2 producer: event row to enqueued",
            run.event_time_us,
            enqueued,
        );
        stages.add("3 queue: enqueued to claimed", enqueued, claimed);
        stages.add_value(
            "3a of which prefetch buffer",
            info_us(info, "prefetch_wait_us"),
        );
        stages.add_value(
            "4 worker: claimed to executor",
            info_us(info, "handoff_wait_us"),
        );
        stages.add_value(
            "5 worker: hydrate and admission check",
            info_us(info, "hydrate_us"),
        );
        stages.add_value(
            "6 worker: handler lookup",
            info_us(info, "handler_lookup_us"),
        );
        stages.add_value("7 worker: handler setup", info_us(info, "handler_setup_us"));
        stages.add_value(
            "8 worker: blocking slot wait",
            info_us(info, "blocking_slot_wait_us"),
        );
        stages.add_value("9 worker: VM thread start", info_us(info, "vm_start_us"));
        stages.add("10 run start to first instruction", run.start_us, first);
        stages.add(
            "stored: e.event_time to r.start_time",
            run.event_time_us,
            run.start_us,
        );
        stages.add_value("stored: queue_wait_us", info_us(info, "queue_wait_us"));
        stages.add_value(
            "stored: handler_dispatch_wait_us",
            info_us(info, "handler_dispatch_wait_us"),
        );
        stages.add_value(
            "stored: worker_preparation_us",
            info_us(info, "worker_preparation_us"),
        );
    }

    async fn events(&mut self, key: &str, title: &str, note: &str, count: usize) {
        let (published, cost) = self
            .sequence(
                "qwbench:probe",
                "qwbench:probe",
                count,
                |seq| json!({"seq": seq}),
            )
            .await;
        let runs = self.reader.runs().await;
        let mut stages = Stages::default();
        for publish in &published {
            let Some(run) = runs
                .iter()
                .find(|run| run.event_id == Some(publish.event_id))
            else {
                continue;
            };
            Self::event_stages(&mut stages, Some(publish.sent_us), run);
            stages.add(
                "publish call round trip",
                Some(publish.sent_us),
                Some(publish.acked_us),
            );
        }
        self.report.section(key, title, note, &stages);
        // Over minutes of idling, what the background issues swamps a few
        // events' statements, so the after-idle rows carry no count.
        if self.idle_before.is_none() {
            self.statements(key, "event", cost);
        }
    }

    /// An event sent by a handler, measured from the `send` call.
    async fn relayed(&mut self, count: usize) {
        let (published, cost) = self
            .sequence(
                "qwbench:relay",
                "qwbench:hop",
                count,
                |seq| json!({"seq": seq}),
            )
            .await;
        let runs = self.reader.runs().await;
        // A hop belongs to the relay whose send it followed most closely.
        let mut hops: Vec<&RunRow> = runs
            .iter()
            .filter(|run| run.event_type.as_deref() == Some("qwbench:hop"))
            .collect();
        hops.sort_by_key(|run| run.event_time_us);
        let mut stages = Stages::default();
        for publish in &published {
            let Some(relay) = runs
                .iter()
                .find(|run| run.event_id == Some(publish.event_id))
            else {
                continue;
            };
            let send_start = clock_us(relay.result.as_ref(), "send-start");
            let send_end = clock_us(relay.result.as_ref(), "send-end");
            let Some(hop) = hops.iter().find(|hop| {
                hop.event_time_us >= send_start.map(|us| us - 1_000)
                    && hop.event_time_us <= send_end
            }) else {
                continue;
            };
            stages.add("send() call in the sending handler", send_start, send_end);
            Self::event_stages(&mut stages, send_start, hop);
        }
        self.report.section(
            "relay",
            "Event sent by a handler (send → first instruction of the next handler)",
            "Sent one at a time. The sending handler is still finishing when its event is claimed, so this is the in-flight path for the claimer.",
            &stages,
        );
        self.statements("relay", "relay + sent event (two events, two runs)", cost);
    }

    /// Events sent by other processes (`hot eval`), which a SQLite queue's
    /// consumer learns of by polling. Run after everything else is idle.
    async fn cross_process(&mut self, count: usize, eval: impl Fn(usize) -> Command) {
        for seq in 0..count {
            let done = self.reader.finished("qwbench:xproc").await;
            let sent = eval(seq).output().expect("run hot eval");
            assert!(sent.status.success(), "{sent:?}");
            let reader = &self.reader;
            wait_until("qwbench:xproc", Duration::from_secs(120), async || {
                reader.finished("qwbench:xproc").await > done
            })
            .await;
            tokio::time::sleep(self.jitter.gap()).await;
        }
        let runs = self.reader.runs().await;
        let mut stages = Stages::default();
        for run in &runs {
            if run.event_type.as_deref() == Some("qwbench:xproc") {
                Self::event_stages(&mut stages, None, run);
            }
        }
        self.report.section(
            "cross_process",
            "Event sent by another process (`hot eval`), workers idle",
            "The sender is a separate `hot` process, so only the rows from the event row onward apply. Stage 3 is how long the consumer took to see a message another process enqueued.",
            &stages,
        );
    }

    /// How long one exchange with each server takes from this machine right
    /// now: what an item's exchange count is multiplied by.
    async fn round_trips(&mut self, key: &str, when: &str, redis_uri: Option<&str>) {
        let mut stages = Stages::default();
        if let Reader::Postgres(pool) = &self.reader
            && let Ok(mut connection) = pool.acquire().await
        {
            for _ in 0..300 {
                let started = Instant::now();
                let answered = sqlx::query(sqlx::AssertSqlSafe(format!("{MARK} SELECT 1")))
                    .execute(&mut *connection)
                    .await
                    .is_ok();
                stages.add_value(
                    "PostgreSQL: SELECT 1",
                    answered.then(|| started.elapsed().as_micros() as i64),
                );
            }
        }
        for sample in redis_uri.map_or_else(Vec::new, |uri| redis_round_trips(uri, 300)) {
            stages.add_value("Redis: PING", Some(sample));
        }
        if stages.order.is_empty() {
            return;
        }
        self.report.section(
            key,
            &format!("Server round trips, {when}"),
            "One statement or command from the harness on an open connection, 300 times. The time an event or task spends talking to a server is roughly its exchange count times this, so on a busy machine this row says how much of every other row is the machine.",
            &stages,
        );
    }

    async fn burst(&mut self, count: usize) {
        let done = self.reader.finished("qwbench:probe").await;
        let started = Instant::now();
        let publishes = (0..count).map(|seq| {
            let api = self.api.clone();
            tokio::spawn(async move { api.publish("qwbench:probe", json!({"seq": seq})).await })
        });
        let mut published = Vec::new();
        for publish in publishes.collect::<Vec<_>>() {
            published.push(publish.await.unwrap());
        }
        let reader = &self.reader;
        wait_until("the burst", Duration::from_secs(300), async || {
            reader.finished("qwbench:probe").await >= done + count as i64
        })
        .await;
        let drained = started.elapsed();
        let runs = self.reader.runs().await;
        let mut stages = Stages::default();
        for publish in &published {
            if let Some(run) = runs
                .iter()
                .find(|run| run.event_id == Some(publish.event_id))
            {
                Self::event_stages(&mut stages, Some(publish.sent_us), run);
            }
        }
        self.report.section(
            "burst",
            &format!("Burst: {count} events published at once"),
            &format!(
                "All {count} runs finished {:.0} ms after the first publish ({:.0} events per second).",
                drained.as_secs_f64() * 1_000.0,
                count as f64 / drained.as_secs_f64()
            ),
            &stages,
        );
    }

    async fn tasks(&mut self, key: &str, title: &str, note: &str, count: usize, hold_ms: usize) {
        let url = self.server.url.clone();
        let (published, cost) = self
            .sequence("qwbench:task", "qwbench:task", count, |seq| {
                if hold_ms > 0 {
                    json!({"seq": seq, "url": format!("{url}/{hold_ms}/{}", Uuid::now_v7().simple())})
                } else {
                    json!({"seq": seq})
                }
            })
            .await;
        let runs = self.reader.runs().await;
        let tasks = self.reader.tasks().await;
        let mut stages = Stages::default();
        for publish in &published {
            let Some(run) = runs
                .iter()
                .find(|run| run.event_id == Some(publish.event_id))
            else {
                continue;
            };
            let result = run.result.as_ref();
            let Some(task) = result
                .and_then(|result| find(result, "task-id"))
                .and_then(as_text)
                .and_then(|id| id.parse::<Uuid>().ok())
                .and_then(|id| tasks.get(&id))
            else {
                continue;
            };
            let timing = task.timing.as_ref();
            let time = |key: &str| timing?.get(key)?.as_str().and_then(parse_time_us);
            let value_ms = |key: &str| Some(timing?.get(key)?.as_i64()? * 1_000);
            let call = clock_us(result, "start-call");
            let returned = clock_us(result, "start-returned");
            let awaited = clock_us(result, "awaited");
            let first = clock_us(task.result.as_ref(), "t0");
            let last = clock_us(task.result.as_ref(), "t1");
            let claimed = time("claimed_at");
            // As for events: a memory queue records only the wait.
            let enqueued =
                time("enqueued_at").or_else(|| Some(claimed? - value_ms("queue_wait_ms")?));
            let workload = time("workload_started_at");

            stages.add("total: task/start call to first instruction", call, first);
            stages.add("1 producer: task/start call", call, returned);
            stages.add("2 producer: start call to task row", call, task.created_us);
            stages.add(
                "3 producer: task row to enqueued",
                task.created_us,
                enqueued,
            );
            stages.add("4 queue: enqueued to claimed", enqueued, claimed);
            let (admitted, setup) = (time("admitted_at"), time("code_setup_started_at"));
            stages.add("5 worker: claimed to admitted", claimed, admitted);
            stages.add("6 worker: timing write, lease and slots", admitted, setup);
            stages.add_value("6a of which capacity wait", value_ms("capacity_wait_ms"));
            stages.add("7 worker: code setup", setup, workload);
            stages.add("5 to 7: claimed to VM hand-off", claimed, workload);
            stages.add("8 VM hand-off to first instruction", workload, first);
            stages.add("await: last instruction to await returning", last, awaited);
            stages.add(
                "await: task stop_time to await returning",
                task.stop_us,
                awaited,
            );
            stages.add("task: last instruction to stop_time", last, task.stop_us);
            stages.add_value("stored: waiting_ms", value_ms("waiting_ms"));
            stages.add_value("stored: queue_wait_ms", value_ms("queue_wait_ms"));
            stages.add_value(
                "stored: worker_preparation_ms",
                value_ms("worker_preparation_ms"),
            );
            stages.add_value("stored: execution_ms", value_ms("execution_ms"));
        }
        self.report.section(key, title, note, &stages);
        self.statements(
            key,
            "started + awaited task (one event, two runs, one task)",
            cost,
        );
    }
}

/// The restricted login made for a benchmark database.
fn runtime_role_name(database: &str) -> String {
    format!("hot_qwbench_{}", &database[database.len() - 12..])
}

/// Run SQL through `command`, which is a whole `psql` command line that
/// reaches the server; the login, database and variables are added to it.
fn psql(command: &str, admin_uri: &str, database: &str, vars: &[(&str, &str)], sql: &str) {
    let mut words = command.split_whitespace();
    let mut psql = Command::new(words.next().expect("HOT_BENCH_PSQL names a command"));
    psql.args(words);
    // The login is the owner's; its password comes from the URI.
    let (user, password) = admin_uri
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .map(|(login, _)| login.split_once(':').unwrap_or((login, "")))
        .expect("a login in HOT_TEST_POSTGRES_URI");
    psql.args(["-U", user, "-d", database, "-v", "ON_ERROR_STOP=1", "-q"]);
    for (name, value) in vars {
        psql.args(["-v", &format!("{name}={value}")]);
    }
    let mut child = psql
        .env("PGPASSWORD", password)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("start psql");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "psql failed");
}

/// A connection to a plain-TCP Redis, authenticated and on the URI's database.
fn redis_connect(uri: &str) -> Option<(TcpStream, std::io::BufReader<TcpStream>)> {
    let rest = uri.strip_prefix("redis://")?;
    let (address, db) = rest.split_once('/').unwrap_or((rest, ""));
    let (auth, address) = address.rsplit_once('@').unwrap_or(("", address));
    let stream = TcpStream::connect(address).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_nodelay(true).ok()?;
    let mut connection = (stream.try_clone().ok()?, std::io::BufReader::new(stream));
    if let Some(password) = auth
        .rsplit(':')
        .next()
        .filter(|password| !password.is_empty())
    {
        redis_send(&mut connection, &format!("AUTH {password}"))?;
    }
    if !db.is_empty() {
        redis_send(&mut connection, &format!("SELECT {db}"))?;
    }
    Some(connection)
}

/// Send one inline command and return its reply line, or `None` on an error.
fn redis_send(
    (stream, reader): &mut (TcpStream, std::io::BufReader<TcpStream>),
    command: &str,
) -> Option<String> {
    use std::io::BufRead;
    stream.write_all(format!("{command}\r\n").as_bytes()).ok()?;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    (!line.starts_with('-')).then(|| line.trim().to_string())
}

/// Send one inline command to a plain-TCP Redis and return its reply line.
fn redis_command(uri: &str, command: &str) -> Option<String> {
    redis_send(&mut redis_connect(uri)?, command)
}

/// How long `count` PINGs each took on one connection, in microseconds.
fn redis_round_trips(uri: &str, count: usize) -> Vec<i64> {
    let Some(mut connection) = redis_connect(uri) else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|_| {
            let started = Instant::now();
            redis_send(&mut connection, "PING")?;
            Some(started.elapsed().as_micros() as i64)
        })
        .collect()
}

async fn queue_wait_benchmark(mut backend: Backend) {
    let events = env_usize("HOT_BENCH_EVENTS", 200);
    let tasks = env_usize("HOT_BENCH_TASKS", 100);
    let burst = env_usize("HOT_BENCH_BURST", 100);
    let long_tasks = env_usize("HOT_BENCH_LONG_TASKS", 10);
    let long_task_ms = env_usize("HOT_BENCH_LONG_TASK_MS", 2_000);
    let in_flight = env_usize("HOT_BENCH_IN_FLIGHT", 2);
    let cross_process = env_usize("HOT_BENCH_CROSS_PROCESS", 30);
    // Only PostgreSQL has a pool that goes cold.
    let cold = match backend {
        Backend::Postgres(_) => env_usize("HOT_BENCH_COLD", 3),
        Backend::Sqlite { .. } => 0,
    };
    let cold_idle_seconds = env_usize("HOT_BENCH_COLD_IDLE_SECONDS", 75);
    let worker_threads = env_usize("HOT_BENCH_WORKER_THREADS", 4);
    let task_concurrency = env_usize("HOT_BENCH_TASK_CONCURRENCY", 4);
    // `hot deploy --local` is another process, which a memory queue's worker
    // never hears from, so that backend measures the live build.
    let bundle = !matches!(backend, Backend::Sqlite { queue: "memory" })
        && !std::env::var("HOT_BENCH_DEPLOY").is_ok_and(|mode| mode == "live");
    let out = std::env::var_os("HOT_BENCH_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            repo_root().join(format!("target/queue-wait-runs/{}", now_us() / 1_000_000))
        });
    std::fs::create_dir_all(&out).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    let home = hot_home(project);
    write(&project.join("hot/src/qwbench.hot"), SOURCE);
    let conf = |backend: &Backend| {
        format!(
            "::hot::conf ns\n\nhot.set.project \"qwbench\"\nhot.project.qwbench.src.paths [\"./hot/src\"]\n\
             hot.worker.threads {worker_threads}\nhot.task.max-concurrent {task_concurrency}\n\
             hot.box.enabled false\n{}",
            backend.conf()
        )
    };
    write(&project.join("hot.hot"), &conf(&backend));

    // A restricted runtime login, as production has, when psql is at hand.
    if let (Backend::Postgres(pg), Ok(psql_command)) =
        (&mut backend, std::env::var("HOT_BENCH_PSQL"))
    {
        let role = runtime_role_name(&pg.database);
        let (server, _) = pg.admin_uri.rsplit_once('/').unwrap();
        let (scheme, rest) = server.split_once("://").unwrap();
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        pg.runtime_uri = format!("{scheme}://{role}:{role}@{host}/{}", pg.database);
        pg.runtime_role = Some(role.clone());
        let (admin_uri, database) = (pg.admin_uri.clone(), pg.database.clone());
        let owner_uri = format!("{server}/{database}");
        write(&project.join("hot.hot"), &conf(&backend));
        psql(
            &psql_command,
            &admin_uri,
            &database,
            &[],
            &format!(
                "CREATE ROLE {role} LOGIN PASSWORD '{role}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE;"
            ),
        );
        let migrate = hot(project, &home)
            .args(["db", "migrate"])
            .env("HOT_DB_MIGRATION_URI", &owner_uri)
            .output()
            .expect("run hot db migrate");
        assert!(migrate.status.success(), "{migrate:?}");
        psql(
            &psql_command,
            &admin_uri,
            &database,
            &[("runtime_role", &role), ("schema", "hot")],
            &std::fs::read_to_string(
                repo_root().join("resources/db/postgres/provision_runtime.sql"),
            )
            .unwrap(),
        );
    }
    let label = backend.label();
    let migration_uri = match &backend {
        Backend::Postgres(pg) if pg.runtime_role.is_some() => {
            let (server, _) = pg.admin_uri.rsplit_once('/').unwrap();
            Some(format!("{server}/{}", pg.database))
        }
        _ => None,
    };
    let hot_in_project = |home: &Path| {
        let mut command = hot(project, home);
        if let Some(uri) = &migration_uri {
            command.env("HOT_DB_MIGRATION_URI", uri);
        }
        command
    };

    let log = out.join(format!("{label}.hot-dev.log"));
    println!("hot dev log: {}", log.display());
    let log_file = std::fs::File::create(&log).unwrap();
    let (api_port, app_port) = (free_port(), free_port());
    let mut dev = Dev(hot_in_project(&home)
        .arg("dev")
        .args(["--api.port", &api_port.to_string()])
        .args(["--app.port", &app_port.to_string()])
        .stdout(log_file.try_clone().unwrap())
        .stderr(log_file)
        .spawn()
        .expect("start hot dev"));
    let deadline = Instant::now() + Duration::from_secs(300);
    while TcpStream::connect(("127.0.0.1", api_port)).is_err() {
        assert!(
            Instant::now() < deadline && matches!(dev.0.try_wait(), Ok(None)),
            "hot dev did not start: {}",
            std::fs::read_to_string(&log).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let key = hot_in_project(&home)
        .args(["key", "create"])
        .output()
        .expect("create an API key");
    assert!(key.status.success(), "{key:?}");
    let key = String::from_utf8(key.stdout).unwrap();
    let api = Api {
        client: reqwest::Client::new(),
        url: format!("http://127.0.0.1:{api_port}/v1/events"),
        key: key.trim().lines().last().unwrap().trim().to_string(),
    };

    let reader = match &backend {
        Backend::Sqlite { .. } => Reader::Sqlite(
            sqlx::SqlitePool::connect(&format!(
                "sqlite:{}?mode=ro",
                home.join("db/hot.sqlite.db").display()
            ))
            .await
            .unwrap(),
        ),
        Backend::Postgres(pg) => {
            let (server, _) = pg.admin_uri.rsplit_once('/').unwrap();
            Reader::Postgres(
                sqlx::PgPool::connect(&format!("{server}/{}", pg.database))
                    .await
                    .unwrap(),
            )
        }
    };
    let counter = match &backend {
        Backend::Postgres(pg) => StatementCounter::new(&pg.stats_uri, &pg.database).await,
        Backend::Sqlite { .. } => None,
    };

    let version = Command::new(hot_bin()).arg("--version").output().unwrap();
    let mut report = Report {
        text: String::new(),
        data: serde_json::Map::new(),
    };
    report.note(&format!(
        "## {}\n\n- binary: `{}` ({})\n- deploy: {}\n- worker.threads {worker_threads}, task.max-concurrent {task_concurrency}, {in_flight} held in flight for the in-flight rows\n- statement counts: {}\n",
        backend.describe(),
        String::from_utf8_lossy(&version.stdout).trim(),
        if hot_bin().to_string_lossy().contains("/release/") { "release build" } else { "see path; debug builds are several times slower" },
        if bundle {
            "bundle (`hot deploy --local`), as a deployed project runs"
        } else {
            "live build, as `hot dev` serves a project being edited"
        },
        match (&backend, &counter) {
            (Backend::Postgres(_), Some(_)) => "pg_stat_statements",
            (Backend::Postgres(_), None) => "not available (the server does not preload pg_stat_statements)",
            _ => "PostgreSQL only",
        },
    ));

    let mut bench = Bench {
        api,
        reader,
        server: HoldServer::start().await,
        counter,
        idle_per_second: HashMap::new(),
        jitter: Jitter(0x5eed),
        idle_before: None,
        report,
        out: out.clone(),
        label: label.clone(),
    };

    // Warm up: compile the handlers, fill the pools and caches.
    bench
        .sequence(
            "qwbench:probe",
            "qwbench:probe",
            10,
            |seq| json!({"seq": seq}),
        )
        .await;
    if bundle {
        let deploy = hot_in_project(&home)
            .args(["deploy", "--local"])
            .output()
            .expect("deploy the bundle");
        assert!(deploy.status.success(), "{deploy:?}");
        // Probes run on the live build until the bundle's handlers are
        // loaded; wait for the first run that names another build.
        let live_builds = bench.reader.build_ids().await;
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            bench
                .sequence(
                    "qwbench:probe",
                    "qwbench:probe",
                    1,
                    |seq| json!({"seq": seq}),
                )
                .await;
            if bench.reader.build_ids().await.len() > live_builds.len() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the bundle never served an event"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    bench
        .sequence(
            "qwbench:probe",
            "qwbench:probe",
            10,
            |seq| json!({"seq": seq}),
        )
        .await;
    bench
        .sequence("qwbench:relay", "qwbench:hop", 5, |seq| json!({"seq": seq}))
        .await;
    bench
        .sequence("qwbench:task", "qwbench:task", 5, |seq| json!({"seq": seq}))
        .await;

    // What the idle services issue on their own, to subtract.
    if let Some(counter) = &bench.counter {
        counter.reset().await;
        let before = counter.snapshot().await;
        let window = Duration::from_secs(10);
        tokio::time::sleep(window).await;
        let after = counter.snapshot().await;
        bench.idle_per_second = after
            .iter()
            .map(|(query, (calls, _))| {
                let earlier = before.get(query).map_or(0, |(calls, _)| *calls);
                (
                    query.clone(),
                    (calls - earlier) as f64 / window.as_secs_f64(),
                )
            })
            .collect();
        let total: f64 = bench.idle_per_second.values().sum();
        bench.report.note(&format!(
            "Idle background: {total:.1} PostgreSQL statements per second with no events or tasks.\n"
        ));
    }

    let redis_uri = match &backend {
        Backend::Postgres(pg) => Some(pg.redis_uri.clone()),
        Backend::Sqlite { .. } => None,
    };
    bench
        .round_trips(
            "round_trips_before",
            "before the scenarios",
            redis_uri.as_deref(),
        )
        .await;

    bench
        .events(
            "event_idle",
            "Event, workers idle (publish → first instruction)",
            "Published over the API one at a time, each after the previous run ended and a random 20-80 ms gap.",
            events,
        )
        .await;

    let holds = InFlight::start(&bench.api, &bench.server, "qwbench:hold", in_flight);
    tokio::time::sleep(Duration::from_millis(500)).await;
    bench
        .events(
            "event_in_flight",
            "Event, handlers in flight (publish → first instruction)",
            &format!("As above, while {in_flight} other handlers are running (waiting on HTTP). Executors are free: {worker_threads} event slots."),
            events,
        )
        .await;
    holds.stop().await;
    // Let the held handlers end before the next rows.
    let settle = Duration::from_millis(env_usize("HOT_BENCH_HOLD_MS", 3_000) as u64 + 2_000);
    tokio::time::sleep(settle).await;

    bench.relayed(events / 2).await;
    bench.burst(burst).await;

    bench
        .tasks(
            "task_idle",
            "Code task, task worker idle (task/start → first instruction, and task/await)",
            "A handler starts a task that returns at once and awaits it; one at a time.",
            tasks,
            0,
        )
        .await;

    let holds = InFlight::start(&bench.api, &bench.server, "qwbench:hold-task", in_flight);
    tokio::time::sleep(Duration::from_millis(500)).await;
    bench
        .tasks(
            "task_in_flight",
            "Code task, tasks in flight (task/start → first instruction, and task/await)",
            &format!("As above, while {in_flight} other code tasks are running. Slots are free: task.max-concurrent {task_concurrency}."),
            tasks,
            0,
        )
        .await;
    holds.stop().await;
    tokio::time::sleep(settle).await;

    if long_tasks > 0 {
        bench
            .tasks(
                "task_long",
                &format!("task/await of a {long_task_ms} ms task"),
                "The await rows show how late the awaiting run learns of the task's end.",
                long_tasks,
                long_task_ms,
            )
            .await;
    }

    if matches!(backend, Backend::Sqlite { queue: "sqlite" }) && cross_process > 0 {
        bench
            .cross_process(cross_process, |seq| {
                let mut eval = hot_in_project(&home);
                eval.args(["eval", &format!(r#"send("qwbench:xproc", {{seq: {seq}}})"#)]);
                eval
            })
            .await;
    }

    if cold > 0 {
        bench.idle_before = Some(Duration::from_secs(cold_idle_seconds as u64));
        bench
            .events(
                "event_cold",
                &format!("Event after {cold_idle_seconds} s with no events (publish → first instruction)"),
                "Each publish follows an idle period longer than the PostgreSQL pool's 60 s idle timeout, so connections that only event work keeps open have been closed.",
                cold,
            )
            .await;
        bench.idle_before = None;
    }

    bench
        .round_trips(
            "round_trips_after",
            "after the scenarios",
            redis_uri.as_deref(),
        )
        .await;

    let Bench { report, reader, .. } = bench;
    write(&out.join(format!("{label}.md")), &report.text);
    write(
        &out.join(format!("{label}.json")),
        &serde_json::to_string_pretty(&Value::Object(report.data)).unwrap(),
    );
    println!("Report written to {}", out.display());

    // Stop `hot dev` before its database goes away.
    match reader {
        Reader::Sqlite(pool) => pool.close().await,
        Reader::Postgres(pool) => pool.close().await,
    }
    drop(dev);
}

impl Reader {
    /// The builds runs have used so far.
    async fn build_ids(&self) -> Vec<String> {
        match self {
            Reader::Sqlite(pool) => sqlx::query_scalar("SELECT DISTINCT hex(build_id) FROM run")
                .fetch_all(pool)
                .await
                .unwrap_or_default(),
            Reader::Postgres(pool) => sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "{MARK} SELECT DISTINCT build_id::text FROM hot.run"
            )))
            .fetch_all(pool)
            .await
            .unwrap_or_default(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "benchmark; run with scripts/queue-wait-benchmark.sh or `-- --ignored --nocapture`"]
async fn queue_wait_e2e_sqlite_db_memory_queue() {
    queue_wait_benchmark(Backend::Sqlite { queue: "memory" }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "benchmark; run with scripts/queue-wait-benchmark.sh or `-- --ignored --nocapture`"]
async fn queue_wait_e2e_sqlite_db_sqlite_queue() {
    queue_wait_benchmark(Backend::Sqlite { queue: "sqlite" }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "benchmark; needs HOT_TEST_POSTGRES_URI and HOT_REDIS_URI"]
async fn queue_wait_e2e_postgres_db_redis_queue() {
    let (Ok(admin_uri), Ok(redis_uri)) = (
        std::env::var("HOT_TEST_POSTGRES_URI"),
        std::env::var("HOT_REDIS_URI"),
    ) else {
        eprintln!("skipping: HOT_TEST_POSTGRES_URI and HOT_REDIS_URI are not both set");
        return;
    };
    // The queues use Hot's own key names, so the Redis database must be this
    // benchmark's alone: it is emptied afterwards.
    match redis_command(&redis_uri, "DBSIZE").as_deref() {
        Some(":0") => {}
        Some(size) => {
            eprintln!(
                "skipping: the Redis database at HOT_REDIS_URI is not empty ({size}); use a throwaway server or an unused database number"
            );
            return;
        }
        None => {
            eprintln!("skipping: HOT_REDIS_URI is not a reachable plain redis:// server");
            return;
        }
    }
    let admin = sqlx::PgPool::connect(&admin_uri)
        .await
        .expect("connect to HOT_TEST_POSTGRES_URI");
    let database = format!("hot_queue_wait_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .execute(&admin)
        .await
        .expect("create the benchmark database");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE {database}_stats"
    )))
    .execute(&admin)
    .await
    .expect("create the statistics database");
    let (server, _) = admin_uri.rsplit_once('/').expect("a database in the URI");
    let role = runtime_role_name(&database);

    let benchmark = AssertUnwindSafe(queue_wait_benchmark(Backend::Postgres(Postgres {
        admin_uri: admin_uri.clone(),
        runtime_uri: format!("{server}/{database}"),
        database: database.clone(),
        runtime_role: None,
        stats_uri: format!("{server}/{database}_stats"),
        redis_uri: redis_uri.clone(),
    })))
    .catch_unwind()
    .await;

    // Clean up whether or not the benchmark finished.
    for name in [database.clone(), format!("{database}_stats")] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
        )))
        .execute(&admin)
        .await
        .expect("drop a benchmark database");
    }
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
        .execute(&admin)
        .await
        .ok();
    redis_command(&redis_uri, "FLUSHDB");
    if let Err(panic) = benchmark {
        std::panic::resume_unwind(panic);
    }
}

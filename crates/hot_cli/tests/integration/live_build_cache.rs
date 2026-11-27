//! End-to-end regression test for live-build bytecode cache sharing between
//! `hot_worker` and `hot_task_worker` under `hot dev`.
//!
//! Alternates events and code tasks on one live build and verifies:
//! 1. Unchanged source compiles at most once and never recompiles across
//!    alternating event/task runs (no cache ping-pong eviction).
//! 2. Live builds write only content-hashed `.bc.zst` entries (no UUID keys).
//! 3. Editing the live source file takes effect on the very next task (no
//!    stale bytecode) and is subsequently reused without recompilation.

use serde_json::{Value, json};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn source_for(answer: &str) -> String {
    format!(
        r#"::livecache ns

::task ::hot::task

answer fn (args: Any): Str {{
    "{answer}"
}}

dispatch meta {{on-event: "livecache:run"}}
fn (event) {{
    info ::task/start(::livecache/answer, {{seq: event.data.seq}})
    res ::task/await(info.id)
    {{seq: event.data.seq, status: res.status, answer: res.result}}
}}
"#
    )
}

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn hot_home(dir: &Path) -> PathBuf {
    let home = dir.join(".test-hot-home");
    std::fs::create_dir_all(&home).unwrap();
    let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
    std::os::unix::fs::symlink(pkg.canonicalize().unwrap(), home.join("pkg")).unwrap();
    home
}

fn hot(dir: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hot"));
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Dev(Child);

impl Drop for Dev {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

async fn publish_event(client: &reqwest::Client, api_port: u16, api_key: &str, seq: i64) -> Uuid {
    let response = client
        .post(format!("http://127.0.0.1:{api_port}/v1/events"))
        .bearer_auth(api_key)
        .json(&json!({
            "event_type": "livecache:run",
            "event_data": {"seq": seq}
        }))
        .send()
        .await
        .expect("POST /v1/events");
    let status = response.status();
    let body: Value = response.json().await.expect("parse event response");
    assert!(
        status.is_success(),
        "publish event seq={seq} failed ({status}): {body}"
    );
    body["data"]["event_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing data.event_id in response: {body}"))
        .parse()
        .unwrap()
}

async fn wait_for_event_result(home: &Path, event_id: Uuid, log_path: &Path) -> Value {
    let uri = format!("sqlite:{}?mode=ro", home.join("db/hot.sqlite.db").display());
    let pool = sqlx::SqlitePool::connect(&uri).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let result = loop {
        let row: Option<(i64, Option<String>)> = sqlx::query_as(
            "SELECT status_id, result FROM run WHERE event_id = ? AND run_type_id = 2",
        )
        .bind(event_id)
        .fetch_optional(&pool)
        .await
        .unwrap();

        if let Some((status_id, raw_result)) = row
            && status_id != 1
        {
            assert_eq!(
                status_id,
                2,
                "event run for {event_id} failed (status_id={status_id}, result={raw_result:?})\n{}",
                std::fs::read_to_string(log_path).unwrap_or_default()
            );
            let parsed: Value =
                serde_json::from_str(raw_result.as_deref().unwrap_or("null")).unwrap();
            break parsed;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for event {event_id}\n{}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    pool.close().await;
    result
}

fn count_compiles(log_slice: &str) -> (usize, usize) {
    let event_compiles = log_slice
        .matches("Compiling project from source for 'livecache'")
        .count();
    let task_compiles = log_slice
        .matches("Bytecode cache miss — compiling live build from source")
        .count();
    (event_compiles, task_compiles)
}

#[tokio::test]
async fn live_build_alternating_events_and_tasks_share_cache_and_pick_up_edits() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    let source_path = project.join("hot/src/livecache.hot");
    write_file(
        &project.join("hot.hot"),
        "::hot::conf ns\n\nhot.set.project \"livecache\"\nhot.project.livecache.src.paths [\"./hot/src\"]\nhot.box.enabled false\n",
    );
    write_file(&source_path, &source_for("v1"));
    let home = hot_home(project);

    let log = project.join("dev.log");
    let log_file = std::fs::File::create(&log).unwrap();
    let (api_port, app_port) = (free_port(), free_port());
    let _dev = Dev(hot(project, &home)
        .args(["--log.level", "debug", "dev"])
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
            "hot dev did not start:\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let key_out = hot(project, &home)
        .args(["key", "create"])
        .output()
        .expect("create API key");
    assert!(key_out.status.success(), "{key_out:?}");
    let api_key = String::from_utf8(key_out.stdout)
        .unwrap()
        .trim()
        .lines()
        .last()
        .unwrap()
        .trim()
        .to_string();
    let client = reqwest::Client::new();

    // Round 1 warms the shared live-build cache.
    let e1 = publish_event(&client, api_port, &api_key, 1).await;
    let r1 = wait_for_event_result(&home, e1, &log).await;
    assert_eq!(r1["status"], "completed", "round 1: {r1}");
    assert_eq!(r1["answer"], "v1", "round 1: {r1}");

    let log_after_r1 = std::fs::read_to_string(&log).unwrap();
    let (r1_event_compiles, r1_task_compiles) = count_compiles(&log_after_r1);
    assert!(
        r1_event_compiles + r1_task_compiles <= 1,
        "round 1 should compile livecache at most once across both workers (event={r1_event_compiles}, task={r1_task_compiles}):\n{log_after_r1}"
    );
    let offset_after_r1 = log_after_r1.len();

    // Rounds 2 and 3 alternate events and tasks on unchanged source.
    // Neither worker may recompile or invalidate the other's cache entry.
    for seq in [2, 3] {
        let ev = publish_event(&client, api_port, &api_key, seq).await;
        let res = wait_for_event_result(&home, ev, &log).await;
        assert_eq!(res["status"], "completed", "round {seq}: {res}");
        assert_eq!(res["answer"], "v1", "round {seq}: {res}");
    }

    let log_after_r3 = std::fs::read_to_string(&log).unwrap();
    let r2_r3_log = &log_after_r3[offset_after_r1..];
    let (r2_r3_event_compiles, r2_r3_task_compiles) = count_compiles(r2_r3_log);
    assert_eq!(
        (r2_r3_event_compiles, r2_r3_task_compiles),
        (0, 0),
        "unchanged alternating event/task runs must not recompile:\n{r2_r3_log}"
    );
    assert!(
        !r2_r3_log.contains("Bytecode cache invalid"),
        "hot_worker must not falsely invalidate live bytecode:\n{r2_r3_log}"
    );
    assert!(
        r2_r3_log.matches("Bytecode cache hit").count() >= 2,
        "expected cache hits across rounds 2 and 3:\n{r2_r3_log}"
    );

    // Only one content-hashed .bc.zst file should exist in HOT_HOME/cache/bytecode (no UUID key).
    let bc_files: Vec<String> = std::fs::read_dir(home.join("cache/bytecode"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".bc.zst").then_some(name)
        })
        .collect();
    assert_eq!(
        bc_files.len(),
        1,
        "expected a single shared content-hashed bytecode cache file, found {bc_files:?}"
    );
    let stem = bc_files[0].strip_suffix(".bc.zst").unwrap();
    assert_eq!(
        stem.len(),
        64,
        "live build bytecode file must use a 64-hex content hash, found {bc_files:?}"
    );

    // Edit the live source file: the very next task must return "v2" (no stale bytecode).
    write_file(&source_path, &source_for("v2"));
    let e4 = publish_event(&client, api_port, &api_key, 4).await;
    let r4 = wait_for_event_result(&home, e4, &log).await;
    assert_eq!(r4["status"], "completed", "round 4: {r4}");
    assert_eq!(
        r4["answer"], "v2",
        "very next task after a live source edit must execute the updated code: {r4}"
    );

    // Round 5 on the edited source reuses the new v2 cache with zero recompiles.
    let offset_after_r4 = std::fs::read_to_string(&log).unwrap().len();
    let e5 = publish_event(&client, api_port, &api_key, 5).await;
    let r5 = wait_for_event_result(&home, e5, &log).await;
    assert_eq!(r5["status"], "completed", "round 5: {r5}");
    assert_eq!(r5["answer"], "v2", "round 5: {r5}");

    let log_after_r5 = std::fs::read_to_string(&log).unwrap();
    let r5_log = &log_after_r5[offset_after_r4..];
    let (r5_event_compiles, r5_task_compiles) = count_compiles(r5_log);
    assert_eq!(
        (r5_event_compiles, r5_task_compiles),
        (0, 0),
        "second run after edit must reuse the new cache without recompiling:\n{r5_log}"
    );
}

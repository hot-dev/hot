//! End to end: run a Hot program holding sentinel secrets through `hot run`
//! with a real database, under both the JIT and the interpreter, then look for
//! the sentinels in every column of every table.
//!
//! `HOT_TEST_POSTGRES_URI` adds a PostgreSQL pass, under its own schema.
#![cfg(unix)]

use base64::Engine as _;
use sqlx::Row;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const DECLARED: &str = "SENTINEL-DECLARED-KEY-11111";
const CTX_FILE: &str = "SENTINEL-CTX-FILE-KEY-22222";
const SET_SECRET: &str = "SENTINEL-SET-SECRET-33333";
const SET_SECRET_MAP: &str = "SENTINEL-SET-SECRET-MAP-44444";
const CTX_SET: &str = "SENTINEL-CTX-SET-55555";
const CTX_SET_MAP: &str = "SENTINEL-CTX-SET-MAP-66666";
const MARKED: &str = "SENTINEL-MARKED-77777";
const MARKED_LATE: &str = "SENTINEL-MARKED-LATE-88888";
const SPECIAL: &str = "SENTINEL+SPECIAL/VALUE=99999 x&y";
const MINTED: &str = "SENTINEL-MINTED-AAAAA";
const OPTS: &str = "SENTINEL-OPTS-KEY-BBBBB";
const OAUTH_ACCESS: &str = "SENTINEL-OAUTH-ACCESS-CCCCC";
const OAUTH_REFRESH: &str = "SENTINEL-OAUTH-REFRESH-DDDDD";
const OAUTH_COOKIE: &str = "SENTINEL-OAUTH-COOKIE-EEEEE";
const FAILURE: &str = "SENTINEL-FAILURE-FFFFF";
const FAILURE_BYTES: &str = "SENTINEL-FAILURE-BYTES-GGGGG";
const FAILURE_RESPONSE: &str = "SENTINEL-FAILURE-RESPONSE-HHHHH";
const UNMARKED_BYTES: &str = "SENTINEL-UNMARKED-BYTES-IIIII";

const SENTINELS: &[&str] = &[
    DECLARED,
    CTX_FILE,
    SET_SECRET,
    SET_SECRET_MAP,
    CTX_SET,
    CTX_SET_MAP,
    MARKED,
    MARKED_LATE,
    SPECIAL,
    MINTED,
    OPTS,
    OAUTH_ACCESS,
    OAUTH_REFRESH,
    OAUTH_COOKIE,
    FAILURE,
    FAILURE_BYTES,
    FAILURE_RESPONSE,
    UNMARKED_BYTES,
];

/// Programs that end in an Err, which a failure's message has to render.
const RETURNS_ERR: &[&str] = &[
    "::secrets/fails-with-bytes()",
    "::secrets/fails-with-response()",
    "::secrets/fails-with-unmarked-bytes()",
];

const PROGRAM: &str = r#"::secrets ns

base
meta {ctx: {"t.base": {required: false, secret: false}}}
fn (): Str { ::hot::ctx/get("t.base") }

helper fn (auth: Str): Int { length(auth) }

api-call fn (key: Str): Any {
    ::hot::http/request(
        "POST",
        `${base()}/echo`,
        {"Authorization": `Bearer ${key}`, "X-Api-Key": key},
        `client_secret=${key}&grant_type=client_credentials`
    )
}

composed fn (key: Str): Vec {
    basic ::hot::base64/encode(`user:${key}`)
    basic-url ::hot::base64/encode-url(`a-longer-user:${key}`)
    query ::hot::uri/encode(key)
    [
        helper(`Bearer ${key}`),
        helper(`Basic ${basic}`),
        helper(`Basic ${basic-url}`),
        helper(`q=${query}&r=1`),
        helper(`{"nested":"${key}"}`),
        is-err(api-call(key)),
    ]
}

declared-keys
meta {ctx: {"t.declared": {required: false}, "t.ctx-file": {required: false}}}
fn (): Vec {
    [
        composed(::hot::ctx/get("t.declared")),
        composed(::hot::ctx/get("t.ctx-file")),
    ]
}

set-secrets fn (): Vec {
    ::hot::ctx/set-secret("t.runtime", "SENTINEL-SET-SECRET-33333")
    ::hot::ctx/set-secret({"t.runtime-map": "SENTINEL-SET-SECRET-MAP-44444"})
    [
        composed(::hot::ctx/get("t.runtime")),
        composed(::hot::ctx/get("t.runtime-map")),
    ]
}

set-declared
meta {ctx: {"t.set": {required: false}, "t.set-map": {required: false}}}
fn (): Vec {
    ::hot::ctx/set("t.set", "SENTINEL-CTX-SET-55555")
    ::hot::ctx/set({"t.set-map": "SENTINEL-CTX-SET-MAP-66666"})
    [
        composed(::hot::ctx/get("t.set")),
        composed(::hot::ctx/get("t.set-map")),
    ]
}

// Marked as the first thing done with it.
marked fn (key: Str): Vec {
    composed(::hot::ctx/mark-secret(key))
}

// Marked after a pause that outlasts the emitter's batch interval, so the
// frames holding it in their args were already written.
marked-late-inner fn (key: Str): Vec {
    is-err(::hot::http/request("GET", `${base()}/slow`, {}, ""))
    composed(::hot::ctx/mark-secret(key))
}
marked-late fn (key: Str): Vec { marked-late-inner(key) }

marked-map fn (opts: Map): Vec {
    ::hot::ctx/mark-secret(opts)
    composed(opts.nested.value)
}

// Nothing marks these: their field names are what give them away.
mint fn (): Map {
    {"access_token": "SENTINEL-MINTED-AAAAA", "token_type": "bearer", "next_page_token": "kept-cursor"}
}

with-opts fn (url: Str, opts: Map): Int {
    length(opts.api-key)
}

oauth fn (): Any {
    response ::hot::http/request(
        "POST",
        `${base()}/token`,
        {"Content-Type": "application/x-www-form-urlencoded"},
        "grant_type=client_credentials"
    )
    is-ok(response)
}

all fn (): Vec {
    [
        declared-keys(),
        set-secrets(),
        set-declared(),
        marked("SENTINEL-MARKED-77777"),
        marked-late("SENTINEL-MARKED-LATE-88888"),
        marked-map({nested: {value: "SENTINEL+SPECIAL/VALUE=99999 x&y"}}),
        mint().token_type,
        with-opts("https://example.com", {"api-key": "SENTINEL-OPTS-KEY-BBBBB"}),
        oauth(),
    ]
}

fails fn (): Any {
    key ::hot::ctx/mark-secret("SENTINEL-FAILURE-FFFFF")
    fail(`request failed for Bearer ${key}`)
}

fails-with-bytes fn (): Any {
    key ::hot::ctx/mark-secret("SENTINEL-FAILURE-BYTES-GGGGG")
    err({status: 400, body-bytes: Bytes(`x ${key} y`), nested: [{raw: Bytes(key)}]})
}

fails-with-response fn (): Any {
    key ::hot::ctx/mark-secret("SENTINEL-FAILURE-RESPONSE-HHHHH")
    err(::hot::http/request("POST", `${base()}/refuse`, {"X-Thing": key}, "hello"))
}

fails-with-unmarked-bytes fn (): Any {
    err({status: 400, body-bytes: Bytes("{\"access_token\":\"SENTINEL-UNMARKED-BYTES-IIIII\"}")})
}
"#;

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

/// A `HOT_HOME` for one test directory, holding only the checkout's packages.
fn hot_home(dir: &Path) -> String {
    let home = dir.join(".test-hot-home");
    if !home.join("pkg").exists() {
        std::fs::create_dir_all(&home).unwrap();
        let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
        std::os::unix::fs::symlink(pkg.canonicalize().unwrap(), home.join("pkg")).unwrap();
    }
    home.display().to_string()
}

fn run_hot(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(hot_binary());
    command
        .args(args)
        .current_dir(dir)
        .env("HOT_HOME", hot_home(dir))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("run hot");
    let deadline = Instant::now() + Duration::from_secs(180);
    while child.try_wait().expect("wait for hot").is_none() {
        if Instant::now() > deadline {
            child.kill().ok();
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    child.wait_with_output().expect("collect hot output")
}

/// A server answering the program's requests: a token exchange, an echo of
/// what it was sent, a refusal that repeats it, and a slow answer.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 8192];
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .ok();
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let line = request.lines().next().unwrap_or_default().to_string();
                if line.contains("/slow") {
                    std::thread::sleep(Duration::from_millis(1200));
                }
                let (extra_headers, body) = if line.contains("/token") {
                    (
                        format!("Set-Cookie: session={OAUTH_COOKIE}; Path=/\r\n"),
                        format!(
                            r#"{{"access_token":"{OAUTH_ACCESS}","refresh_token":"{OAUTH_REFRESH}","token_type":"bearer","expires_in":3600}}"#
                        ),
                    )
                } else if line.contains("/echo") || line.contains("/refuse") {
                    // What a provider's error page may repeat back.
                    (String::new(), request)
                } else {
                    (String::new(), "ok".to_string())
                };
                let status = if line.contains("/refuse") {
                    "400 Bad Request"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).ok();
            });
        }
    });
    format!("http://{address}")
}

/// Every form of the sentinels a record could hold them in.
fn needles() -> Vec<String> {
    let mut needles = Vec::new();
    for sentinel in SENTINELS {
        needles.push(sentinel.to_string());
        needles.push(percent_encode(sentinel));
        for composed in [
            sentinel.to_string(),
            format!("user:{sentinel}"),
            format!("a-longer-user:{sentinel}"),
        ] {
            needles.push(base64::engine::general_purpose::STANDARD.encode(&composed));
            needles.push(base64::engine::general_purpose::URL_SAFE.encode(&composed));
        }
    }
    needles.sort();
    needles.dedup();
    needles
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn write_project(dir: &Path, base: &str, db_uri: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("hot.hot"),
        format!(
            "::hot::conf ns\nhot.set.project \"masking\"\nhot.project.masking.src.paths [\"./src\"]\nhot.db.uri \"{db_uri}\"\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/secrets.hot"), PROGRAM).unwrap();
    std::fs::write(
        dir.join("ctx.hot"),
        format!(
            "::ctxfile ns\n::hot::ctx/set(\"t.declared\", \"{DECLARED}\")\n::hot::ctx/set(\"t.ctx-file\", \"{CTX_FILE}\")\n::hot::ctx/set(\"t.base\", \"{base}\")\n"
        ),
    )
    .unwrap();
}

/// Which `needles` `value` holds: as itself, inside base64 of any longer text,
/// or inside Bytes, which JSON holds as an array of numbers.
fn found_in(value: &str, needles: &[String]) -> Vec<String> {
    let mut texts = vec![value.to_string()];
    let is_base64 = |c: char| c.is_ascii_alphanumeric() || "+/-_=".contains(c);
    for run in value.split(|c| !is_base64(c)).filter(|run| run.len() >= 16) {
        let run = run
            .trim_end_matches('=')
            .replace('-', "+")
            .replace('_', "/");
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD_NO_PAD.decode(&run) {
            texts.push(String::from_utf8_lossy(&decoded).into_owned());
        }
    }
    for array in value.split('[').skip(1) {
        let numbers = array.split(']').next().unwrap_or_default();
        let bytes: Option<Vec<u8>> = numbers
            .split(',')
            .map(|number| number.trim().parse::<u8>().ok())
            .collect();
        if let Some(bytes) = bytes.filter(|bytes| bytes.len() >= 16) {
            texts.push(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    needles
        .iter()
        .filter(|needle| {
            // `jsonb` text escapes what JSON must: compare in that form too.
            let quoted = serde_json::to_string(needle).unwrap();
            let quoted = &quoted[1..quoted.len() - 1];
            texts
                .iter()
                .any(|text| text.contains(needle.as_str()) || text.contains(quoted))
        })
        .cloned()
        .collect()
}

/// What the columns of every table hold of `needles`, as `table.column: value`.
async fn scan_sqlite(uri: &str, needles: &[String]) -> (Vec<String>, Vec<String>) {
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
    let mut found = Vec::new();
    let mut calls = Vec::new();
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
                let Ok(value) = row.try_get::<String, _>("value") else {
                    continue;
                };
                for needle in found_in(&value, needles) {
                    found.push(format!("{table}.{column} has {needle}: {value}"));
                }
                if table == "call" && (column == "args" || column == "return_value") {
                    calls.push(value);
                }
            }
        }
    }
    pool.close().await;
    (found, calls)
}

async fn scan_postgres(uri: &str, schema: &str, needles: &[String]) -> (Vec<String>, Vec<String>) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(uri)
        .await
        .expect("open the program's database");
    let tables: Vec<String> = sqlx::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = $1 AND table_type = 'BASE TABLE' ORDER BY table_name",
    )
    .bind(schema)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get("table_name"))
    .collect();
    let mut found = Vec::new();
    let mut calls = Vec::new();
    for table in &tables {
        // Every column at once, as text: a superuser sees past row policies.
        let query = format!("SELECT to_jsonb(t)::text AS value FROM {schema}.\"{table}\" t");
        for row in sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&pool)
            .await
            .unwrap()
        {
            let value: String = row.get("value");
            for needle in found_in(&value, needles) {
                found.push(format!("{table} has {needle}: {value}"));
            }
            if table == "call" {
                calls.push(value);
            }
        }
    }
    pool.close().await;
    (found, calls)
}

fn assert_ran(output: &Output, what: &str, expect_success: bool) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.success(),
        expect_success,
        "{what}: {}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
}

fn assert_nothing_found(found: &[String], what: &str) {
    assert!(
        found.is_empty(),
        "{what}: {} sentinels recorded:\n{}",
        found.len(),
        found.join("\n")
    );
}

/// The records must have been kept, with the masks where they should be and
/// ordinary values left alone: this test passes vacuously otherwise.
fn assert_records_are_useful(calls: &[String], what: &str) {
    // PostgreSQL's `jsonb` text puts a space after each colon.
    let joined = calls.join("\n").replace("\": ", "\":");
    for expected in [
        "Bearer <secret>",
        "Basic <secret>",
        "q=<secret>&r=1",
        "client_secret=<secret>&grant_type=client_credentials",
        "kept-cursor",
        "token_type",
        "https://example.com",
        "\"access_token\":\"<secret>\"",
        "expires_in",
    ] {
        assert!(
            joined.contains(expected),
            "{what}: no {expected} in the call records:\n{joined}"
        );
    }
}

fn run_program(dir: &Path, envs: &[(&str, &str)], what: &str) {
    // `hot run` also evaluates the other Hot files beside the one it's given.
    let run = |entry: &str| {
        std::fs::write(dir.join("run.hot"), format!("::run ns\n{entry}\n")).unwrap();
        run_hot(
            dir,
            &["run", "run.hot", "--emitter.type", "db", "--ctx", "ctx.hot"],
            envs,
        )
    };
    let succeeded = run("::secrets/all()");
    assert_ran(&succeeded, what, true);
    let mut printed = format!(
        "{}{}",
        String::from_utf8_lossy(&succeeded.stdout),
        String::from_utf8_lossy(&succeeded.stderr),
    );
    let failed = run("::secrets/fails()");
    assert_ran(&failed, what, false);
    printed.push_str(&String::from_utf8_lossy(&failed.stderr));
    for entry in RETURNS_ERR {
        let returned = run(entry);
        assert_ran(&returned, what, true);
        printed.push_str(&String::from_utf8_lossy(&returned.stdout));
        printed.push_str(&String::from_utf8_lossy(&returned.stderr));
    }
    assert!(
        printed.contains("request failed for Bearer <secret>"),
        "{what}: the failure should be reported, masked: {printed}"
    );
    assert!(
        found_in(&printed, &needles()).is_empty(),
        "{what}: a sentinel was printed: {printed}"
    );
}

const ENGINES: &[(&str, &[(&str, &str)])] = &[
    ("interpreter", &[("HOT_JIT", "off")]),
    ("jit", &[("HOT_JIT_THRESHOLD", "1")]),
];

#[tokio::test(flavor = "multi_thread")]
async fn sentinels_reach_no_column_of_any_table_on_sqlite() {
    let base = serve();
    let needles = needles();
    for (engine, envs) in ENGINES {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("masking.db");
        let uri = format!("sqlite:{}", database.display());
        write_project(dir.path(), &base, &uri);
        let path = dir.path().to_path_buf();
        let envs: Vec<(&str, &str)> = envs.to_vec();
        let what = format!("sqlite, {engine}");
        let what_run = what.clone();
        tokio::task::spawn_blocking(move || run_program(&path, &envs, &what_run))
            .await
            .unwrap();
        let (found, calls) = scan_sqlite(&uri, &needles).await;
        assert_nothing_found(&found, &what);
        assert_records_are_useful(&calls, &what);
    }
}

/// The same program through the same engine and emitter, under a persisted
/// org and environment, which PostgreSQL requires of every write. `hot run`
/// can't do this without an API server to resolve them.
fn run_program_in_process(
    dir: &Path,
    db: &hot::db::DatabasePool,
    emitter: std::sync::Arc<dyn hot::lang::emitter::EngineEventEmitter>,
    fixture: &hot::db::TestData,
    base: &str,
    jit: &str,
    entry: &str,
) -> Result<hot::val::Val, String> {
    use hot::lang::engine::{Engine, PipelineMode};
    let entry_file = dir.join("run.hot");
    std::fs::write(&entry_file, format!("::run ns\n{entry}\n")).unwrap();
    let context = hot::lang::event::ExecutionContext::new(
        uuid::Uuid::now_v7(),
        fixture.stream_id,
        hot::db::run::RunType::Run.as_id(),
        Some(fixture.env_id),
        Some(fixture.user_id),
        Some(fixture.org_id),
        Some(fixture.build_id),
    );
    let mut storage = ahash::AHashMap::new();
    storage.insert("t.declared".to_string(), hot::val::Val::from(DECLARED));
    storage.insert("t.base".to_string(), hot::val::Val::from(base));
    let conf = hot::val!({"jit": jit});
    let src = dir.join("src").display().to_string();
    let target = entry_file.display().to_string();
    let db = std::sync::Arc::new(db.clone());
    let handle = tokio::runtime::Handle::current();
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let _runtime = handle.enter();
                Engine::run_unified_pipeline(
                    std::slice::from_ref(&src),
                    &[],
                    Some(&conf),
                    Some("masking"),
                    Some(&target),
                    None,
                    PipelineMode::Execute,
                    Some(emitter),
                    Some(context),
                    None,
                    Some(storage),
                    Some(db),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                )
            })
            .unwrap()
            .join()
            .unwrap()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn sentinels_reach_no_column_of_any_table_on_postgres() {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL secret masking test: HOT_TEST_POSTGRES_URI not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test here that reads or writes the environment
    // in this process: the others hand it to the processes they start.
    unsafe {
        std::env::set_var("HOT_HOME", hot_home(dir.path()));
    }
    // Its own database, so that the scan sees only what this test recorded.
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_uri)
        .await
        .expect("connect to the fixture database");
    let name = format!("hot_secret_masking_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create the test database");
    let (server, _) = admin_uri.rsplit_once('/').expect("a database in the URI");
    let uri = format!("{server}/{name}");
    let conf = hot::val!({"uri": uri.clone(), "rls": {"enforce": false}});
    hot::db::run_migrations(&conf).await.expect("migrate");
    let db = hot::db::create_db_pool(&conf).await.expect("connect");
    let fixture = hot::db::insert_test_data(&db).await.expect("fixture");

    let base = serve();
    let needles = needles();
    write_project(dir.path(), &base, &uri);
    std::fs::rename(
        dir.path().join("ctx.hot"),
        dir.path().join("src/ctxfile.hot"),
    )
    .unwrap();
    for jit in ["off", "on"] {
        let what = format!("postgres, jit {jit}");
        let emitter = std::sync::Arc::new(
            hot::lang::emitter::database::DatabaseEngineEventEmitter::new_with_pool(db.clone()),
        );
        let path = dir.path().to_path_buf();
        let (run_db, run_fixture, run_base) = (db.clone(), fixture.clone(), base.clone());
        let run_emitter = emitter.clone();
        let (succeeded, failed, returned) = tokio::task::spawn_blocking(move || {
            let run = |entry: &str| {
                run_program_in_process(
                    &path,
                    &run_db,
                    run_emitter.clone(),
                    &run_fixture,
                    &run_base,
                    jit,
                    entry,
                )
            };
            (
                run("::secrets/all()"),
                run("::secrets/fails()"),
                RETURNS_ERR
                    .iter()
                    .map(|entry| run(entry))
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .unwrap();
        succeeded.unwrap_or_else(|error| panic!("{what}: {error}"));
        assert!(failed.is_err(), "{what}: the program should fail");
        for result in returned {
            let result = result.unwrap_or_else(|error| panic!("{what}: {error}"));
            assert!(result.is_err(), "{what}: should end in an Err: {result}");
        }
        hot::lang::emitter::EngineEventEmitter::shutdown(&*emitter)
            .await
            .unwrap();

        let (found, calls) = scan_postgres(&uri, "hot", &needles).await;
        assert_nothing_found(&found, &what);
        assert_records_are_useful(&calls, &what);
    }

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("drop the test database");
}

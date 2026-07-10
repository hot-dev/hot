// SQLite natives (::hot::sqlite) over libsqlite3-sys.
//
// file.mode aware, mirroring ::hot::file:
// - "direct": open the host path with sqlite3_open_v2 (default for CLI).
// - "service": check the database file out of FileStorage into a local
//   scratch copy, run SQLite against the copy, and commit the bytes back
//   on close/sync. Commits go through FileStorage::write_file_if — an
//   atomic compare-and-swap on the file record's etag — so a concurrent
//   commit loses cleanly with a conflict error instead of clobbering.
//
// Connections are Val::Box handles (like ::hot::tcp): an Arc'd inner with
// a mutex-serialized raw sqlite3 pointer. Connections are opened with
// SQLITE_OPEN_FULLMUTEX, and every use goes through the state mutex, so a
// handle crossing VM/task boundaries is safe (serialized, never faster).

use crate::file_storage::compute_md5;
use crate::lang::hot::file::{get_file_context, get_file_mode};
use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::validate_args;
use libsqlite3_sys as ffi;
use std::any::Any;
use std::ffi::{CStr, CString};
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Helper to run async FileStorage operations synchronously from the VM.
/// Same rationale as file.rs: the VM runs in spawn_blocking context.
fn block_on_storage<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Handle::current().block_on(future)
}

// ----------------------------------------------------------------------------
// Connection state
// ----------------------------------------------------------------------------

/// Raw sqlite3 pointer wrapper. Send is sound because all access is
/// serialized through SqliteConnInner.state's mutex and the database is
/// opened with SQLITE_OPEN_FULLMUTEX (serialized threading mode).
struct DbPtr(*mut ffi::sqlite3);
unsafe impl Send for DbPtr {}

/// Service-mode checkout bookkeeping.
struct ServiceCheckout {
    /// Path within FileStorage (what the caller passed to open).
    storage_path: String,
    /// Record etag at checkout (None when the file did not exist yet —
    /// the first commit creates it, racing on the unique index).
    checkout_etag: Option<String>,
    read_only: bool,
}

struct SqliteConnState {
    /// None after close.
    db: Option<DbPtr>,
    /// Local filesystem path SQLite is actually operating on. In direct
    /// mode this is the caller's path; in service mode it is the scratch
    /// copy (removed on close).
    local_path: PathBuf,
    service: Option<ServiceCheckout>,
}

pub(crate) struct SqliteConnInner {
    id: String,
    display_path: String,
    state: Mutex<SqliteConnState>,
}

pub struct SqliteConnectionHandle {
    pub(crate) inner: Arc<SqliteConnInner>,
}

impl std::fmt::Debug for SqliteConnectionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SqliteConnection<{}>", self.inner.id)
    }
}

impl crate::val::ValBox for SqliteConnectionHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn clone_box(&self) -> Box<dyn crate::val::ValBox> {
        Box::new(SqliteConnectionHandle {
            inner: Arc::clone(&self.inner),
        })
    }

    fn equals(&self, other: &dyn crate::val::ValBox) -> bool {
        other
            .as_any()
            .downcast_ref::<SqliteConnectionHandle>()
            .is_some_and(|o| Arc::ptr_eq(&self.inner, &o.inner))
    }

    fn hash(&self, _state: &mut dyn Hasher) {}

    fn to_string(&self) -> String {
        format!("SqliteConnection<{}>", self.inner.display_path)
    }

    fn compare(&self, _other: &dyn crate::val::ValBox) -> Option<std::cmp::Ordering> {
        None
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "$type": "SqliteConnection",
            "id": self.inner.id,
            "path": self.inner.display_path
        }))
    }

    fn type_name(&self) -> &'static str {
        "SqliteConnection"
    }
}

// ----------------------------------------------------------------------------
// FFI helpers
// ----------------------------------------------------------------------------

fn last_errmsg(db: *mut ffi::sqlite3) -> String {
    unsafe {
        let msg = ffi::sqlite3_errmsg(db);
        if msg.is_null() {
            "unknown sqlite error".to_string()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        }
    }
}

fn open_db(path: &std::path::Path, read_only: bool) -> Result<DbPtr, String> {
    let c_path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| "path contains a NUL byte".to_string())?;
    let flags = if read_only {
        ffi::SQLITE_OPEN_READONLY | ffi::SQLITE_OPEN_FULLMUTEX
    } else {
        ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_FULLMUTEX
    };
    let mut db: *mut ffi::sqlite3 = std::ptr::null_mut();
    let rc = unsafe { ffi::sqlite3_open_v2(c_path.as_ptr(), &mut db, flags, std::ptr::null()) };
    if rc != ffi::SQLITE_OK {
        let msg = if db.is_null() {
            format!("sqlite open failed (code {})", rc)
        } else {
            let m = last_errmsg(db);
            unsafe { ffi::sqlite3_close(db) };
            m
        };
        return Err(msg);
    }
    // Sane defaults: foreign keys on, 5s busy timeout for co-located writers.
    unsafe {
        let pragma = CString::new("PRAGMA foreign_keys = ON;").unwrap();
        ffi::sqlite3_exec(
            db,
            pragma.as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        ffi::sqlite3_busy_timeout(db, 5000);
    }
    Ok(DbPtr(db))
}

fn bind_param(
    db: *mut ffi::sqlite3,
    stmt: *mut ffi::sqlite3_stmt,
    index: i32,
    value: &Val,
) -> Result<(), String> {
    let rc = unsafe {
        match value {
            Val::Null => ffi::sqlite3_bind_null(stmt, index),
            Val::Int(i) => ffi::sqlite3_bind_int64(stmt, index, *i),
            Val::Byte(b) => ffi::sqlite3_bind_int64(stmt, index, *b as i64),
            Val::Bool(b) => ffi::sqlite3_bind_int64(stmt, index, if *b { 1 } else { 0 }),
            Val::Dec(d) => ffi::sqlite3_bind_double(stmt, index, d.to_f64()),
            Val::Str(s) => ffi::sqlite3_bind_text(
                stmt,
                index,
                s.as_ptr() as *const std::os::raw::c_char,
                s.len() as std::os::raw::c_int,
                ffi::SQLITE_TRANSIENT(),
            ),
            Val::Bytes(b) => ffi::sqlite3_bind_blob(
                stmt,
                index,
                b.as_ptr() as *const std::os::raw::c_void,
                b.len() as std::os::raw::c_int,
                ffi::SQLITE_TRANSIENT(),
            ),
            other => {
                return Err(format!(
                    "parameter {} has unsupported type {:?} (bind Int, Dec, Str, Bytes, Bool, or null)",
                    index,
                    std::mem::discriminant(other)
                ));
            }
        }
    };
    if rc != ffi::SQLITE_OK {
        return Err(format!("bind {} failed: {}", index, last_errmsg(db)));
    }
    Ok(())
}

fn column_value(stmt: *mut ffi::sqlite3_stmt, col: i32) -> Val {
    unsafe {
        match ffi::sqlite3_column_type(stmt, col) {
            ffi::SQLITE_INTEGER => Val::Int(ffi::sqlite3_column_int64(stmt, col)),
            ffi::SQLITE_FLOAT => Val::from(ffi::sqlite3_column_double(stmt, col)),
            ffi::SQLITE_TEXT => {
                let ptr = ffi::sqlite3_column_text(stmt, col);
                let len = ffi::sqlite3_column_bytes(stmt, col) as usize;
                if ptr.is_null() {
                    Val::Null
                } else {
                    let bytes = std::slice::from_raw_parts(ptr, len);
                    Val::from(String::from_utf8_lossy(bytes).into_owned())
                }
            }
            ffi::SQLITE_BLOB => {
                let ptr = ffi::sqlite3_column_blob(stmt, col);
                let len = ffi::sqlite3_column_bytes(stmt, col) as usize;
                if ptr.is_null() || len == 0 {
                    Val::Bytes(Vec::new())
                } else {
                    Val::Bytes(std::slice::from_raw_parts(ptr as *const u8, len).to_vec())
                }
            }
            _ => Val::Null,
        }
    }
}

/// Prepare a single statement; reject trailing non-whitespace (one statement
/// per call keeps parameter binding unambiguous).
struct Stmt {
    db: *mut ffi::sqlite3,
    ptr: *mut ffi::sqlite3_stmt,
}

impl Drop for Stmt {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::sqlite3_finalize(self.ptr) };
        }
    }
}

fn prepare(db: *mut ffi::sqlite3, sql: &str) -> Result<Stmt, String> {
    let c_sql = CString::new(sql).map_err(|_| "sql contains a NUL byte".to_string())?;
    let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
    let mut tail: *const std::os::raw::c_char = std::ptr::null();
    let rc = unsafe {
        ffi::sqlite3_prepare_v2(
            db,
            c_sql.as_ptr(),
            sql.len() as std::os::raw::c_int + 1,
            &mut stmt,
            &mut tail,
        )
    };
    if rc != ffi::SQLITE_OK {
        return Err(last_errmsg(db));
    }
    if stmt.is_null() {
        return Err("sql contains no statement".to_string());
    }
    if !tail.is_null() {
        let rest = unsafe { CStr::from_ptr(tail) }.to_string_lossy();
        if !rest.trim().trim_matches(';').trim().is_empty() {
            unsafe { ffi::sqlite3_finalize(stmt) };
            return Err(
                "one statement per call (found trailing SQL after the first statement)".to_string(),
            );
        }
    }
    Ok(Stmt { db, ptr: stmt })
}

fn bind_all(stmt: &Stmt, params: &[Val]) -> Result<(), String> {
    let expected = unsafe { ffi::sqlite3_bind_parameter_count(stmt.ptr) };
    if expected as usize != params.len() {
        return Err(format!(
            "statement expects {} parameters, got {}",
            expected,
            params.len()
        ));
    }
    for (i, value) in params.iter().enumerate() {
        bind_param(stmt.db, stmt.ptr, (i + 1) as i32, value)?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Handle helpers
// ----------------------------------------------------------------------------

fn get_handle<'a>(args: &'a [Val], fn_name: &str) -> Result<&'a SqliteConnectionHandle, String> {
    match &args[0] {
        Val::Box(b) => b
            .as_any()
            .downcast_ref::<SqliteConnectionHandle>()
            .ok_or_else(|| format!("{}: first argument is not a SqliteConnection", fn_name)),
        _ => Err(format!(
            "{}: first argument must be a SqliteConnection from ::hot::sqlite/open",
            fn_name
        )),
    }
}

fn scratch_path(id: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("hot-sqlite");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}.db", id))
}

// ----------------------------------------------------------------------------
// Natives
// ----------------------------------------------------------------------------

/// ::hot::sqlite/open — open a database.
/// Args: options Map {path: Str, read-only: Bool?} or a bare path Str.
pub fn open(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::sqlite/open", args, 1);

    let (path, read_only) = match &args[0] {
        Val::Str(s) => (s.to_string(), false),
        Val::Map(m) => {
            let path = match m.get(&Val::from("path")) {
                Some(Val::Str(s)) => s.to_string(),
                _ => {
                    return HotResult::Err(Val::from(
                        "::hot::sqlite/open: options must include a `path` string".to_string(),
                    ));
                }
            };
            let read_only = matches!(m.get(&Val::from("read-only")), Some(Val::Bool(true)));
            (path, read_only)
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::sqlite/open: pass a path string or an options map".to_string(),
            ));
        }
    };

    let id = uuid::Uuid::now_v7().to_string();
    let mode = get_file_mode(vm);

    let (local_path, service) = if mode == "direct" {
        (PathBuf::from(&path), None)
    } else {
        // Service mode: check the file out of FileStorage into a scratch copy.
        let file_storage = match vm.get_file_storage() {
            Some(fs) => fs,
            None => {
                return HotResult::Err(Val::from(
                    "::hot::sqlite/open: file storage not configured".to_string(),
                ));
            }
        };
        let ctx = match get_file_context(vm) {
            Ok(c) => c,
            Err(e) => return HotResult::Err(Val::from(format!("::hot::sqlite/open: {}", e))),
        };

        let exists = match block_on_storage(file_storage.file_exists(&path, &ctx)) {
            Ok(b) => b,
            Err(e) => return HotResult::Err(Val::from(format!("::hot::sqlite/open: {}", e))),
        };

        let scratch = scratch_path(&id);
        let checkout_etag = if exists {
            let bytes = match block_on_storage(file_storage.read_file(&path, &ctx)) {
                Ok(b) => b,
                Err(e) => return HotResult::Err(Val::from(format!("::hot::sqlite/open: {}", e))),
            };
            let meta = match block_on_storage(file_storage.get_file_metadata(&path, &ctx)) {
                Ok(m) => m,
                Err(e) => return HotResult::Err(Val::from(format!("::hot::sqlite/open: {}", e))),
            };
            if let Err(e) = std::fs::write(&scratch, &bytes) {
                return HotResult::Err(Val::from(format!(
                    "::hot::sqlite/open: scratch write failed: {}",
                    e
                )));
            }
            // Legacy records without an etag can't CAS; hash what we read so
            // the commit still has an expectation to compare against skips.
            Some(meta.etag.unwrap_or_else(|| compute_md5(&bytes)))
        } else if read_only {
            return HotResult::Err(Val::from(format!(
                "::hot::sqlite/open: '{}' does not exist (read-only open)",
                path
            )));
        } else {
            None
        };

        (
            scratch,
            Some(ServiceCheckout {
                storage_path: path.clone(),
                checkout_etag,
                read_only,
            }),
        )
    };

    let db = match open_db(&local_path, read_only) {
        Ok(db) => db,
        Err(e) => {
            if service.is_some() {
                let _ = std::fs::remove_file(&local_path);
            }
            return HotResult::Err(Val::from(format!("::hot::sqlite/open: {}", e)));
        }
    };

    let handle = SqliteConnectionHandle {
        inner: Arc::new(SqliteConnInner {
            id,
            display_path: path,
            state: Mutex::new(SqliteConnState {
                db: Some(db),
                local_path,
                service,
            }),
        }),
    };
    HotResult::Ok(Val::Box(Box::new(handle)))
}

/// ::hot::sqlite/execute — run a statement, return {rows-affected, last-insert-id}.
/// Args: (conn, sql: Str, params: Vec)
pub fn execute(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::sqlite/execute", args, 3);
    match run(args, "::hot::sqlite/execute", RunMode::Execute) {
        Ok(v) => HotResult::Ok(v),
        Err(e) => HotResult::Err(Val::from(format!("::hot::sqlite/execute: {}", e))),
    }
}

/// ::hot::sqlite/query — run a query, return a Vec of row Maps.
/// Args: (conn, sql: Str, params: Vec)
pub fn query(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::sqlite/query", args, 3);
    match run(args, "::hot::sqlite/query", RunMode::Query) {
        Ok(v) => HotResult::Ok(v),
        Err(e) => HotResult::Err(Val::from(format!("::hot::sqlite/query: {}", e))),
    }
}

enum RunMode {
    Execute,
    Query,
}

fn run(args: &[Val], fn_name: &str, mode: RunMode) -> Result<Val, String> {
    let handle = get_handle(args, fn_name)?;
    let sql = match &args[1] {
        Val::Str(s) => s.to_string(),
        _ => return Err("sql must be a string".to_string()),
    };
    let params = match &args[2] {
        Val::Vec(v) => v.clone(),
        Val::Null => Vec::new(),
        _ => return Err("params must be a Vec".to_string()),
    };

    let state = handle
        .inner
        .state
        .lock()
        .map_err(|_| "connection poisoned")?;
    let db = state.db.as_ref().ok_or("connection is closed")?.0;

    let stmt = prepare(db, &sql)?;
    bind_all(&stmt, &params)?;

    match mode {
        RunMode::Execute => loop {
            match unsafe { ffi::sqlite3_step(stmt.ptr) } {
                ffi::SQLITE_ROW => continue,
                ffi::SQLITE_DONE => {
                    let changes = unsafe { ffi::sqlite3_changes64(db) };
                    let last_id = unsafe { ffi::sqlite3_last_insert_rowid(db) };
                    let mut out = indexmap::IndexMap::new();
                    out.insert(Val::from("rows-affected"), Val::Int(changes));
                    out.insert(Val::from("last-insert-id"), Val::Int(last_id));
                    return Ok(Val::Map(Box::new(out)));
                }
                _ => return Err(last_errmsg(db)),
            }
        },
        RunMode::Query => {
            let col_count = unsafe { ffi::sqlite3_column_count(stmt.ptr) };
            let mut names: Vec<Val> = Vec::with_capacity(col_count as usize);
            for c in 0..col_count {
                let name = unsafe {
                    let ptr = ffi::sqlite3_column_name(stmt.ptr, c);
                    if ptr.is_null() {
                        format!("column{}", c)
                    } else {
                        CStr::from_ptr(ptr).to_string_lossy().into_owned()
                    }
                };
                names.push(Val::from(name));
            }

            let mut rows: Vec<Val> = Vec::new();
            loop {
                match unsafe { ffi::sqlite3_step(stmt.ptr) } {
                    ffi::SQLITE_ROW => {
                        let mut row = indexmap::IndexMap::new();
                        for c in 0..col_count {
                            row.insert(names[c as usize].clone(), column_value(stmt.ptr, c));
                        }
                        rows.push(Val::Map(Box::new(row)));
                    }
                    ffi::SQLITE_DONE => return Ok(Val::Vec(rows)),
                    _ => return Err(last_errmsg(db)),
                }
            }
        }
    }
}

/// Commit the scratch copy back to FileStorage (service mode).
/// Compare-before-write: refuses to overwrite when the remote content no
/// longer matches the checkout hash.
fn commit_service(
    vm: &mut VirtualMachine,
    state: &mut SqliteConnState,
    fn_name: &str,
) -> Result<(), String> {
    let Some(service) = state.service.as_mut() else {
        return Ok(()); // direct mode: nothing to commit
    };
    if service.read_only {
        return Ok(());
    }

    let file_storage = vm.get_file_storage().ok_or("file storage not configured")?;
    let ctx = get_file_context(vm)?;

    // Make sure everything SQLite buffered is in the main db file.
    if let Some(db) = state.db.as_ref() {
        let pragma = CString::new("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        unsafe {
            ffi::sqlite3_exec(
                db.0,
                pragma.as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }

    let local_bytes = std::fs::read(&state.local_path)
        .map_err(|e| format!("{}: scratch read failed: {}", fn_name, e))?;
    let local_etag = compute_md5(&local_bytes);

    // Skip the upload when nothing changed since checkout.
    if service.checkout_etag.as_deref() == Some(local_etag.as_str()) {
        return Ok(());
    }

    // Atomic compare-and-swap on the record etag: exactly one concurrent
    // committer wins; losers get a conflict Err and never touch the blob.
    let committed = block_on_storage(file_storage.write_file_if(
        &service.storage_path,
        &local_bytes,
        None,
        service.checkout_etag.as_deref(),
        &ctx,
    ))?;
    match committed {
        Some(meta) => {
            service.checkout_etag = Some(meta.etag.unwrap_or(local_etag));
            Ok(())
        }
        None => Err(format!(
            "{}: conflict — '{}' changed since checkout; reopen and retry",
            fn_name, service.storage_path
        )),
    }
}

/// ::hot::sqlite/sync — commit the service-mode checkout without closing.
/// No-op in direct mode. Args: (conn)
pub fn sync(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::sqlite/sync", args, 1);
    let handle = match get_handle(args, "::hot::sqlite/sync") {
        Ok(h) => h,
        Err(e) => return HotResult::Err(Val::from(e)),
    };
    let inner = Arc::clone(&handle.inner);
    let mut state = match inner.state.lock() {
        Ok(s) => s,
        Err(_) => return HotResult::Err(Val::from("::hot::sqlite/sync: connection poisoned")),
    };
    if state.db.is_none() {
        return HotResult::Err(Val::from("::hot::sqlite/sync: connection is closed"));
    }
    match commit_service(vm, &mut state, "::hot::sqlite/sync") {
        Ok(()) => HotResult::Ok(Val::Bool(true)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// ::hot::sqlite/close — close the connection; in service mode, commit the
/// checkout and remove the scratch copy. Args: (conn)
pub fn close(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::sqlite/close", args, 1);
    let handle = match get_handle(args, "::hot::sqlite/close") {
        Ok(h) => h,
        Err(e) => return HotResult::Err(Val::from(e)),
    };
    let inner = Arc::clone(&handle.inner);
    let mut state = match inner.state.lock() {
        Ok(s) => s,
        Err(_) => return HotResult::Err(Val::from("::hot::sqlite/close: connection poisoned")),
    };
    if state.db.is_none() {
        return HotResult::Ok(Val::Bool(true)); // idempotent close
    }

    // Commit BEFORE closing so a conflict leaves the connection usable
    // (the caller can inspect, copy data out, or force via sync-after-reopen).
    if let Err(e) = commit_service(vm, &mut state, "::hot::sqlite/close") {
        return HotResult::Err(Val::from(e));
    }

    if let Some(db) = state.db.take() {
        unsafe { ffi::sqlite3_close(db.0) };
    }
    if state.service.is_some() {
        let _ = std::fs::remove_file(&state.local_path);
    }
    HotResult::Ok(Val::Bool(true))
}

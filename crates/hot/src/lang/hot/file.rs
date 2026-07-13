// File storage functions for bytecode engine
//
// Supports two modes:
// - "direct": Simple filesystem operations without database (default for CLI)
// - "service": Managed storage with database tracking, org isolation (default for projects)

use crate::file_storage::{FileRunProvenance, FileStorage, FileStorageContext};
use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;
use std::path::Path;
use std::sync::Arc;

/// Get the file mode from VM configuration
/// Returns "direct" (default) or "service"
pub(crate) fn get_file_mode(vm: &VirtualMachine) -> String {
    if let Some(val) = vm.get_conf().get("file.mode") {
        match val {
            Val::Str(s) => {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
            _ => {
                // Convert other types to string representation
                let s = val.to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    "direct".to_string()
}

/// Helper to run async file operations synchronously from the VM
/// This blocks the current thread until the async operation completes.
/// Uses Handle::block_on directly since VM runs in spawn_blocking context
/// (block_in_place panics from spawn_blocking).
fn block_on_file_op<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Handle::current().block_on(future)
}

/// Helper to get file storage context from VM (for service mode)
pub(crate) fn get_file_context(vm: &VirtualMachine) -> Result<FileStorageContext, String> {
    let db = vm
        .get_database_pool()
        .ok_or("Database pool not available for file operations")?;

    let exec_ctx = vm
        .get_execution_context()
        .as_ref()
        .ok_or("Execution context not available for file operations")?;

    let org_id = exec_ctx
        .org_id
        .ok_or("Organization ID not available in execution context")?;

    let user_id = exec_ctx
        .user_id
        .ok_or("User ID not available in execution context")?;

    // Only pre-insert placeholder run rows when an emitter will later deliver
    // the authoritative run:start/run:stop. Without an emitter the placeholder
    // would sit as a zombie "running" run forever; the FK-retry fallback in
    // insert_file_record (NULL run id) covers that case instead.
    let run_provenance = vm.get_emitter().as_ref().map(|_| FileRunProvenance {
        stream_id: exec_ctx.stream_id,
        build_id: exec_ctx.build_id,
        run_type_id: exec_ctx.run_type_id,
        access_id: exec_ctx.access_id,
    });

    Ok(FileStorageContext {
        db,
        org_id,
        env_id: exec_ctx.env_id,
        user_id,
        run_id: Some(exec_ctx.run_id),
        run_provenance,
        file_max_bytes_conf: FileStorageContext::conf_file_max_bytes(vm.get_conf()),
    })
}

// ============================================================================
// Direct Mode Helpers
// ============================================================================

/// Normalize path for direct mode - prevent directory traversal
fn normalize_direct_path(path: &str) -> Result<std::path::PathBuf, String> {
    let path = Path::new(path);

    // Resolve the path components to prevent traversal
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(c) => normalized.push(c),
            std::path::Component::CurDir => {} // Skip "."
            std::path::Component::ParentDir => {
                // Allow ".." but track it
                normalized.push("..");
            }
            std::path::Component::RootDir => {
                normalized.push("/");
            }
            std::path::Component::Prefix(p) => {
                normalized.push(p.as_os_str());
            }
        }
    }

    Ok(normalized)
}

/// Infer content type from file extension using mime_guess
fn infer_content_type(path: &str) -> Option<String> {
    // First check for custom extensions not in mime_guess
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    if let Some(ref ext) = ext {
        // Handle custom extensions not in mime_guess
        if ext.as_str() == "hot" {
            return Some("text/x-hot".to_string());
        }
    }

    // Use mime_guess for standard extensions
    mime_guess::from_path(path).first().map(|m| m.to_string())
}

/// Create a FileMeta-compatible Val from filesystem metadata
fn create_direct_file_meta(path: &str, metadata: &std::fs::Metadata) -> Val {
    use std::time::UNIX_EPOCH;

    let mut map = IndexMap::new();
    map.insert(Val::from("file-id"), Val::from(path.to_string())); // Use path as ID
    map.insert(Val::from("path"), Val::from(path.to_string()));
    map.insert(Val::from("size"), Val::Int(metadata.len() as i64));

    if let Some(ct) = infer_content_type(path) {
        map.insert(Val::from("content-type"), Val::from(ct));
    }

    map.insert(Val::from("storage-backend"), Val::from("direct"));

    // Get timestamps
    let created_at = metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let updated_at = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    map.insert(Val::from("created-at"), Val::Int(created_at));
    map.insert(Val::from("updated-at"), Val::Int(updated_at));

    Val::Map(Box::new(map))
}

// ============================================================================
// Direct Mode Implementations
// ============================================================================

fn read_file_direct(path: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    match std::fs::read_to_string(&normalized) {
        Ok(contents) => HotResult::Ok(Val::from(contents)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to read file: {}", e))),
    }
}

fn read_file_bytes_direct(path: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    match std::fs::read(&normalized) {
        Ok(bytes) => HotResult::Ok(Val::Bytes(bytes)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to read file: {}", e))),
    }
}

fn write_file_direct(path: &str, contents: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    // Create parent directories if needed
    if let Some(parent) = normalized.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return HotResult::Err(Val::from(format!(
            "Failed to create parent directories: {}",
            e
        )));
    }

    match std::fs::write(&normalized, contents) {
        Ok(()) => {
            // Get metadata for return value
            match std::fs::metadata(&normalized) {
                Ok(meta) => HotResult::Ok(create_direct_file_meta(path, &meta)),
                Err(_) => {
                    // File written but can't get metadata - create minimal response
                    let mut map = IndexMap::new();
                    map.insert(Val::from("file-id"), Val::from(path.to_string()));
                    map.insert(Val::from("path"), Val::from(path.to_string()));
                    map.insert(Val::from("size"), Val::Int(contents.len() as i64));
                    map.insert(Val::from("storage-backend"), Val::from("direct"));
                    HotResult::Ok(Val::Map(Box::new(map)))
                }
            }
        }
        Err(e) => HotResult::Err(Val::from(format!("Failed to write file: {}", e))),
    }
}

fn write_file_bytes_direct(path: &str, contents: &[u8]) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    // Create parent directories if needed
    if let Some(parent) = normalized.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return HotResult::Err(Val::from(format!(
            "Failed to create parent directories: {}",
            e
        )));
    }

    match std::fs::write(&normalized, contents) {
        Ok(()) => {
            // Get metadata for return value
            match std::fs::metadata(&normalized) {
                Ok(meta) => HotResult::Ok(create_direct_file_meta(path, &meta)),
                Err(_) => {
                    // File written but can't get metadata - create minimal response
                    let mut map = IndexMap::new();
                    map.insert(Val::from("file-id"), Val::from(path.to_string()));
                    map.insert(Val::from("path"), Val::from(path.to_string()));
                    map.insert(Val::from("size"), Val::Int(contents.len() as i64));
                    map.insert(Val::from("storage-backend"), Val::from("direct"));
                    HotResult::Ok(Val::Map(Box::new(map)))
                }
            }
        }
        Err(e) => HotResult::Err(Val::from(format!("Failed to write file: {}", e))),
    }
}

fn delete_file_direct(path: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    if !normalized.exists() {
        return HotResult::Ok(Val::Bool(false));
    }

    match std::fs::remove_file(&normalized) {
        Ok(()) => HotResult::Ok(Val::Bool(true)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to delete file: {}", e))),
    }
}

fn file_exists_direct(path: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    HotResult::Ok(Val::Bool(normalized.exists() && normalized.is_file()))
}

fn file_info_direct(path: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(path) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    match std::fs::metadata(&normalized) {
        Ok(meta) => HotResult::Ok(create_direct_file_meta(path, &meta)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to get file info: {}", e))),
    }
}

fn list_files_direct(prefix: &str) -> HotResult<Val> {
    let normalized = match normalize_direct_path(prefix) {
        Ok(p) => p,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    // Determine the directory to search and the prefix filter
    let (search_dir, prefix_filter) = if normalized.is_dir() {
        (normalized.clone(), None)
    } else if let Some(parent) = normalized.parent() {
        let file_prefix = normalized
            .file_name()
            .map(|s| s.to_string_lossy().to_string());
        (parent.to_path_buf(), file_prefix)
    } else {
        (std::path::PathBuf::from("."), Some(prefix.to_string()))
    };

    if !search_dir.exists() {
        return HotResult::Ok(Val::Vec(vec![]));
    }

    let mut files = Vec::new();

    fn collect_files(
        dir: &Path,
        base: &Path,
        prefix_filter: &Option<String>,
        files: &mut Vec<Val>,
    ) -> Result<(), std::io::Error> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                // Apply prefix filter if present
                if let Some(filter) = prefix_filter
                    && !relative.starts_with(filter)
                {
                    continue;
                }

                if let Ok(meta) = std::fs::metadata(&path) {
                    files.push(create_direct_file_meta(&relative, &meta));
                }
            } else if path.is_dir() {
                collect_files(&path, base, prefix_filter, files)?;
            }
        }
        Ok(())
    }

    if let Err(e) = collect_files(&search_dir, &search_dir, &prefix_filter, &mut files) {
        return HotResult::Err(Val::from(format!("Failed to list files: {}", e)));
    }

    HotResult::Ok(Val::Vec(files))
}

// ============================================================================
// VM-Aware Wrappers
// ============================================================================

/// Read file contents as string (VM-aware wrapper)
pub fn read_file(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/read-file", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    let mode = get_file_mode(vm);
    if mode == "direct" {
        return read_file_direct(path);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    read_file_internal(args, &file_storage, &ctx)
}

/// Read file contents as bytes (VM-aware wrapper)
pub fn read_file_bytes(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/read-file-bytes", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    let mode = get_file_mode(vm);
    if mode == "direct" {
        return read_file_bytes_direct(path);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    read_file_bytes_internal(args, &file_storage, &ctx)
}

/// Write file contents from string (VM-aware wrapper)
pub fn write_file(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/write-file", args, 2);

    let mode = get_file_mode(vm);
    if mode == "direct" {
        let (path, content) = match (&args[0], &args[1]) {
            (Val::Str(p), Val::Str(c)) => (&**p, &**c),
            _ => return HotResult::Err(Val::from("Path and content must be strings")),
        };
        return write_file_direct(path, content);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    write_file_internal(args, &file_storage, &ctx)
}

/// Write file contents from bytes (VM-aware wrapper)
pub fn write_file_bytes(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/write-file-bytes", args, 2);

    let mode = get_file_mode(vm);
    if mode == "direct" {
        let (path, content) = match (&args[0], &args[1]) {
            (Val::Str(p), Val::Bytes(b)) => (&**p, b.as_slice()),
            _ => {
                return HotResult::Err(Val::from(
                    "Path must be string, content must be bytes".to_string(),
                ));
            }
        };
        return write_file_bytes_direct(path, content);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    write_file_bytes_internal(args, &file_storage, &ctx)
}

/// Delete a file (VM-aware wrapper)
pub fn delete_file(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/delete-file", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    let mode = get_file_mode(vm);
    if mode == "direct" {
        return delete_file_direct(path);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    delete_file_internal(args, &file_storage, &ctx)
}

/// Check if a file exists (VM-aware wrapper)
pub fn file_exists(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/file-exists", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    let mode = get_file_mode(vm);
    if mode == "direct" {
        return file_exists_direct(path);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    file_exists_internal(args, &file_storage, &ctx)
}

/// Get file metadata (VM-aware wrapper)
pub fn file_info(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/file-info", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    let mode = get_file_mode(vm);
    if mode == "direct" {
        return file_info_direct(path);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    file_info_internal(args, &file_storage, &ctx)
}

/// List files by prefix (VM-aware wrapper)
pub fn list_files(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::file/list-files", args, 1);

    let prefix = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Prefix must be a string")),
    };

    let mode = get_file_mode(vm);
    if mode == "direct" {
        return list_files_direct(prefix);
    }

    // Service mode
    let file_storage = match vm.get_file_storage() {
        Some(fs) => fs,
        None => return HotResult::Err(Val::from("File storage not configured")),
    };

    let ctx = match get_file_context(vm) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    list_files_internal(args, &file_storage, &ctx)
}

// ============================================================================
// Service Mode Implementation Functions
// ============================================================================

/// Read file contents as string (service mode)
fn read_file_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/read-file", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    match block_on_file_op(file_storage.read_file(path, ctx)) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => HotResult::Ok(Val::from(s)),
            Err(e) => HotResult::Err(Val::from(format!("File is not valid UTF-8: {}", e))),
        },
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Read file contents as bytes (service mode)
fn read_file_bytes_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/read-file-bytes", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    match block_on_file_op(file_storage.read_file(path, ctx)) {
        Ok(bytes) => HotResult::Ok(Val::Bytes(bytes)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Write file contents from string (service mode)
fn write_file_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/write-file", args, 2);

    let (path, content) = match (&args[0], &args[1]) {
        (Val::Str(p), Val::Str(c)) => (&**p, &**c),
        _ => return HotResult::Err(Val::from("Path and content must be strings")),
    };

    match block_on_file_op(file_storage.write_file(path, content.as_bytes(), None, ctx)) {
        Ok(metadata) => HotResult::Ok(metadata.to_val()),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Write file contents from bytes (service mode)
fn write_file_bytes_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/write-file-bytes", args, 2);

    let (path, content) = match (&args[0], &args[1]) {
        (Val::Str(p), Val::Bytes(b)) => (&**p, b.as_slice()),
        _ => {
            return HotResult::Err(Val::from(
                "Path must be string, content must be bytes".to_string(),
            ));
        }
    };

    match block_on_file_op(file_storage.write_file(path, content, None, ctx)) {
        Ok(metadata) => HotResult::Ok(metadata.to_val()),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Delete a file (service mode)
fn delete_file_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/delete-file", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    match block_on_file_op(file_storage.delete_file(path, ctx)) {
        Ok(existed) => HotResult::Ok(Val::Bool(existed)), // Return whether file existed
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Check if a file exists (service mode)
fn file_exists_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/file-exists", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    match block_on_file_op(file_storage.file_exists(path, ctx)) {
        Ok(exists) => HotResult::Ok(Val::Bool(exists)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Get file metadata (service mode)
fn file_info_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/file-info", args, 1);

    let path = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Path must be a string")),
    };

    match block_on_file_op(file_storage.get_file_metadata(path, ctx)) {
        Ok(metadata) => HotResult::Ok(metadata.to_val()),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// List files by prefix (service mode)
fn list_files_internal(
    args: &[Val],
    file_storage: &Arc<dyn FileStorage>,
    ctx: &FileStorageContext,
) -> HotResult<Val> {
    validate_args!("::hot::file/list-files", args, 1);

    let prefix = match &args[0] {
        Val::Str(s) => &**s,
        _ => return HotResult::Err(Val::from("Prefix must be a string")),
    };

    match block_on_file_op(file_storage.list_files(prefix, ctx)) {
        Ok(files) => {
            let vals: Vec<Val> = files.iter().map(|f| f.to_val()).collect();
            HotResult::Ok(Val::Vec(vals))
        }
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ========================================================================
    // Path Normalization Tests
    // ========================================================================

    #[test]
    fn test_normalize_direct_path_simple() {
        let result = normalize_direct_path("foo/bar.txt").unwrap();
        assert_eq!(result.to_string_lossy(), "foo/bar.txt");
    }

    #[test]
    fn test_normalize_direct_path_with_dot() {
        let result = normalize_direct_path("./foo/bar.txt").unwrap();
        assert_eq!(result.to_string_lossy(), "foo/bar.txt");
    }

    #[test]
    fn test_normalize_direct_path_with_parent() {
        let result = normalize_direct_path("foo/../bar.txt").unwrap();
        // Parent dir is allowed in direct mode
        assert!(result.to_string_lossy().contains("bar.txt"));
    }

    #[test]
    fn test_normalize_direct_path_absolute() {
        let result = normalize_direct_path("/tmp/test.txt").unwrap();
        assert!(result.to_string_lossy().contains("tmp"));
    }

    // ========================================================================
    // Content Type Inference Tests
    // ========================================================================

    #[test]
    fn test_infer_content_type_common_extensions() {
        assert_eq!(
            infer_content_type("file.txt"),
            Some("text/plain".to_string())
        );
        assert_eq!(
            infer_content_type("file.json"),
            Some("application/json".to_string())
        );
        assert_eq!(
            infer_content_type("file.html"),
            Some("text/html".to_string())
        );
        assert_eq!(
            infer_content_type("file.png"),
            Some("image/png".to_string())
        );
        assert_eq!(
            infer_content_type("file.jpg"),
            Some("image/jpeg".to_string())
        );
        assert_eq!(infer_content_type("file.css"), Some("text/css".to_string()));
        assert_eq!(
            infer_content_type("file.js"),
            Some("text/javascript".to_string())
        );
    }

    #[test]
    fn test_infer_content_type_hot_extension() {
        // Custom extension for Hot files
        assert_eq!(
            infer_content_type("file.hot"),
            Some("text/x-hot".to_string())
        );
    }

    #[test]
    fn test_infer_content_type_no_extension() {
        assert_eq!(infer_content_type("file"), None);
    }

    #[test]
    fn test_infer_content_type_unknown_extension() {
        // mime_guess returns None for unknown extensions
        assert!(infer_content_type("file.xyz123unknown").is_none());
    }

    // ========================================================================
    // Direct Mode File Operation Tests
    // ========================================================================

    #[test]
    fn test_read_file_direct_success() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let content = "Hello, World!";
        fs::write(&file_path, content).unwrap();

        let result = read_file_direct(file_path.to_str().unwrap());
        match result {
            HotResult::Ok(val) => {
                if let Val::Str(s) = val {
                    assert_eq!(&*s, content);
                } else {
                    panic!("Expected string value");
                }
            }
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[test]
    fn test_read_file_direct_not_found() {
        let result = read_file_direct("/nonexistent/path/file.txt");
        match result {
            HotResult::Err(_) => {} // Expected
            _ => panic!("Expected error for non-existent file"),
        }
    }

    #[test]
    fn test_write_file_direct_success() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("output.txt");
        let content = "Test content";

        let result = write_file_direct(file_path.to_str().unwrap(), content);
        match result {
            HotResult::Ok(val) => {
                // Check metadata returned
                if let Val::Map(map) = val {
                    assert!(map.contains_key(&Val::from("path")));
                    assert!(map.contains_key(&Val::from("size")));
                    assert!(map.contains_key(&Val::from("storage-backend")));

                    // Verify storage-backend is "direct"
                    if let Some(backend) = map.get(&Val::from("storage-backend")) {
                        assert_eq!(backend.to_string(), "\"direct\"");
                    }
                } else {
                    panic!("Expected map value");
                }

                // Verify file was written
                let written = fs::read_to_string(&file_path).unwrap();
                assert_eq!(written, content);
            }
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[test]
    fn test_write_file_direct_creates_parent_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("subdir/nested/output.txt");
        let content = "Nested content";

        let result = write_file_direct(file_path.to_str().unwrap(), content);
        match result {
            HotResult::Ok(_) => {
                // Verify file was written
                let written = fs::read_to_string(&file_path).unwrap();
                assert_eq!(written, content);
            }
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[test]
    fn test_delete_file_direct_success() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("to_delete.txt");
        fs::write(&file_path, "delete me").unwrap();

        assert!(file_path.exists());

        let result = delete_file_direct(file_path.to_str().unwrap());
        match result {
            HotResult::Ok(Val::Bool(true)) => {
                assert!(!file_path.exists());
            }
            _ => panic!("Expected Ok(true)"),
        }
    }

    #[test]
    fn test_delete_file_direct_not_found() {
        let result = delete_file_direct("/nonexistent/file.txt");
        match result {
            HotResult::Ok(Val::Bool(false)) => {} // Expected - file didn't exist
            _ => panic!("Expected Ok(false) for non-existent file"),
        }
    }

    #[test]
    fn test_file_exists_direct() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("exists.txt");
        fs::write(&file_path, "exists").unwrap();

        // File exists
        let result = file_exists_direct(file_path.to_str().unwrap());
        match result {
            HotResult::Ok(Val::Bool(true)) => {}
            _ => panic!("Expected Ok(true) for existing file"),
        }

        // File doesn't exist
        let result = file_exists_direct("/nonexistent/file.txt");
        match result {
            HotResult::Ok(Val::Bool(false)) => {}
            _ => panic!("Expected Ok(false) for non-existent file"),
        }
    }

    #[test]
    fn test_file_info_direct() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("info.txt");
        let content = "test content for info";
        fs::write(&file_path, content).unwrap();

        let result = file_info_direct(file_path.to_str().unwrap());
        match result {
            HotResult::Ok(Val::Map(map)) => {
                // Check size
                if let Some(Val::Int(size)) = map.get(&Val::from("size")) {
                    assert_eq!(*size, content.len() as i64);
                } else {
                    panic!("Expected size field");
                }

                // Check storage-backend
                if let Some(backend) = map.get(&Val::from("storage-backend")) {
                    assert_eq!(backend.to_string(), "\"direct\"");
                } else {
                    panic!("Expected storage-backend field");
                }

                // Check content-type (should be text/plain for .txt)
                if let Some(ct) = map.get(&Val::from("content-type")) {
                    assert!(ct.to_string().contains("text/plain"));
                }
            }
            HotResult::Ok(other) => panic!("Expected Map, got {:?}", other),
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[test]
    fn test_list_files_direct() {
        let temp_dir = TempDir::new().unwrap();

        // Create some files
        fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();
        fs::write(temp_dir.path().join("subdir/c.txt"), "c").unwrap();

        let result = list_files_direct(temp_dir.path().to_str().unwrap());
        match result {
            HotResult::Ok(Val::Vec(files)) => {
                // Should find at least 3 files (a.txt, b.txt, subdir/c.txt)
                assert!(
                    files.len() >= 3,
                    "Expected at least 3 files, got {}",
                    files.len()
                );
            }
            HotResult::Ok(other) => panic!("Expected Vec, got {:?}", other),
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[test]
    fn test_read_file_bytes_direct() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("binary.bin");
        let content: Vec<u8> = vec![0x00, 0x01, 0x02, 0xFF, 0xFE];
        fs::write(&file_path, &content).unwrap();

        let result = read_file_bytes_direct(file_path.to_str().unwrap());
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, content);
            }
            HotResult::Ok(other) => panic!("Expected Bytes, got {:?}", other),
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[test]
    fn test_write_file_bytes_direct() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("binary_out.bin");
        let content: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];

        let result = write_file_bytes_direct(file_path.to_str().unwrap(), &content);
        match result {
            HotResult::Ok(Val::Map(map)) => {
                // Verify size
                if let Some(Val::Int(size)) = map.get(&Val::from("size")) {
                    assert_eq!(*size, content.len() as i64);
                }

                // Verify file was written
                let written = fs::read(&file_path).unwrap();
                assert_eq!(written, content);
            }
            HotResult::Ok(other) => panic!("Expected Map, got {:?}", other),
            HotResult::Err(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    // ========================================================================
    // FileMeta Creation Tests
    // ========================================================================

    #[test]
    fn test_create_direct_file_meta() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("meta_test.json");
        fs::write(&file_path, r#"{"key": "value"}"#).unwrap();

        let metadata = fs::metadata(&file_path).unwrap();
        let result = create_direct_file_meta("meta_test.json", &metadata);

        if let Val::Map(map) = result {
            // Check file-id equals path
            if let Some(Val::Str(id)) = map.get(&Val::from("file-id")) {
                assert_eq!(&**id, "meta_test.json");
            } else {
                panic!("Expected file-id");
            }

            // Check storage-backend
            if let Some(Val::Str(backend)) = map.get(&Val::from("storage-backend")) {
                assert_eq!(&**backend, "direct");
            } else {
                panic!("Expected storage-backend");
            }

            // Check content-type for .json
            if let Some(Val::Str(ct)) = map.get(&Val::from("content-type")) {
                assert!(ct.contains("json"));
            }
        } else {
            panic!("Expected Map");
        }
    }
}

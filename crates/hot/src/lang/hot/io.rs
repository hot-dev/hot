// IO functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::{validate_args, validate_no_args};
use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A trait for writable streams that can be captured
pub trait HotWriter: Write + Send + Sync {}

impl<T: Write + Send + Sync> HotWriter for T {}

/// A capture handle that can redirect output to a buffer
#[derive(Debug, Clone)]
pub struct CaptureBuffer {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl PartialEq for CaptureBuffer {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.buffer, &other.buffer)
    }
}

impl Eq for CaptureBuffer {}

impl std::hash::Hash for CaptureBuffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.buffer).hash(state);
    }
}

impl PartialOrd for CaptureBuffer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CaptureBuffer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        Arc::as_ptr(&self.buffer).cmp(&Arc::as_ptr(&other.buffer))
    }
}

impl serde::Serialize for CaptureBuffer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("CaptureBuffer")
    }
}

impl Default for CaptureBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureBuffer {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn get_contents(&self) -> String {
        if let Ok(buffer) = self.buffer.lock() {
            String::from_utf8_lossy(&buffer).to_string()
        } else {
            String::new()
        }
    }

    pub fn clear(&self) {
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.clear();
        }
    }
}

impl Write for CaptureBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.extend_from_slice(buf);
            Ok(buf.len())
        } else {
            Err(io::Error::other("Failed to lock buffer"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writer types for managed streams
enum WriterType {
    Stdout(BufWriter<io::Stdout>),
    Stderr(BufWriter<io::Stderr>),
    Capture(CaptureBuffer),
}

impl Write for WriterType {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            WriterType::Stdout(writer) => writer.write(buf),
            WriterType::Stderr(writer) => writer.write(buf),
            WriterType::Capture(buffer) => buffer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            WriterType::Stdout(writer) => writer.flush(),
            WriterType::Stderr(writer) => writer.flush(),
            WriterType::Capture(buffer) => buffer.flush(),
        }
    }
}

/// Managed stream that can be stdout, stderr, or a capture buffer
struct ManagedStream {
    writer: WriterType,
    capture_buffer: Option<CaptureBuffer>,
}

impl ManagedStream {
    fn new_stdout() -> Self {
        Self {
            writer: WriterType::Stdout(BufWriter::new(io::stdout())),
            capture_buffer: None,
        }
    }

    fn new_stderr() -> Self {
        Self {
            writer: WriterType::Stderr(BufWriter::new(io::stderr())),
            capture_buffer: None,
        }
    }

    fn start_capture(&mut self) -> CaptureBuffer {
        let capture_buffer = CaptureBuffer::new();
        self.writer = WriterType::Capture(capture_buffer.clone());
        self.capture_buffer = Some(capture_buffer.clone());
        capture_buffer
    }

    fn release_capture(&mut self, is_stderr: bool) -> Option<String> {
        if let Some(buffer) = &self.capture_buffer {
            let contents = buffer.get_contents();

            // Restore original stream
            self.writer = if is_stderr {
                WriterType::Stderr(BufWriter::new(io::stderr()))
            } else {
                WriterType::Stdout(BufWriter::new(io::stdout()))
            };
            self.capture_buffer = None;

            Some(contents)
        } else {
            // Even if there's no capture buffer, restore the original stream
            self.writer = if is_stderr {
                WriterType::Stderr(BufWriter::new(io::stderr()))
            } else {
                WriterType::Stdout(BufWriter::new(io::stdout()))
            };
            None
        }
    }
}

impl Write for ManagedStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// Global stream instances
static STDOUT_STREAM: std::sync::LazyLock<Mutex<ManagedStream>> =
    std::sync::LazyLock::new(|| Mutex::new(ManagedStream::new_stdout()));

static STDERR_STREAM: std::sync::LazyLock<Mutex<ManagedStream>> =
    std::sync::LazyLock::new(|| Mutex::new(ManagedStream::new_stderr()));

/// Whether user output becomes log records instead of raw process output.
/// Set while logs are JSON on stdout, where a raw line breaks the
/// one-object-per-line stream and output without a newline fuses into the
/// next record.
static USER_OUTPUT_TO_LOG: AtomicBool = AtomicBool::new(false);

/// Send user `print`/`eprint` output to the log (target `hot::user_output`,
/// field `stream`) instead of the process's stdout and stderr. Captures
/// still receive stdout output.
pub fn route_user_output_to_log(enabled: bool) {
    USER_OUTPUT_TO_LOG.store(enabled, Ordering::Relaxed);
}

/// One log record per write, without its trailing newline.
pub(crate) fn log_user_output(stream: &'static str, text: &str) {
    let text = text.strip_suffix('\n').unwrap_or(text);
    tracing::info!(target: "hot::user_output", stream, "{text}");
}

/// Write to stdout
pub fn write_stdout(text: &str) -> io::Result<()> {
    if let Ok(mut stream) = STDOUT_STREAM.lock() {
        if stream.capture_buffer.is_none() && USER_OUTPUT_TO_LOG.load(Ordering::Relaxed) {
            drop(stream);
            log_user_output("stdout", text);
            return Ok(());
        }
        stream.write_all(text.as_bytes())?;
        stream.flush()
    } else {
        Err(io::Error::other("Failed to lock stdout"))
    }
}

/// What one run is capturing. `::hot::io/capture-stdout` captures here, not on
/// the process's streams: a worker holds many tenants' runs, and a run's
/// capture must never hold another run's output. A run's `parallel` branches
/// share it, and it ends with the run.
#[derive(Debug, Default)]
pub struct RunCapture {
    stdout: Mutex<Option<CaptureBuffer>>,
    stderr: Mutex<Option<CaptureBuffer>>,
}

impl RunCapture {
    fn stream(&self, is_stderr: bool) -> &Mutex<Option<CaptureBuffer>> {
        if is_stderr {
            &self.stderr
        } else {
            &self.stdout
        }
    }

    fn start(&self, is_stderr: bool) -> Option<CaptureHandle> {
        let buffer = CaptureBuffer::new();
        *self.stream(is_stderr).lock().ok()? = Some(buffer.clone());
        Some(CaptureHandle::new(buffer, is_stderr))
    }

    fn release(&self, is_stderr: bool) {
        if let Ok(mut stream) = self.stream(is_stderr).lock() {
            *stream = None;
        }
    }

    fn buffer(&self, is_stderr: bool) -> Option<CaptureBuffer> {
        self.stream(is_stderr).lock().ok()?.clone()
    }
}

/// Write a run's output: to its capture if it has one, else to the process.
fn write_run_output(vm: &VirtualMachine, is_stderr: bool, text: &str) -> io::Result<()> {
    match vm.run_capture().buffer(is_stderr) {
        Some(mut buffer) => buffer.write_all(text.as_bytes()),
        None if is_stderr => write_stderr(text),
        None => write_stdout(text),
    }
}

/// Handle for managing stream captures
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct CaptureHandle {
    buffer: CaptureBuffer,
    is_stderr: bool,
}

impl CaptureHandle {
    fn new(buffer: CaptureBuffer, is_stderr: bool) -> Self {
        Self { buffer, is_stderr }
    }

    /// Get the current captured content without releasing the capture
    pub fn get_contents(&self) -> String {
        self.buffer.get_contents()
    }

    /// Clear the captured content
    pub fn clear(&self) {
        self.buffer.clear();
    }

    /// Release the capture and return captured content
    pub fn release(self) -> Option<String> {
        if self.is_stderr {
            release_capture_stderr()
        } else {
            release_capture_stdout()
        }
    }

    /// Check if this is a stderr capture
    pub fn is_stderr(&self) -> bool {
        self.is_stderr
    }
}

/// Start capturing stdout, returns a handle to manage the capture
pub fn start_capture_stdout() -> Option<CaptureHandle> {
    if let Ok(mut stream) = STDOUT_STREAM.lock() {
        let buffer = stream.start_capture();
        Some(CaptureHandle::new(buffer, false))
    } else {
        None
    }
}

/// Start capturing stderr, returns a handle to manage the capture
pub fn start_capture_stderr() -> Option<CaptureHandle> {
    if let Ok(mut stream) = STDERR_STREAM.lock() {
        let buffer = stream.start_capture();
        Some(CaptureHandle::new(buffer, true))
    } else {
        None
    }
}

/// Release stdout capture and return captured content
pub fn release_capture_stdout() -> Option<String> {
    if let Ok(mut stream) = STDOUT_STREAM.lock() {
        stream.release_capture(false)
    } else {
        None
    }
}

/// Release stderr capture and return captured content
pub fn release_capture_stderr() -> Option<String> {
    if let Ok(mut stream) = STDERR_STREAM.lock() {
        stream.release_capture(true)
    } else {
        None
    }
}

/// Handle for managing stream captures - Hot language interface
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct HotCaptureHandle {
    inner: CaptureHandle,
    stream_type: String,
}

impl std::fmt::Debug for HotCaptureHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotCaptureHandle")
            .field("stream_type", &self.stream_type)
            .finish()
    }
}

impl std::fmt::Display for HotCaptureHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HotCaptureHandle({})", self.stream_type)
    }
}

impl HotCaptureHandle {
    pub fn new(handle: CaptureHandle, stream_type: String) -> Self {
        Self {
            inner: handle,
            stream_type,
        }
    }

    /// Check if this is a stderr capture
    pub fn is_stderr(&self) -> bool {
        self.inner.is_stderr()
    }

    /// Get captured content without releasing
    pub fn get_contents(&self) -> String {
        self.inner.get_contents()
    }

    /// Clear captured content
    pub fn clear(&self) {
        self.inner.clear()
    }
}

/// Format a value for raw output (without quotes for strings)
fn format_val_for_raw_output(val: &Val) -> String {
    match val {
        Val::Str(s) => (**s).to_owned(), // No quotes for raw output
        Val::Dec(d) => {
            let f = d.to_string().parse::<f64>().unwrap_or(0.0);
            format!("{:.2}", f)
        }
        _ => val.to_string().trim_matches('"').to_string(), // Remove quotes from other types
    }
}

fn format_output_args(args: &[Val]) -> Result<String, Val> {
    let mut output = String::new();
    for arg in args {
        arg.check_text_shape("print").map_err(Val::from)?;
        let piece = format_val_for_raw_output(arg);
        if piece.len() > crate::blob::MAX_INLINE_HARD_LIMIT_BYTES.saturating_sub(output.len()) {
            return Err(Val::from("print: formatted output exceeds byte limit"));
        }
        output.push_str(&piece);
    }
    Ok(output)
}

/// Print values without newline
pub fn print(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let final_output = match format_output_args(args) {
        Ok(output) => output,
        Err(error) => return HotResult::Err(error),
    };
    if let Err(e) = write_run_output(vm, false, &final_output) {
        return HotResult::Err(Val::from(format!("Failed to write to stdout: {}", e)));
    }
    HotResult::Ok(Val::from(final_output))
}

/// Print values with newline
pub fn println(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let final_line = match format_output_args(args) {
        Ok(output) => output,
        Err(error) => return HotResult::Err(error),
    };
    // Ensure stdout receives the same 2-decimal formatting as returned value
    if let Err(e) = write_run_output(vm, false, &(final_line.clone() + "\n")) {
        return HotResult::Err(Val::from(format!("Failed to write to stdout: {}", e)));
    }
    HotResult::Ok(Val::from(final_line + "\n"))
}

/// Print to stderr without newline
pub fn eprint(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let final_output = match format_output_args(args) {
        Ok(output) => output,
        Err(error) => return HotResult::Err(error),
    };
    if let Err(e) = write_run_output(vm, true, &final_output) {
        return HotResult::Err(Val::from(format!("Failed to write to stderr: {}", e)));
    }
    HotResult::Ok(Val::from(final_output))
}

/// Print to stderr with newline
pub fn eprintln(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    // One write, so a logged line is one record.
    let mut line = match format_output_args(args) {
        Ok(output) => output,
        Err(error) => return HotResult::Err(error),
    };
    line.push('\n');
    if let Err(e) = write_run_output(vm, true, &line) {
        return HotResult::Err(Val::from(format!("Failed to write to stderr: {}", e)));
    }

    // Return the first argument instead of Null so it can be used in expressions
    let result = if args.is_empty() {
        Val::Null
    } else {
        args[0].clone()
    };

    HotResult::Ok(result)
}

/// tap: print value and return it unchanged. For pipeline debugging.
/// 1-arity: tap(value) — prints value, returns value
/// 2-arity: tap(value, label) — prints "label: value", returns value
///
/// In production (HOT_ENV != "development") tap is a no-op pass-through:
/// it returns the value but does NOT print anything. This prevents user
/// code from flooding production logs with debug prints, while keeping
/// `tap` ergonomic during local development. If a user really needs a
/// production log line, they should use the logging API explicitly.
pub fn tap(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(
            "tap expects 1 or 2 arguments (value, [label])".to_string(),
        ));
    }

    // The value parameter is lazy so Results reach tap intact instead of
    // halting at the call boundary; force the thunk here.
    let value = &match super::exec::evaluate_lazy(vm, &args[0]) {
        Ok(value) => value,
        Err(halt) => return halt,
    };

    if !crate::env::is_local_dev() {
        return HotResult::Ok(value.clone());
    }

    let output = if args.len() == 2 {
        let label = match &args[1] {
            Val::Str(s) => s.to_string(),
            other => format!("{}", other),
        };
        format!("{}: {}", label, value)
    } else {
        format!("{}", value)
    };

    eprintln!("{}", output);
    HotResult::Ok(value.clone())
}

/// Helper function to write to stderr
fn write_stderr(s: &str) -> Result<(), std::io::Error> {
    use std::io::Write;
    if USER_OUTPUT_TO_LOG.load(Ordering::Relaxed) {
        log_user_output("stderr", s);
        return Ok(());
    }
    std::io::stderr().write_all(s.as_bytes())?;
    std::io::stderr().flush()?;
    Ok(())
}

/// Capture stdout
pub fn capture_stdout(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_no_args!("::hot::io/capture-stdout", args);

    match vm.run_capture().start(false) {
        Some(handle) => {
            let hot_handle = HotCaptureHandle::new(handle, "stdout".to_string());
            HotResult::Ok(Val::boxed(hot_handle))
        }
        None => HotResult::Err(Val::from("Failed to start stdout capture")),
    }
}

/// Capture stderr
pub fn capture_stderr(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_no_args!("::hot::io/capture-stderr", args);

    match vm.run_capture().start(true) {
        Some(handle) => {
            let hot_handle = HotCaptureHandle::new(handle, "stderr".to_string());
            HotResult::Ok(Val::boxed(hot_handle))
        }
        None => HotResult::Err(Val::from("Failed to start stderr capture")),
    }
}

/// Release a captured stdout or stderr handle
pub fn release(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::io/release", args, 1);

    let handle = &args[0];
    match handle {
        Val::Box(boxed_val) => {
            if let Some(capture_handle) = boxed_val.as_any().downcast_ref::<HotCaptureHandle>() {
                vm.run_capture().release(capture_handle.is_stderr());
                HotResult::Ok(Val::Bool(true))
            } else {
                HotResult::Err(Val::from("Expected a HotCaptureHandle"))
            }
        }
        _ => HotResult::Err(Val::from(
            "Expected a capture handle (boxed value)".to_string(),
        )),
    }
}

/// Discard a captured handle without getting its contents
pub fn discard(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::io/discard", args, 1);

    let handle = &args[0];
    match handle {
        Val::Box(boxed_val) => {
            if let Some(capture_handle) = boxed_val.as_any().downcast_ref::<HotCaptureHandle>() {
                vm.run_capture().release(capture_handle.is_stderr());
                HotResult::Ok(Val::Bool(true))
            } else {
                HotResult::Err(Val::from("Expected a HotCaptureHandle"))
            }
        }
        _ => HotResult::Err(Val::from(
            "Expected a capture handle (boxed value)".to_string(),
        )),
    }
}

/// Get captured content from a handle without releasing it
pub fn get_captured_content(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::io/get-captured-content", args, 1);

    let handle = &args[0];
    match handle {
        Val::Box(boxed_val) => {
            if let Some(capture_handle) = boxed_val.as_any().downcast_ref::<HotCaptureHandle>() {
                let contents = capture_handle.get_contents();
                HotResult::Ok(Val::from(contents))
            } else {
                HotResult::Err(Val::from("Expected a HotCaptureHandle"))
            }
        }
        _ => HotResult::Err(Val::from(
            "Expected a capture handle (boxed value)".to_string(),
        )),
    }
}

/// Clear captured content from a handle without releasing it
pub fn clear_captured_content(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::io/clear-captured-content", args, 1);

    let handle = &args[0];
    match handle {
        Val::Box(boxed_val) => {
            if let Some(capture_handle) = boxed_val.as_any().downcast_ref::<HotCaptureHandle>() {
                capture_handle.clear();
                HotResult::Ok(Val::Null)
            } else {
                HotResult::Err(Val::from("Expected a HotCaptureHandle"))
            }
        }
        _ => HotResult::Err(Val::from(
            "Expected a capture handle (boxed value)".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::BytecodeProgram;
    use crate::lang::compiler::core_registry::CoreVariableRegistry;
    use std::sync::Arc;

    fn run() -> VirtualMachine {
        VirtualMachine::new(
            Arc::new(BytecodeProgram::new()),
            None,
            Arc::new(indexmap::IndexMap::new()),
            Arc::new(indexmap::IndexMap::new()),
            Arc::new(indexmap::IndexMap::new()),
            Arc::new(CoreVariableRegistry::new()),
            None,
        )
    }

    fn captured(handle: &Val) -> String {
        match get_captured_content(std::slice::from_ref(handle)) {
            HotResult::Ok(content) => content.to_string(),
            HotResult::Err(error) => panic!("{error}"),
        }
    }

    fn capture(vm: &mut VirtualMachine) -> Val {
        match capture_stdout(vm, &[]) {
            HotResult::Ok(handle) => handle,
            HotResult::Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn a_run_captures_its_own_output_and_no_other_runs() {
        let mut capturing = run();
        let mut other = run();
        let handle = capture(&mut capturing);

        let _ = println(&mut other, &[Val::from("another tenant's token")]);
        let _ = println(&mut capturing, &[Val::from("my own line")]);

        let content = captured(&handle);
        assert!(content.contains("my own line"), "{content}");
        assert!(!content.contains("token"), "{content}");
    }

    #[test]
    fn a_runs_stderr_capture_is_its_own_too() {
        let mut capturing = run();
        let mut other = run();
        let HotResult::Ok(handle) = capture_stderr(&mut capturing, &[]) else {
            panic!("capture must start");
        };

        let _ = eprint(&mut other, &[Val::from("another tenant's error")]);
        let _ = eprint(&mut capturing, &[Val::from("my own error")]);

        let content = captured(&handle);
        assert!(content.contains("my own error"), "{content}");
        assert!(!content.contains("tenant"), "{content}");
    }

    #[test]
    fn a_runs_parallel_branches_share_its_capture() {
        let mut parent = run();
        let handle = capture(&mut parent);

        let mut branch = run();
        branch.inherit_host_context(&parent.host_context_snapshot());
        let _ = println(&mut branch, &[Val::from("from a branch")]);

        assert!(captured(&handle).contains("from a branch"));
    }

    #[test]
    fn output_after_a_release_is_not_captured() {
        let mut vm = run();
        let handle = capture(&mut vm);
        let _ = print(&mut vm, &[Val::from("kept")]);

        let _ = release(&mut vm, std::slice::from_ref(&handle));
        let _ = print(&mut vm, &[Val::from("after")]);

        let content = captured(&handle);
        assert!(content.contains("kept"), "{content}");
        assert!(!content.contains("after"), "{content}");
    }
}

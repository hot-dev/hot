// IO functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::{validate_args, validate_no_args};
use std::io::{self, BufWriter, Write};
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

/// Write to stdout
pub fn write_stdout(text: &str) -> io::Result<()> {
    if let Ok(mut stream) = STDOUT_STREAM.lock() {
        stream.write_all(text.as_bytes())?;
        stream.flush()
    } else {
        Err(io::Error::other("Failed to lock stdout"))
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

    /// Release the capture and return captured content
    pub fn release(&self) -> Option<String> {
        // Clone the handle to move it into release
        self.inner.clone().release()
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

/// Print values without newline
pub fn print(args: &[Val]) -> HotResult<Val> {
    // Build formatted output for all args
    let mut pieces: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        pieces.push(format_val_for_raw_output(arg));
    }
    let final_output = pieces.join("");
    if let Err(e) = write_stdout(&final_output) {
        return HotResult::Err(Val::from(format!("Failed to write to stdout: {}", e)));
    }
    HotResult::Ok(Val::from(final_output))
}

/// Print values with newline
pub fn println(args: &[Val]) -> HotResult<Val> {
    // Build formatted output for all args
    let mut pieces: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        pieces.push(format_val_for_raw_output(arg));
    }
    let final_line = pieces.join("");
    // Ensure stdout receives the same 2-decimal formatting as returned value
    if let Err(e) = write_stdout(&(final_line.clone() + "\n")) {
        return HotResult::Err(Val::from(format!("Failed to write to stdout: {}", e)));
    }
    HotResult::Ok(Val::from(final_line + "\n"))
}

/// Print to stderr without newline
pub fn eprint(args: &[Val]) -> HotResult<Val> {
    // Build formatted output for all args
    let mut pieces: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        pieces.push(format_val_for_raw_output(arg));
    }
    let final_output = pieces.join("");
    if let Err(e) = write_stderr(&final_output) {
        return HotResult::Err(Val::from(format!("Failed to write to stderr: {}", e)));
    }
    HotResult::Ok(Val::from(final_output))
}

/// Print to stderr with newline
pub fn eprintln(args: &[Val]) -> HotResult<Val> {
    for arg in args {
        let output = format_val_for_raw_output(arg);
        match write_stderr(&output) {
            Ok(()) => {}
            Err(e) => {
                return HotResult::Err(Val::from(format!("Failed to write to stderr: {}", e)));
            }
        }
    }
    match write_stderr("\n") {
        Ok(()) => {}
        Err(e) => return HotResult::Err(Val::from(format!("Failed to write to stderr: {}", e))),
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
pub fn tap(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(
            "tap expects 1 or 2 arguments (value, [label])".to_string(),
        ));
    }

    let value = &args[0];

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
    std::io::stderr().write_all(s.as_bytes())?;
    std::io::stderr().flush()?;
    Ok(())
}

/// Capture stdout
pub fn capture_stdout(args: &[Val]) -> HotResult<Val> {
    validate_no_args!("::hot::io/capture-stdout", args);

    match start_capture_stdout() {
        Some(handle) => {
            let hot_handle = HotCaptureHandle::new(handle, "stdout".to_string());
            HotResult::Ok(Val::boxed(hot_handle))
        }
        None => HotResult::Err(Val::from("Failed to start stdout capture")),
    }
}

/// Capture stderr
pub fn capture_stderr(args: &[Val]) -> HotResult<Val> {
    validate_no_args!("::hot::io/capture-stderr", args);

    match start_capture_stderr() {
        Some(handle) => {
            let hot_handle = HotCaptureHandle::new(handle, "stderr".to_string());
            HotResult::Ok(Val::boxed(hot_handle))
        }
        None => HotResult::Err(Val::from("Failed to start stderr capture")),
    }
}

/// Release a captured stdout or stderr handle
pub fn release(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::io/release", args, 1);

    let handle = &args[0];
    match handle {
        Val::Box(boxed_val) => {
            if let Some(capture_handle) = boxed_val.as_any().downcast_ref::<HotCaptureHandle>() {
                capture_handle.release();
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
pub fn discard(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::io/discard", args, 1);

    let handle = &args[0];
    match handle {
        Val::Box(boxed_val) => {
            if let Some(capture_handle) = boxed_val.as_any().downcast_ref::<HotCaptureHandle>() {
                capture_handle.release();
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

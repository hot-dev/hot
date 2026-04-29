use crate::val::Val;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tracing::{Level, error, info};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;

pub const DEFAULT_LOG_LEVEL: LevelFilter = LevelFilter::INFO;
pub const DEFAULT_LOG_TARGET: LogTarget = LogTarget::Stdout;
pub const DEFAULT_LOG_FORMAT: LogFormat = LogFormat::Full;
pub const DEFAULT_LOG_DIR_RELATIVE_PATH: &str = ".hot/log";
pub const DEFAULT_LOG_ROTATION: LogRotation = LogRotation::Daily;
pub const DEFAULT_LOG_RETENTION: usize = 7;
/// How often to run periodic cleanup (24 hours)
const CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum LogTarget {
    /// Write logs to stdout (console)
    Stdout,
    /// Write logs to a file in `log.dir`
    File,
    /// Disable logging entirely
    None,
}

impl FromStr for LogTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "stdout" => Ok(LogTarget::Stdout),
            "file" => Ok(LogTarget::File),
            "none" => Ok(LogTarget::None),
            _ => Err(format!("Unknown log target: {}", s)),
        }
    }
}

impl std::fmt::Display for LogTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogTarget::Stdout => write!(f, "stdout"),
            LogTarget::File => write!(f, "file"),
            LogTarget::None => write!(f, "none"),
        }
    }
}

/// Log output format
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum LogFormat {
    /// Full format: timestamp, level, target, message (for servers/long-running processes)
    Full,
    /// Simple format: just the message (for CLI commands)
    Simple,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "full" => Ok(LogFormat::Full),
            "simple" => Ok(LogFormat::Simple),
            _ => Err(format!(
                "Unknown log format: '{}'. Valid options: full, simple",
                s
            )),
        }
    }
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Full => write!(f, "full"),
            LogFormat::Simple => write!(f, "simple"),
        }
    }
}

/// Log rotation frequency
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum LogRotation {
    /// Create a new log file every hour: hot.YYYY-MM-DD-HH
    Hourly,
    /// Create a new log file every day: hot.YYYY-MM-DD
    Daily,
    /// No rotation, single log file: hot.log
    None,
}

impl FromStr for LogRotation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "hourly" => Ok(LogRotation::Hourly),
            "daily" => Ok(LogRotation::Daily),
            "none" => Ok(LogRotation::None),
            _ => Err(format!(
                "Unknown log rotation: '{}'. Valid options: hourly, daily, none",
                s
            )),
        }
    }
}

impl std::fmt::Display for LogRotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogRotation::Hourly => write!(f, "hourly"),
            LogRotation::Daily => write!(f, "daily"),
            LogRotation::None => write!(f, "none"),
        }
    }
}

impl LogRotation {
    /// Convert to tracing-appender Rotation
    fn to_tracing_rotation(self) -> Rotation {
        match self {
            LogRotation::Hourly => Rotation::HOURLY,
            LogRotation::Daily => Rotation::DAILY,
            LogRotation::None => Rotation::NEVER,
        }
    }
}

/// Create a default log configuration and merge it with the provided config
pub fn get_resolved_conf(conf: Val) -> Val {
    // Calculate default log directory
    let default_log_dir = match std::env::current_dir() {
        Ok(current_dir) => {
            let log_dir = current_dir.join(DEFAULT_LOG_DIR_RELATIVE_PATH);
            Val::from(log_dir.to_string_lossy().to_string())
        }
        Err(_) => {
            // If we can't get the current directory, just use the relative path
            Val::from(DEFAULT_LOG_DIR_RELATIVE_PATH)
        }
    };

    // Create default log configuration
    let default_conf = crate::val!({
        "level": format!("{}", DEFAULT_LOG_LEVEL),
        "target": DEFAULT_LOG_TARGET.to_string(),
        "dir": default_log_dir,
        "rotation": DEFAULT_LOG_ROTATION.to_string(),
        "retention": DEFAULT_LOG_RETENTION as i64
    });

    // Merge with provided conf (provided conf takes precedence)
    default_conf.merge(&conf)
}

/// Convert a string log level to a tracing::Level
pub fn string_to_level(level_str: &str) -> Level {
    match level_str.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO, // Default to INFO
    }
}

/// Get log level from configuration
pub fn get_log_level_from_conf(conf: &crate::val::Val) -> LevelFilter {
    let level_str = conf.get_str_or_default("log.level", &DEFAULT_LOG_LEVEL.to_string());
    LevelFilter::from_str(&level_str.to_lowercase()).unwrap_or(DEFAULT_LOG_LEVEL)
}

/// Get log target from configuration
pub fn get_log_target_from_conf(conf: &crate::val::Val) -> LogTarget {
    let target_str = conf.get_str_or_default("log.target", &DEFAULT_LOG_TARGET.to_string());
    LogTarget::from_str(&target_str).unwrap_or(DEFAULT_LOG_TARGET)
}

/// Get log directory from configuration
pub fn get_log_dir_from_conf(conf: &crate::val::Val) -> Option<String> {
    let dir = conf.get_str_or_default("log.dir", "null");
    if dir == "null" { None } else { Some(dir) }
}

/// Get log rotation from configuration
pub fn get_log_rotation_from_conf(conf: &crate::val::Val) -> LogRotation {
    let rotation_str = conf.get_str_or_default("log.rotation", &DEFAULT_LOG_ROTATION.to_string());
    LogRotation::from_str(&rotation_str).unwrap_or(DEFAULT_LOG_ROTATION)
}

/// Get log retention (number of files to keep) from configuration
pub fn get_log_retention_from_conf(conf: &crate::val::Val) -> usize {
    conf.get_int_or_default("log.retention", DEFAULT_LOG_RETENTION as i64) as usize
}

/// Get log format from configuration
pub fn get_log_format_from_conf(conf: &crate::val::Val) -> LogFormat {
    let format_str = conf.get_str_or_default("log.format", &DEFAULT_LOG_FORMAT.to_string());
    LogFormat::from_str(&format_str).unwrap_or(DEFAULT_LOG_FORMAT)
}

/// Clean up old log files, keeping only the most recent `retention` files
pub fn cleanup_old_logs(log_dir: &PathBuf, retention: usize) {
    if retention == 0 {
        return; // 0 means keep all logs
    }

    // List all hot.* files in the directory (tracing-appender uses "hot.YYYY-MM-DD" format)
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let mut log_files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let path = entry.path();
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Match both old format (hot.*.log) and new format (hot.YYYY-MM-DD)
            filename.starts_with("hot.")
        })
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((path, modified))
        })
        .collect();

    // Sort by modification time, newest first
    log_files.sort_by_key(|f| std::cmp::Reverse(f.1));

    // Delete files beyond the retention count
    for (path, _) in log_files.into_iter().skip(retention) {
        if let Err(e) = fs::remove_file(&path) {
            // Use eprintln since this might run before logging is initialized
            eprintln!("Failed to delete old log file {}: {}", path.display(), e);
        }
    }
}

/// Spawn a background task that periodically cleans up old log files.
/// This ensures log cleanup happens even for long-running processes like the LSP.
fn spawn_periodic_cleanup(log_dir: PathBuf, retention: usize) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        // Skip the first tick (cleanup already ran at startup)
        interval.tick().await;

        loop {
            interval.tick().await;
            cleanup_old_logs(&log_dir, retention);
            info!("Periodic log cleanup completed (retention: {})", retention);
        }
    });
}

/// Set up logging based on configuration
/// The `format` parameter controls output format:
/// - `Full`: timestamp, level, target, message (for servers)
/// - `Simple`: just the message (for CLI commands)
pub fn setup_tracing(
    conf: &crate::val::Val,
    format: LogFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    // Extract log settings from configuration
    let log_level = get_log_level_from_conf(conf);
    let log_target = get_log_target_from_conf(conf);
    let log_dir = get_log_dir_from_conf(conf);
    let log_rotation = get_log_rotation_from_conf(conf);
    let log_retention = get_log_retention_from_conf(conf);

    // Check if format is overridden in config
    let format = {
        let format_str = conf.get_str_or_default("log.format", "");
        if format_str.is_empty() {
            format // Use the provided default
        } else {
            LogFormat::from_str(&format_str).unwrap_or(format)
        }
    };

    // Create enhanced filter that respects the configured log level for all components
    // Suppress sqlx::postgres::notice INFO messages (e.g., "schema already exists, skipping")
    let filter = EnvFilter::new(format!(
        "{},sqlx={},sqlx::postgres::notice=warn,tower_http={},rustyline=off,cranelift_jit=warn,cranelift_codegen=warn",
        log_level, log_level, log_level
    ));

    match log_target {
        LogTarget::Stdout => {
            // Disable ANSI colors if NO_COLOR is set or when running outside local development.
            let use_ansi = std::env::var("NO_COLOR").is_err() && crate::env::is_local_dev();

            match format {
                LogFormat::Full => {
                    // Full format with timestamp, level, target
                    fmt::Subscriber::builder()
                        .with_writer(std::io::stdout)
                        .with_ansi(use_ansi)
                        .with_target(true)
                        .with_level(true)
                        .with_thread_ids(false)
                        .with_thread_names(false)
                        .with_file(false)
                        .with_line_number(false)
                        .with_timer(fmt::time::UtcTime::rfc_3339())
                        .with_env_filter(filter)
                        .init();
                }
                LogFormat::Simple => {
                    // Simple format: just the message
                    fmt::Subscriber::builder()
                        .with_writer(std::io::stdout)
                        .with_ansi(use_ansi)
                        .with_target(false)
                        .with_level(false)
                        .with_thread_ids(false)
                        .with_thread_names(false)
                        .with_file(false)
                        .with_line_number(false)
                        .without_time()
                        .with_env_filter(filter)
                        .init();
                }
            }
        }
        LogTarget::File => {
            // File logging always uses full format (timestamps are important for logs)
            if let Some(dir) = log_dir.clone() {
                let path = PathBuf::from(&dir);

                // Create the log directory if it doesn't exist
                if !path.exists()
                    && let Err(e) = fs::create_dir_all(&path)
                {
                    eprintln!("Failed to create log directory {}: {}", dir, e);
                    // Fall back to console logging with simple format
                    let use_ansi = std::env::var("NO_COLOR").is_err() && crate::env::is_local_dev();
                    fmt::Subscriber::builder()
                        .with_writer(std::io::stderr)
                        .with_ansi(use_ansi)
                        .with_target(false)
                        .with_level(false)
                        .with_thread_ids(false)
                        .with_thread_names(false)
                        .with_file(false)
                        .with_line_number(false)
                        .without_time()
                        .with_env_filter(filter)
                        .init();
                    return Ok(());
                }

                // Clean up old log files before setting up the appender
                cleanup_old_logs(&path, log_retention);

                // Create rolling file appender that automatically rotates based on time
                let file_appender = RollingFileAppender::new(
                    log_rotation.to_tracing_rotation(),
                    &dir,
                    "hot", // prefix - files will be named hot.YYYY-MM-DD or hot.YYYY-MM-DD-HH
                );

                // Initialize the file logging subscriber with the rolling appender
                // File logs always use full format for debugging purposes
                fmt::Subscriber::builder()
                    .with_writer(file_appender)
                    .with_ansi(false) // No ANSI colors in log files
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(false)
                    .with_thread_names(false)
                    .with_file(false)
                    .with_line_number(false)
                    .with_timer(fmt::time::UtcTime::rfc_3339())
                    .with_env_filter(filter)
                    .init();

                // Log the directory being used for logging
                info!(
                    "Logging to directory: {} (rotation: {}, retention: {})",
                    dir, log_rotation, log_retention
                );

                // Spawn background task for periodic cleanup (for long-running processes)
                spawn_periodic_cleanup(path, log_retention);
            } else {
                // Fall back to console logging if no log directory is specified
                error!(
                    "Log target set to 'file' but no log directory specified, falling back to stdout"
                );
                let use_ansi = std::env::var("NO_COLOR").is_err() && crate::env::is_local_dev();
                fmt::Subscriber::builder()
                    .with_writer(std::io::stderr)
                    .with_ansi(use_ansi)
                    .with_target(false)
                    .with_level(false)
                    .with_thread_ids(false)
                    .with_thread_names(false)
                    .with_file(false)
                    .with_line_number(false)
                    .without_time()
                    .with_env_filter(filter)
                    .init();
            }
        }
        LogTarget::None => {
            // Do not initialize any subscriber; tracing will use the no-op default.
            return Ok(());
        }
    }

    Ok(())
}

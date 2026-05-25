//! Command-line interface definitions: clap structs/enums for `hot`'s
//! commands, options, and subcommands. Field visibility is `pub(crate)` so
//! the dispatch logic in `main.rs` and the per-command handler modules can
//! destructure `Command` variants and read option fields directly.

use clap::builder::BoolishValueParser;
use clap::{Args, Parser as ClapParser, Subcommand, ValueHint};
use clap_complete::Shell;

// Global options that apply to all commands
#[derive(Args, Debug, Clone, Default)]
pub(crate) struct GlobalOptions {
    #[arg(
        short = 'c',
        long = "conf",
        value_name = "FILE",
        help = "Configuration file(s) (can be used multiple times)",
        action = clap::ArgAction::Append,
        value_hint = ValueHint::FilePath,
        global = true,
    )]
    pub(crate) conf_files: Vec<String>,

    #[arg(
        long = "ctx",
        value_name = "FILE",
        help = "Context file(s) for setting context variables (can be used multiple times)",
        action = clap::ArgAction::Append,
        value_hint = ValueHint::FilePath,
        global = true,
    )]
    pub(crate) ctx_files: Vec<String>,

    #[arg(
        short = 'p',
        long = "project",
        value_name = "NAME",
        help = "Project name to use (defaults to configured default project)",
        global = true
    )]
    pub(crate) project: Option<String>,

    #[arg(
        short = 's',
        long = "src.path",
        value_name = "DIRECTORY",
        help = "Source directory for current project (can be used multiple times)",
        action = clap::ArgAction::Append,
        value_hint = ValueHint::DirPath,
        global = true,
    )]
    pub(crate) src_paths: Vec<String>,

    #[arg(
        short = 't',
        long = "test.path",
        value_name = "DIRECTORY",
        help = "Test directory for current project (can be used multiple times)",
        action = clap::ArgAction::Append,
        value_hint = ValueHint::DirPath,
        global = true,
    )]
    pub(crate) test_paths: Vec<String>,

    #[arg(
        short = 'r',
        long = "resource.path",
        value_name = "DIRECTORY",
        help = "Resource directory to expose to ::hot::resource (can be used multiple times)",
        action = clap::ArgAction::Append,
        value_hint = ValueHint::DirPath,
        global = true,
    )]
    pub(crate) resource_paths: Vec<String>,

    #[arg(
        long = "no-gitignore",
        help = "Do not honor .gitignore / .hotignore when discovering files and resources",
        global = true
    )]
    pub(crate) no_gitignore: bool,

    // Cache options removed - will be reimplemented with  bytecode caching
    #[arg(
        long = "engine.threads",
        value_name = "COUNT",
        help = "Engine threads for parallel execution (default: 4)",
        global = true
    )]
    pub(crate) engine_threads: Option<usize>,

    #[arg(
        long = "jit",
        value_name = "MODE",
        help = "JIT compilation: enabled, disabled (default: enabled on supported platforms)",
        global = true
    )]
    pub(crate) jit_mode: Option<String>,

    #[arg(
        long = "jit.threshold",
        value_name = "COUNT",
        help = "JIT compilation threshold — calls before compiling (default: 100)",
        global = true
    )]
    pub(crate) jit_threshold: Option<u32>,

    #[arg(
        long = "db.uri",
        value_name = "URI",
        help = "Database connection URI",
        global = true
    )]
    pub(crate) db_uri: Option<String>,

    #[arg(
        long = "log.level",
        value_name = "LEVEL",
        help = "Log level: off, trace, debug, error, warn, info",
        global = true
    )]
    pub(crate) log_level: Option<String>,

    #[arg(
        long = "log.target",
        value_name = "TARGET",
        help = "Log output: stdout, file, none",
        global = true
    )]
    pub(crate) log_target: Option<String>,

    #[arg(
        long = "log.dir",
        value_name = "DIRECTORY",
        help = "Directory for log files",
        value_hint = ValueHint::DirPath,
        global = true,
    )]
    pub(crate) log_dir: Option<String>,

    #[arg(
        long = "log.rotation",
        value_name = "ROTATION",
        help = "Log rotation: hourly, daily, none",
        global = true
    )]
    pub(crate) log_rotation: Option<String>,

    #[arg(
        long = "log.retention",
        value_name = "COUNT",
        help = "Number of log files to keep (0 = keep all)",
        global = true
    )]
    pub(crate) log_retention: Option<i64>,

    #[arg(
        long = "log.format",
        value_name = "FORMAT",
        help = "Log output format: full (timestamp+level+target+message), simple (message only)",
        global = true
    )]
    pub(crate) log_format: Option<String>,

    #[arg(
        long = "deploy.auto",
        help = "Automatically deploy live builds when running CLI commands",
        value_name = "ENABLED",
        value_parser = BoolishValueParser::new(),
        action = clap::ArgAction::Set,
        default_value = "true",
        global = true,
    )]
    pub(crate) deploy_auto: bool,

    #[command(flatten)]
    pub(crate) emitter: EmitterOptions,

    #[arg(
        long = "with-tests",
        help = "Include test files in compile/check/watch [default: false]",
        value_name = "ENABLED",
        value_parser = BoolishValueParser::new(),
        action = clap::ArgAction::Set,
        global = true,
    )]
    pub(crate) with_tests: Option<bool>,
}

// Emitter options for event tracking
#[derive(Args, Debug, Clone, Default)]
pub(crate) struct EmitterOptions {
    #[arg(
        long = "emitter.type",
        value_name = "TYPE",
        help = "Emitter type: none, console, db [default: db in project, none otherwise]",
        global = true
    )]
    pub(crate) emitter_type: Option<String>,
}

// Server options for services (api, app, worker, scheduler, dev)
#[derive(Args, Debug, Clone)]
pub(crate) struct ServerOptions {
    #[arg(
        short = 'd',
        long = "daemon",
        help = "Run in daemon mode",
        action = clap::ArgAction::SetTrue,
    )]
    pub(crate) daemon: bool,
}

// Dev-specific options
#[derive(Args, Debug, Clone)]
pub(crate) struct DevOptions {
    #[arg(
        long = "open",
        help = "Open browser to app URL after server starts",
        action = clap::ArgAction::SetTrue,
    )]
    pub(crate) open: bool,
}

// Network options for api and app services
#[derive(Args, Debug, Clone)]
pub(crate) struct NetworkOptions {
    #[arg(long = "api.host", value_name = "HOST", help = "API server host")]
    pub(crate) api_host: Option<String>,

    #[arg(long = "api.port", value_name = "PORT", help = "API server port")]
    pub(crate) api_port: Option<u16>,

    #[arg(long = "app.host", value_name = "HOST", help = "App server host")]
    pub(crate) app_host: Option<String>,

    #[arg(long = "app.port", value_name = "PORT", help = "App server port")]
    pub(crate) app_port: Option<u16>,
}

// Queue options for services that use queues
#[derive(Args, Debug, Clone)]
pub(crate) struct QueueOptions {
    #[arg(
        long = "queue.type",
        value_name = "TYPE",
        help = "Queue type: memory, redis"
    )]
    pub(crate) queue_type: Option<String>,

    #[arg(long = "redis.uri", value_name = "URI", help = "Redis connection URI")]
    pub(crate) redis_uri: Option<String>,

    #[arg(
        long = "serialization",
        value_name = "FORMAT",
        help = "Serialization format: json, zstdjson"
    )]
    pub(crate) serialization: Option<String>,
}

// Worker-specific options
#[derive(Args, Debug, Clone)]
pub(crate) struct WorkerOptions {
    #[arg(
        long = "worker.threads",
        value_name = "COUNT",
        help = "Worker thread count (default: 8)"
    )]
    pub(crate) worker_threads: Option<usize>,
}

// Scheduler-specific options
#[derive(Args, Debug, Clone)]
pub(crate) struct SchedulerOptions {
    #[arg(
        long = "scheduler.backfill",
        help = "Enable backfilling missed scheduled events on restart",
        value_name = "ENABLED",
        value_parser = BoolishValueParser::new(),
        action = clap::ArgAction::Set,
    )]
    pub(crate) scheduler_backfill: Option<bool>,

    #[arg(
        long = "scheduler.sync-interval-seconds",
        value_name = "SECONDS",
        help = "Interval for syncing schedules from database"
    )]
    pub(crate) scheduler_sync_interval_seconds: Option<u64>,
}

// Test-specific options
#[derive(Args, Debug, Clone)]
pub(crate) struct TestOptions {
    #[arg(
        long = "test.capture",
        help = "Enable test output capture",
        action = clap::ArgAction::Set,
    )]
    pub(crate) capture: Option<bool>,

    #[arg(
        long = "integration",
        help = "Boot integration services (task worker, API, etc.) from the package's integration.hot config before running tests",
        action = clap::ArgAction::SetTrue,
    )]
    pub(crate) integration: bool,
}

// Show configuration flag
#[derive(Args, Debug, Clone)]
pub(crate) struct ShowConfOptions {
    #[arg(
        long = "show-conf",
        help = "Show configuration for this command and exit",
        action = clap::ArgAction::SetTrue,
    )]
    pub(crate) show_conf: bool,
}

#[derive(Subcommand, Debug)]
#[command(subcommand_value_name = "COMMAND")]
pub(crate) enum Command {
    // ==================== Running Code (1-9) ====================
    /// Execute a single .hot file
    #[command(display_order = 1)]
    Run {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Path to the .hot file to execute
        #[arg(value_hint = ValueHint::FilePath)]
        file: String,
        /// Output format for values: "hot" (default) or "json"
        #[arg(long = "value.format", value_name = "FORMAT")]
        value_format: Option<String>,
    },
    /// Evaluate a Hot code string directly
    #[command(display_order = 2)]
    Eval {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Hot code to evaluate
        code: String,
        /// Output format for values: "hot" (default) or "json"
        #[arg(long = "value.format", value_name = "FORMAT")]
        value_format: Option<String>,
    },
    /// Start the interactive Hot REPL
    #[command(display_order = 3)]
    Repl {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Output format for values: "hot" (default) or "json"
        #[arg(long = "value.format", value_name = "FORMAT")]
        value_format: Option<String>,
    },

    // ==================== Services (10-19) ====================
    /// Run all services in development mode (api + app + worker + scheduler)
    #[command(display_order = 10)]
    Dev {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        server: ServerOptions,
        #[command(flatten)]
        network: NetworkOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(flatten)]
        worker: WorkerOptions,
        #[command(flatten)]
        scheduler: SchedulerOptions,
        #[command(flatten)]
        dev: DevOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    /// Run the API server
    #[command(display_order = 11)]
    Api {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        server: ServerOptions,
        #[command(flatten)]
        network: NetworkOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    /// Run the web application server
    #[command(display_order = 12)]
    App {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        server: ServerOptions,
        #[command(flatten)]
        network: NetworkOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    /// Run the background worker
    #[command(display_order = 13)]
    Worker {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        server: ServerOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(flatten)]
        worker: WorkerOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    /// Run the task worker (processes ::hot::task and ::hot::box tasks)
    #[command(name = "task-worker", display_order = 14)]
    TaskWorker {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    /// Run the job scheduler
    #[command(display_order = 15)]
    Scheduler {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        server: ServerOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(flatten)]
        scheduler: SchedulerOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    // ==================== Testing & Development (20-29) ====================
    /// Run tests (all tests or matching pattern)
    #[command(display_order = 20)]
    Test {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        test: TestOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Test pattern to match
        pattern: Option<String>,
    },
    /// Analyze project sources and report diagnostics without executing
    #[command(display_order = 21)]
    Check {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Output format: pretty (ariadne, default), simple (one line per error),
        /// json (pretty-printed array of LSP-shaped Diagnostic objects),
        /// or json-min (single-line JSON, ideal for CI / editor tooling).
        /// Exit code is 0 on success, 1 if any diagnostics are emitted.
        #[arg(long = "check.format", value_name = "FORMAT")]
        format: Option<String>,
        /// Raw output (pretty/simple): suppresses success banners and headers
        #[arg(long = "check.raw", action = clap::ArgAction::SetTrue)]
        raw: bool,
        /// Optional path to a specific .hot file or directory to check (if not provided, checks all project sources)
        #[arg(value_hint = ValueHint::AnyPath)]
        path: Option<String>,
    },
    /// Watch for changes and continuously analyze the project (like `check --check.watch`)
    #[command(display_order = 22)]
    Watch {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Output format: pretty (ariadne, default), simple (one line per error),
        /// json (pretty-printed array of LSP-shaped Diagnostic objects),
        /// or json-min (single-line JSON, ideal for CI / editor tooling).
        /// Re-emitted on each file change. Exit code mirrors the most recent run.
        #[arg(long = "check.format", value_name = "FORMAT")]
        format: Option<String>,
        /// Raw output (pretty/simple): suppresses success banners and headers
        #[arg(long = "check.raw", action = clap::ArgAction::SetTrue)]
        raw: bool,
        /// Debounce in milliseconds for watch mode
        #[arg(long = "watch.debounce", value_name = "MILLISECONDS")]
        watch_debounce_ms: Option<u64>,
    },
    /// Format Hot source files (safe mode: only writes files that pass CHAR-AUDIT)
    #[command(display_order = 23)]
    Fmt {
        #[command(flatten)]
        global: GlobalOptions,
        /// Optional path to a specific .hot file to format (if not provided, formats all files in src/pkg paths)
        #[arg(value_hint = ValueHint::FilePath)]
        file: Option<String>,
        /// Force writing files even if they fail CHAR-AUDIT (default: safe mode, only write files that pass audit)
        #[arg(long = "force", action = clap::ArgAction::SetTrue)]
        force: bool,
        /// Check if files are formatted without writing changes (preview mode)
        #[arg(long = "check", action = clap::ArgAction::SetTrue)]
        check: bool,
    },

    // ==================== Build & Deploy (30-39) ====================
    /// Create a build from current project's source and package files
    #[command(display_order = 30)]
    Build {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Build output directory (default: .hot/build)
        #[arg(long = "build.dir", value_name = "DIRECTORY", value_hint = ValueHint::DirPath)]
        build_dir: Option<String>,
        /// Bypass the build-time secret-shape scanner (use sparingly; for
        /// per-file allowlists prefer `hot.build.allow-secret-shape` in
        /// hot.hot so the exception is reviewable).
        #[arg(long = "allow-secret-shape")]
        allow_secret_shape: bool,
    },
    /// List builds for the current environment
    #[command(display_order = 31)]
    Builds {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Number of builds to show (default: 20)
        #[arg(long = "limit", value_name = "COUNT")]
        limit: Option<i64>,
        /// Number of builds to skip (default: 0)
        #[arg(long = "offset", value_name = "COUNT")]
        offset: Option<i64>,
        /// Query local database directly instead of API
        #[arg(long)]
        local: bool,
    },
    /// Compile project source and create/update live build
    #[command(display_order = 32)]
    Compile {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Project name to compile (defaults to configured default project)
        project_name: Option<String>,
    },
    /// Deploy a specific build by ID
    #[command(display_order = 33)]
    Deploy {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Build ID to deploy (from 'builds' command output). If not provided, creates a new bundle build from current source.
        build_id: Option<String>,
        /// Deploy locally via direct database access instead of API
        #[arg(long)]
        local: bool,
        /// Bypass the build-time secret-shape scanner (use sparingly; for
        /// per-file allowlists prefer `hot.build.allow-secret-shape` in
        /// hot.hot so the exception is reviewable).
        #[arg(long = "allow-secret-shape")]
        allow_secret_shape: bool,
        /// Block the deploy if any required context variables reachable from
        /// your code are unset. Without this flag, missing required ctx
        /// vars produce a warning but the deploy proceeds. Equivalent to
        /// setting `hot.deploy.ctx.strict: true` in `hot.hot`.
        #[arg(long = "strict")]
        strict: bool,
    },
    /// Upload a local build to remote environment (internal command)
    #[command(display_order = 34, hide = true)]
    Upload {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Build ID to upload (from 'builds --local' command output)
        build_id: String,
    },
    /// Extract a build to a directory (internal command)
    #[command(display_order = 35, hide = true)]
    Extract {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Build filename or full path to the build zip file
        #[arg(value_hint = ValueHint::FilePath)]
        build_path: String,
        /// Directory to extract to (default: build-name + short SHA)
        #[arg(long = "extract.dir", value_name = "DIRECTORY", value_hint = ValueHint::DirPath)]
        extract_dir: Option<String>,
        /// Directory to look for build files (default: .hot/build)
        #[arg(long = "build.dir", value_name = "DIRECTORY", value_hint = ValueHint::DirPath)]
        build_dir: Option<String>,
    },

    // ==================== Project Management (40-49) ====================
    /// Initialize Hot in a directory
    ///
    /// With no argument, initializes in the current directory.
    /// With a path, creates the directory if needed and initializes there.
    /// The project name is the last segment of the path (or current dir name).
    #[command(display_order = 40)]
    Init {
        /// Directory to initialize (default: current directory)
        #[arg(value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<String>,
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
    },
    /// Manage a project (activate/deactivate)
    #[command(display_order = 41)]
    Project {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        #[command(subcommand)]
        action: ProjectAction,
        /// Operate on local database directly instead of API
        #[arg(long, global = true)]
        local: bool,
    },
    /// List projects in the current environment
    #[command(display_order = 42)]
    Projects {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Number of projects to show (default: 20)
        #[arg(long = "limit", value_name = "COUNT")]
        limit: Option<i64>,
        /// Number of projects to skip (default: 0)
        #[arg(long = "offset", value_name = "COUNT")]
        offset: Option<i64>,
        /// Query local database directly instead of API
        #[arg(long)]
        local: bool,
    },
    /// Manage project dependencies
    #[command(display_order = 43)]
    Deps {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        #[command(subcommand)]
        action: DepsAction,
    },
    /// Manage project context variables (encrypted key-value storage)
    #[command(display_order = 44, hide = true, alias = "ctx")]
    Context {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        #[command(subcommand)]
        action: ContextAction,
        /// Query local database directly instead of API
        #[arg(long, global = true)]
        local: bool,
    },

    // ==================== Database & Infrastructure (50-59) ====================
    /// Manage database
    #[command(display_order = 50, hide = true)]
    Db {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        #[command(subcommand)]
        action: DbAction,
    },
    /// Manage bytecode and package caches
    #[command(display_order = 36)]
    Cache {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Show or generate configuration
    #[command(display_order = 45)]
    Conf {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        server: ServerOptions,
        #[command(flatten)]
        network: NetworkOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(flatten)]
        worker: WorkerOptions,
        #[command(flatten)]
        test: TestOptions,
        #[command(subcommand)]
        action: Option<ConfAction>,
    },

    // ==================== Tooling (60-69) ====================
    /// Start the LSP server
    #[command(display_order = 60)]
    Lsp {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        show_conf: ShowConfOptions,
        /// Transport (defaults to conf hot.lsp.transport)
        #[arg(long = "lsp.transport", value_name = "TRANSPORT")]
        transport: Option<String>,
        /// Alias for --lsp.transport stdio (added for vscode-languageclient compatibility)
        #[arg(long = "stdio", help = "Alias for --lsp.transport stdio", action = clap::ArgAction::SetTrue, hide = true)]
        stdio: bool,
    },
    /// Generate shell completions
    #[command(display_order = 61)]
    Completions {
        /// Target shell (bash, zsh, fish, powershell, elvish)
        #[arg(value_enum)]
        shell: Shell,
        /// Output directory; if omitted, prints to stdout
        #[arg(long)]
        out_dir: Option<std::path::PathBuf>,
    },
    /// Setup AI coding support using AGENTS.md and SKILL.md standards
    #[command(display_order = 62)]
    Ai {
        #[command(subcommand)]
        action: AiAction,
    },
    /// Generate documentation JSON for packages (internal command)
    #[command(display_order = 63, hide = true)]
    Docs {
        #[command(flatten)]
        global: GlobalOptions,
        /// Package name(s) to generate docs for (can specify multiple: --pkg hot-std --pkg other-pkg)
        #[arg(long = "pkg", value_name = "PACKAGE")]
        packages: Vec<String>,
        /// Generate docs for all known packages
        #[arg(long = "all")]
        all: bool,
        /// Output directory for generated JSON (default: resources/pkg-docs)
        #[arg(long = "out.dir", value_name = "DIRECTORY", value_hint = ValueHint::DirPath)]
        out_dir: Option<String>,
    },

    // ==================== Info (70-79) ====================
    /// Display version information
    #[command(display_order = 70)]
    Version,
    /// Check for available updates
    #[command(display_order = 71)]
    Update {
        /// Force re-download even if already on the latest version
        #[arg(long)]
        force: bool,
        /// Install a specific Hot version, for example 1.4.0
        #[arg(long = "version", short = 'v', value_name = "VERSION")]
        version: Option<String>,
    },
    /// Display help information
    #[command(display_order = 72)]
    Help {
        /// Command to get help for
        command: Option<String>,
    },

    // ==================== Hidden (internal/developer commands) ====================
    /// Manage event queues (internal/developer command)
    #[command(hide = true)]
    Queue {
        #[command(flatten)]
        global: GlobalOptions,
        #[command(flatten)]
        queue: QueueOptions,
        #[command(subcommand)]
        action: QueueAction,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum QueueAction {
    /// Clear all event queues
    Clear,
    /// Show queue status and lengths
    Status,
}

// CacheAction enum removed - will be reimplemented with  bytecode caching

#[derive(Subcommand, Debug)]
pub(crate) enum DbAction {
    /// Show database status
    Status,
    /// Run database migrations
    Migrate,
    /// Port a local Hot 1.x development database to the Hot 2.0 schema
    #[command(name = "port-v1-to-v2")]
    PortV1ToV2,
}

#[derive(Subcommand, Debug)]
pub(crate) enum CacheAction {
    /// Clear all caches (bytecode, package, app caches)
    Clear,
    /// Show cache status and sizes
    Status,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ContextAction {
    /// List all context variables for a project
    List,
    /// Get a context variable value
    Get {
        /// Context variable key
        key: String,
    },
    /// Set a context variable value
    Set {
        /// Context variable key
        key: String,
        /// Context variable value (Hot code expression)
        value: String,
        /// Description for the context variable
        #[arg(long = "description", short = 'd')]
        description: Option<String>,
    },
    /// Delete a context variable
    Delete {
        /// Context variable key
        key: String,
    },
    /// Show required context variables reachable from the project's code,
    /// and which of them are currently set vs unset.
    ///
    /// Builds the project (in-process, no DB writes) and computes the same
    /// set of required ctx keys that `hot deploy` would gate on. Useful to
    /// answer "what do I actually need to set before deploying?".
    Required {
        /// Project name (defaults to configured default project / -p flag)
        project_name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ProjectAction {
    /// Activate a project
    Activate {
        /// Project name
        project_name: String,
    },
    /// Deactivate a project
    Deactivate {
        /// Project name
        project_name: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum DepsAction {
    /// List all dependencies for the current project
    List,
    /// Show detailed information about dependencies
    Show,
    /// Add a new dependency to the project
    Add {
        /// Package name (e.g., "hot.dev/anthropic")
        package: String,
        /// Local file system path for the package
        #[arg(long = "local")]
        local: Option<String>,
        /// Version for registry packages
        #[arg(long = "version")]
        version: Option<String>,
        /// Git repository URL
        #[arg(long = "git")]
        git: Option<String>,
        /// Git branch
        #[arg(long = "branch")]
        branch: Option<String>,
        /// Git tag or commit SHA
        #[arg(long = "tag")]
        tag: Option<String>,
        /// Path within git repository (for monorepos)
        #[arg(long = "path")]
        path: Option<String>,
    },
    /// Remove a dependency from the project
    Remove {
        /// Package name to remove
        package: String,
    },
    /// Update dependencies (resolve and cache)
    Update,
    /// Migrate from pkg.paths to deps format
    Migrate,
}

#[derive(Subcommand, Debug)]
#[command(
    after_help = "Files created:\n  AGENTS.md              - AI agent instructions (passive context)\n  .skills/hot-language/  - Hot language skill with references\n\nExamples:\n  hot ai add           # Add AGENTS.md + bundled skill to project\n  hot ai add --global  # Install bundled skill to ~/.skills/\n  hot ai list          # Show installed files\n  hot ai update        # Update installed files to this Hot version\n\nFor the public skills.sh source, use:\n  npx skills add hot-dev/hot-skills"
)]
pub(crate) enum AiAction {
    /// Add AI coding support (AGENTS.md + .skills/) to the project
    Add {
        /// Install skills to user directory (~/.skills/) instead of project
        #[arg(long)]
        global: bool,
    },
    /// List installed AI support files and their locations
    List,
    /// Update existing AI support files to the latest version
    Update,
}

#[derive(Subcommand, Debug)]
#[command(
    after_help = "Available templates:\n  (default)   - Minimal configuration for local development\n  all         - Full configuration with all available options\n  api         - API server configuration\n  app         - App server configuration\n  worker      - Worker configuration\n  scheduler   - Scheduler configuration\n\nExamples:\n  hot conf generate              # Minimal template to stdout\n  hot conf generate all          # Full template to stdout\n  hot conf generate -o hot.hot   # Write minimal template to file\n  hot conf generate api -o api.hot.hot"
)]
pub(crate) enum ConfAction {
    /// Generate a configuration template
    Generate {
        /// Template type (default: minimal for local development)
        #[arg(default_value = "minimal")]
        template: String,
        /// Output file path (default: stdout)
        #[arg(short = 'o', long = "output", value_name = "PATH")]
        output: Option<String>,
    },
}

// Custom help template with grouped command sections
pub(crate) const HELP_TEMPLATE: &str = "\
{about}

{usage-heading} {usage}

Commands:
  Services:
    dev          Run all services in development mode (api + app + worker + scheduler)
    api          Run the API server
    app          Run the web application server
    worker       Run the background worker
    scheduler    Run the job scheduler

  Running Code:
    run          Execute a single .hot file
    eval         Evaluate a Hot code string directly
    repl         Start the interactive Hot REPL

  Testing & Development:
    test         Run tests (all tests or matching pattern)
    check        Analyze project sources and report diagnostics without executing
    watch        Watch for changes and continuously analyze the project
    fmt          Format Hot source files

  Build & Deploy:
    build        Create a build from current project's source and package files
    builds       List builds for the current environment
    compile      Compile project source and create/update live build
    deploy       Deploy a specific build by ID
    cache        Manage bytecode and package caches

  Project Management:
    init         Initialize Hot in a directory
    project      Manage a project (activate/deactivate)
    projects     List projects in the current environment
    deps         Manage project dependencies
    context      Manage project context variables (encrypted key-value storage)
    conf         Show configuration

  Tooling:
    lsp          Start the LSP server
    completions  Generate shell completions
    ai           Setup AI coding support (AGENTS.md + skills)

  Info:
    version      Display version information
    update       Check for available updates
    help         Display help information

Options:
{options}
{after-help}";

// Hidden commands section shown when HOT_FIRE env var is set
pub(crate) const HIDDEN_COMMANDS_HELP: &str = "
  Internal Commands (HOT_FIRE):
    upload       Upload a local build to remote environment
    extract      Extract a build to a directory
    docs         Generate documentation JSON for packages
    queue        Manage event queues (clear, status)
";

#[derive(ClapParser, Debug)]
#[command(
    name = "hot",
    about = "Hot - A dynamic language runtime and development environment",
    long_about = "Hot is a dynamic language runtime and development environment with built-in\nserver capabilities, testing framework, and interactive REPL.\n\nUse 'hot help <COMMAND>' for command-specific help.\n\nIf no command is provided, hot will read Hot code from stdin and evaluate it.",
    after_help = "Use 'hot version' for detailed version information.",
    help_template = HELP_TEMPLATE,
    disable_version_flag = true,
    disable_help_flag = true,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    #[command(flatten)]
    pub(crate) global: GlobalOptions,
}

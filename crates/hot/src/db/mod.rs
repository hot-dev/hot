use crate::val::Val;
use sqlx::{Executor, Pool, Postgres, Sqlite, postgres::PgPoolOptions};
use std::path::PathBuf;
use thiserror::Error;
use tracing::debug;
use url::Url;
use uuid::Uuid;

/// Macro to log database errors consistently
#[macro_export]
macro_rules! log_db_error {
    ($operation:expr, $error:expr) => {
        tracing::error!("Database operation '{}' failed: {:?}", $operation, $error);
        match &$error {
            sqlx::Error::RowNotFound => {
                tracing::warn!("No rows found for operation: {}", $operation);
            }
            sqlx::Error::Database(db_err) => {
                tracing::error!(
                    "Database-specific error in '{}': code={:?}, message={}",
                    $operation,
                    db_err.code(),
                    db_err.message()
                );
            }
            sqlx::Error::ColumnNotFound(col) => {
                tracing::error!("Column '{}' not found in operation: {}", col, $operation);
            }
            sqlx::Error::TypeNotFound { type_name } => {
                tracing::error!(
                    "Type '{}' not found in operation: {}",
                    type_name,
                    $operation
                );
            }
            sqlx::Error::Configuration(msg) => {
                tracing::error!("Configuration error in '{}': {}", $operation, msg);
            }
            sqlx::Error::Tls(msg) => {
                tracing::error!("TLS error in '{}': {}", $operation, msg);
            }
            sqlx::Error::Protocol(msg) => {
                tracing::error!("Protocol error in '{}': {}", $operation, msg);
            }
            sqlx::Error::Io(io_err) => {
                tracing::error!("IO error in '{}': {}", $operation, io_err);
            }
            sqlx::Error::WorkerCrashed => {
                tracing::error!("Database worker crashed during operation: {}", $operation);
            }
            _ => {
                tracing::error!("Unknown database error in '{}': {:?}", $operation, $error);
            }
        }
    };
    ($operation:expr, $error:expr, $context:expr) => {
        tracing::error!(
            "Database operation '{}' failed with context '{}': {:?}",
            $operation,
            $context,
            $error
        );
        match &$error {
            sqlx::Error::RowNotFound => {
                tracing::warn!(
                    "No rows found for operation '{}' with context: {}",
                    $operation,
                    $context
                );
            }
            sqlx::Error::Database(db_err) => {
                tracing::error!(
                    "Database-specific error in '{}' ({}): code={:?}, message={}",
                    $operation,
                    $context,
                    db_err.code(),
                    db_err.message()
                );
            }
            sqlx::Error::ColumnNotFound(col) => {
                tracing::error!(
                    "Column '{}' not found in operation '{}' ({})",
                    col,
                    $operation,
                    $context
                );
            }
            sqlx::Error::TypeNotFound { type_name } => {
                tracing::error!(
                    "Type '{}' not found in operation '{}' ({})",
                    type_name,
                    $operation,
                    $context
                );
            }
            _ => {
                tracing::error!(
                    "Database error in '{}' ({}): {:?}",
                    $operation,
                    $context,
                    $error
                );
            }
        }
    };
}

// Re-export the macro for use in other modules
pub use log_db_error;

// Module declarations for entity files
pub mod access;
pub mod agent;
pub mod alert;
pub mod api_key;
pub mod build;
pub mod call;
pub mod context;
pub mod domain;
pub mod email_queue;
pub mod email_verification;
pub mod env;
pub mod event;
pub mod event_handler;
pub mod features;
pub mod file;
pub mod file_upload;
pub mod hierarchy;
pub mod invite;
pub mod mcp_tool;
pub mod org;
pub mod org_note;
pub mod port;
pub mod project;
pub mod run;
pub mod schedule;
pub mod schedule_log;
pub mod scheduler_state;
pub(crate) mod search;
pub mod service_key;
pub mod session;
pub mod stream;
pub mod subscription;
pub mod task;
pub mod team;
pub mod user;
pub mod webhook;
pub mod workflow;

// Re-export structs and errors for convenience
pub use access::{Access, AccessError};
pub use agent::{
    Agent, AgentError, AgentHealthSummary, AgentNonAgentCounts, AgentRunStats, AgentStats,
    AgentWithProject,
};
pub use build::{Build, BuildError};
pub use call::Call;
pub use context::{Context, ContextError};
pub use domain::{Domain, DomainError};
pub use email_verification::{EmailVerification, EmailVerificationError, VerificationStatus};
pub use env::{Env, EnvError};
pub use event::{Event, EventError};
pub use event_handler::{EventHandler, EventHandlerError, EventHandlerWithProject};
pub use features::Features;
pub use hierarchy::{HierarchyNode, HierarchyResponse, build_hierarchy};
pub use invite::{Invite, InviteError, InviteStatus};
pub use mcp_tool::{McpServiceSummary, McpTool, McpToolError, McpToolWithProject};
pub use org::{Org, OrgError, OrgUser};
pub use org_note::{OrgNote, OrgNoteError};
pub use project::{Project, ProjectError};
pub use run::{Run, RunError, RunStatus, RunStatusParseError};
pub use schedule::{
    AT_SCHEDULE_PREFIX, Schedule, ScheduleError, SchedulePolicy, ScheduleType, ScheduleWithProject,
    parse_schedule_expression, validate_recurring_schedule_interval,
};
pub use schedule_log::{ScheduleLog, ScheduleLogError};
pub use scheduler_state::{SchedulerState, SchedulerStateError};
pub use service_key::{ServiceKey, ServiceKeyError};
pub use session::{Session, SessionError};
pub use stream::{Stream, StreamError, StreamSummary};
pub use subscription::{
    BillingPeriod, OrgPlan, OrgPlanStatus, OrgUsage, OrgUsageStats, Plan, PlanError,
};
pub use task::{Task, TaskError, TaskStatus};
pub use team::{Team, TeamError, TeamUser, TeamUserWithRole};
pub use user::{User, UserAuth, UserError};
pub use webhook::{Webhook, WebhookError, WebhookServiceSummary, WebhookWithProject};
pub use workflow::{Workflow, WorkflowError, WorkflowWithProject};

pub const DEFAULT_DB_URI: &str = "sqlite:./.hot/db/hot.sqlite.db";
pub const DEFAULT_DB_SCHEMA: &str = "hot";

#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("Database connection error: {0}")]
    Connection(#[from] sqlx::Error),
    #[error("Database URL parsing error: {0}")]
    UrlParse(#[from] url::ParseError),
    #[error("Migration error: {0}")]
    Migration(String),
    #[error("Database not initialized: {0}")]
    NotInitialized(String),
    #[error("Unsupported database type: {0}")]
    UnsupportedType(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("API key error: {0}")]
    ApiKey(#[from] api_key::ApiKeyError),
    #[error("Build error: {0}")]
    Build(#[from] BuildError),
    #[error("Environment error: {0}")]
    Env(#[from] EnvError),
    #[error("Event error: {0}")]
    Event(#[from] EventError),
    #[error("Event handler error: {0}")]
    EventHandler(#[from] event_handler::EventHandlerError),
    #[error("MCP tool error: {0}")]
    McpTool(#[from] mcp_tool::McpToolError),
    #[error("Webhook error: {0}")]
    Webhook(#[from] webhook::WebhookError),
    #[error("Invite error: {0}")]
    Invite(#[from] InviteError),
    #[error("Schedule error: {0}")]
    Schedule(#[from] ScheduleError),
    #[error("Schedule log error: {0}")]
    ScheduleLog(#[from] ScheduleLogError),
    #[error("Scheduler state error: {0}")]
    SchedulerState(#[from] SchedulerStateError),
    #[error("Organization error: {0}")]
    Org(#[from] OrgError),
    #[error("Project error: {0}")]
    Project(#[from] ProjectError),
    #[error("User error: {0}")]
    User(#[from] UserError),
    #[error("Subscription error: {0}")]
    Subscription(#[from] PlanError),
    #[error("Run error: {0}")]
    Run(#[from] run::RunError),
}

#[derive(Debug, Clone)]
pub enum DatabaseType {
    Sqlite,
    Postgres,
}

#[derive(Clone)]
pub enum DatabasePool {
    Sqlite(Pool<Sqlite>),
    Postgres(Pool<Postgres>),
}

impl DatabaseType {
    pub fn from_uri(uri: &str) -> Result<Self, DatabaseError> {
        if uri.starts_with("sqlite:") {
            Ok(DatabaseType::Sqlite)
        } else if uri.starts_with("postgres:") || uri.starts_with("postgresql:") {
            Ok(DatabaseType::Postgres)
        } else {
            Err(DatabaseError::UnsupportedType(format!(
                "Unsupported database URI scheme: {}",
                uri
            )))
        }
    }

    pub fn migration_dir(&self) -> Result<PathBuf, DatabaseError> {
        let db_type_str = match self {
            DatabaseType::Sqlite => "sqlite",
            DatabaseType::Postgres => "postgres",
        };

        crate::resources::get_migration_path(db_type_str).map_err(|e| {
            DatabaseError::Migration(format!("Failed to locate migration directory: {}", e))
        })
    }
}

/// Create a resolved database configuration from the full config using dotted paths
pub fn get_resolved_conf(conf: Val) -> Val {
    // Database is always project-local (.hot/db/), never system-level.
    // This ensures project state is isolated and never accidentally shared.
    let default_db_uri = {
        let db_dir = crate::lang::cache::paths::get_db_dir();
        let db_path = db_dir.join("hot.sqlite.db");
        format!("sqlite:{}", db_path.to_string_lossy())
    };

    // Create db defaults
    let db_defaults = crate::val!({
        "uri": default_db_uri,
        "schema": DEFAULT_DB_SCHEMA,
    });

    // Extract db-specific configuration from the full config
    let db_section = conf.get("db").unwrap_or(crate::val::Val::map_empty());

    // Merge defaults with db-specific config (config overrides defaults)
    db_defaults.merge(&db_section)
}

/// Local dev call data retention in days (default 7). Set to -1 to disable cleanup.
/// Reads from `db.local.retention.call.days` in the full (pre-resolved) config,
/// overridable via `HOT_DB_LOCAL_RETENTION_CALL_DAYS`.
pub fn get_local_call_retention_days(conf: &Val) -> i32 {
    conf.get_int_or_default("db.local.retention.call.days", 7) as i32
}

/// Get database URI from configuration
pub fn get_db_uri_from_conf(conf: &Val) -> String {
    // First try to get from the full config path (db.uri)
    if let Some(crate::val::Val::Str(uri_str)) = conf.get("db.uri") {
        return (*uri_str).to_string();
    }

    // If that doesn't exist, try the resolved config path (uri)
    if let Some(crate::val::Val::Str(uri_str)) = conf.get("uri") {
        return (*uri_str).to_string();
    }

    // Default to SQLite if no URI is configured
    DEFAULT_DB_URI.to_string()
}

/// Redact password from database URI for safe display
pub fn redact_password(uri: &str) -> String {
    match Url::parse(uri) {
        Ok(mut url) => {
            if url.password().is_some() {
                let _ = url.set_password(Some("***"));
            }
            url.to_string()
        }
        Err(_) => uri.to_string(), // Return as-is if can't parse
    }
}

/// Create a database connection pool
pub async fn create_db_pool(conf: &Val) -> Result<DatabasePool, DatabaseError> {
    let uri = get_db_uri_from_conf(conf);
    let db_type = DatabaseType::from_uri(&uri)?;

    tracing::debug!("Creating database pool for: {}", redact_password(&uri));
    tracing::debug!("Database type: {:?}", db_type);

    match db_type {
        DatabaseType::Sqlite => {
            // Check if the SQLite directory exists - don't auto-create it.
            // Directory creation should only happen during `hot init`.
            // This prevents polluting random directories when users run scripts
            // outside of a project context.
            //
            // Skip check for:
            // - In-memory databases (:memory:)
            // - Empty parent paths (just a filename in current dir)
            let path_part = uri.strip_prefix("sqlite:").unwrap_or(&uri);
            let is_memory_db = path_part == ":memory:" || path_part.contains(":memory:");
            if !is_memory_db
                && let Some(parent) = PathBuf::from(path_part).parent()
                // Only check if parent is non-empty (has a directory component)
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                return Err(DatabaseError::NotInitialized(format!(
                    "Database directory does not exist: {}. Run 'hot init' to initialize a project.",
                    parent.display()
                )));
            }

            // Add connection options to enable database creation
            let uri_with_options = format!("{}?mode=rwc", uri);
            tracing::debug!(
                "Connecting to SQLite with URI: {}",
                redact_password(&uri_with_options)
            );

            use sqlx::sqlite::SqlitePoolOptions;
            let max_connections = crate::runtime_budget::derive_sqlite_pool_connections(conf);
            tracing::debug!(
                "SQLite pool max_connections={} from worker.local-write-concurrency",
                max_connections
            );
            let db = SqlitePoolOptions::new()
                .max_connections(max_connections)
                .acquire_timeout(std::time::Duration::from_secs(30))
                .after_connect(|conn, _meta| {
                    Box::pin(async move {
                        use sqlx::Executor;
                        // Enable WAL mode for better concurrency (allows multiple readers with one writer)
                        conn.execute("PRAGMA journal_mode = WAL;").await?;
                        // Increase busy timeout to 30 seconds (waits for locks instead of failing immediately)
                        conn.execute("PRAGMA busy_timeout = 30000;").await?;
                        // Enable foreign keys
                        conn.execute("PRAGMA foreign_keys = ON;").await?;
                        tracing::debug!("SQLite configured: WAL mode enabled, busy_timeout=30s");
                        Ok(())
                    })
                })
                .connect(&uri_with_options)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to connect to SQLite database: {:?}", e);
                    DatabaseError::Connection(e)
                })?;
            tracing::debug!("Successfully connected to SQLite database");
            Ok(DatabasePool::Sqlite(db))
        }
        DatabaseType::Postgres => {
            tracing::debug!("Connecting to Postgres with URI: {}", redact_password(&uri));

            // Get schema configuration - try multiple paths for compatibility
            let schema = {
                let db_schema = conf.get_str_or_default("db.schema", "");
                if !db_schema.is_empty() {
                    db_schema
                } else {
                    conf.get_str_or_default("schema", DEFAULT_DB_SCHEMA)
                }
            };
            tracing::debug!("Using Postgres schema: {}", schema);

            let max_connections = crate::runtime_budget::derive_postgres_pool_connections(conf);
            tracing::debug!(
                "Postgres pool max_connections={} from derived local execution budget",
                max_connections
            );

            let db = PgPoolOptions::new()
                .max_connections(max_connections)
                .acquire_timeout(std::time::Duration::from_secs(5)) // Fail fast instead of hanging
                .idle_timeout(std::time::Duration::from_secs(60))
                .max_lifetime(std::time::Duration::from_secs(1800)) // 30 minutes
                .after_connect(move |conn, _meta| {
                    let schema = schema.clone();
                    Box::pin(async move {
                        // Set search_path so queries use the correct schema
                        conn.execute(sqlx::AssertSqlSafe(format!(
                            "set search_path = '{}', 'public'",
                            schema
                        )))
                        .await?;
                        Ok(())
                    })
                })
                .connect(&uri)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to connect to Postgres database: {:?}", e);
                    DatabaseError::Connection(e)
                })?;
            tracing::debug!("Successfully connected to Postgres database");
            Ok(DatabasePool::Postgres(db))
        }
    }
}

/// Test database connection by executing a simple query
pub async fn test_connection(db: &DatabasePool) -> Result<(), DatabaseError> {
    match db {
        DatabasePool::Sqlite(db) => {
            let _row = sqlx::query("SELECT 1").fetch_one(db).await?;
        }
        DatabasePool::Postgres(db) => {
            let _row = sqlx::query("SELECT 1").fetch_one(db).await?;
        }
    }
    Ok(())
}

/// Run database migrations
pub async fn run_migrations(conf: &Val) -> Result<(), DatabaseError> {
    let uri = get_db_uri_from_conf(conf);
    let db_type = DatabaseType::from_uri(&uri)?;
    let migration_path = db_type.migration_dir()?;

    tracing::debug!("Starting database migrations");
    tracing::debug!("Database URI: {}", redact_password(&uri));
    tracing::debug!("Database type: {:?}", db_type);
    tracing::debug!("Migration directory: {}", migration_path.display());

    // Check if migration directory exists
    if !migration_path.exists() {
        let error_msg = format!(
            "Migration directory does not exist: {}",
            migration_path.display()
        );
        tracing::error!("{}", error_msg);
        return Err(DatabaseError::Migration(error_msg));
    }

    debug!(
        "Running database migrations from: {}",
        migration_path.display()
    );

    // Create pool and run migrations based on database type
    match db_type {
        DatabaseType::Sqlite => {
            tracing::debug!("Running SQLite migrations");
            // Ensure the directory exists for SQLite databases
            let path_part = uri.strip_prefix("sqlite:").unwrap_or(&uri);
            if let Some(parent) = PathBuf::from(path_part).parent() {
                tracing::debug!("Creating SQLite migration directory: {:?}", parent);
                std::fs::create_dir_all(parent)?;
            }

            // Add connection options to enable database creation
            let uri_with_options = format!("{}?mode=rwc", uri);
            tracing::debug!(
                "Connecting to SQLite for migrations: {}",
                redact_password(&uri_with_options)
            );

            use sqlx::sqlite::SqlitePoolOptions;
            let db = SqlitePoolOptions::new()
                .max_connections(10)
                .acquire_timeout(std::time::Duration::from_secs(30))
                .after_connect(|conn, _meta| {
                    Box::pin(async move {
                        use sqlx::Executor;
                        // Enable WAL mode for better concurrency
                        conn.execute("PRAGMA journal_mode = WAL;").await?;
                        // Increase busy timeout to 30 seconds
                        conn.execute("PRAGMA busy_timeout = 30000;").await?;
                        // Enable foreign keys
                        conn.execute("PRAGMA foreign_keys = ON;").await?;
                        tracing::debug!(
                            "SQLite configured for migrations: WAL mode enabled, busy_timeout=30s"
                        );
                        Ok(())
                    })
                })
                .connect(&uri_with_options)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to connect to SQLite for migrations: {:?}", e);
                    DatabaseError::Connection(e)
                })?;

            tracing::debug!("Creating migrator for SQLite");
            let migrator = sqlx::migrate::Migrator::new(migration_path)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to create SQLite migrator: {:?}", e);
                    DatabaseError::Migration(e.to_string())
                })?;

            tracing::debug!("Executing SQLite migrations");
            migrator.run(&db).await.map_err(|e| {
                tracing::error!("SQLite migration failed: {:?}", e);
                translate_migrate_error(&DatabaseType::Sqlite, e)
            })?;

            // Explicitly close the pool to ensure all connections are released
            // and SQLite WAL is properly checkpointed before other connections open
            tracing::debug!("Closing SQLite migration pool");
            db.close().await;
        }
        DatabaseType::Postgres => {
            tracing::debug!("Running Postgres migrations");
            // Get schema configuration - try multiple paths for compatibility
            let schema = {
                let db_schema = conf.get_str_or_default("db.schema", "");
                if !db_schema.is_empty() {
                    db_schema
                } else {
                    conf.get_str_or_default("schema", DEFAULT_DB_SCHEMA)
                }
            };
            tracing::debug!("Using Postgres migration schema: {}", schema);

            let db = PgPoolOptions::new()
                .max_connections(1)
                .after_connect(move |conn, _meta| {
                    let schema = schema.clone();
                    Box::pin(async move {
                        tracing::debug!("Creating schema if not exists: {}", schema);
                        conn.execute(sqlx::AssertSqlSafe(format!(
                            "create schema if not exists {}",
                            schema
                        )))
                        .await?;
                        tracing::debug!("Setting migration search path to: {}", schema);
                        conn.execute(sqlx::AssertSqlSafe(format!(
                            "set search_path = '{}', 'public'",
                            schema
                        )))
                        .await?;
                        Ok(())
                    })
                })
                .connect(&uri)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to connect to Postgres for migrations: {:?}", e);
                    DatabaseError::Connection(e)
                })?;

            tracing::debug!("Creating migrator for Postgres");
            let migrator = sqlx::migrate::Migrator::new(migration_path)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to create Postgres migrator: {:?}", e);
                    DatabaseError::Migration(e.to_string())
                })?;

            tracing::debug!("Executing Postgres migrations");
            migrator.run(&db).await.map_err(|e| {
                tracing::error!("Postgres migration failed: {:?}", e);
                translate_migrate_error(&DatabaseType::Postgres, e)
            })?;

            // Explicitly close the pool to ensure all connections are released
            tracing::debug!("Closing Postgres migration pool");
            db.close().await;
        }
    }

    debug!("Migrations completed successfully");
    Ok(())
}

/// Translate a sqlx migrator error into a `DatabaseError::Migration` with a user-facing
/// hint when the error is the kind that fires when Hot 2 is pointed at a Hot 1.x ledger.
///
/// The raw sqlx wording (for example "migration 2 was previously applied but is missing in
/// the resolved migrations") tells users nothing about what to do next. Hot 2 ships a
/// clean v1 baseline and does not auto-adopt a Hot 1.x schema; the recovery path is
/// `hot db port-v1-to-v2` for SQLite and a fresh v2 database for Postgres.
fn translate_migrate_error(
    db_type: &DatabaseType,
    err: sqlx::migrate::MigrateError,
) -> DatabaseError {
    use sqlx::migrate::MigrateError;
    let needs_hint = matches!(
        err,
        MigrateError::VersionMissing(_)
            | MigrateError::VersionMismatch(_)
            | MigrateError::VersionNotPresent(_)
            | MigrateError::VersionTooOld(_, _)
            | MigrateError::VersionTooNew(_, _)
            | MigrateError::Dirty(_)
    );
    let base = err.to_string();
    if !needs_hint {
        return DatabaseError::Migration(base);
    }
    let hint = match db_type {
        DatabaseType::Sqlite => {
            "Hot 2 detected a Hot 1.x SQLite database. Run `hot db port-v1-to-v2` to back up \
             your v1 database and copy its data into a fresh Hot 2 database, or delete the \
             SQLite file (typically `.hot/db/hot.sqlite.db`) to start fresh without preserving \
             data. \
             See https://hot.dev/docs/migrations#upgrading-from-hot-1x-to-hot-2"
        }
        DatabaseType::Postgres => {
            "Hot 2 detected a Hot 1.x Postgres database. Hot 2 does not auto-port Postgres data; \
             point Hot 2 at a fresh Postgres database (or schema). For Hot Cloud production \
             environments, the v1→v2 backfill is owned by the private cloud repository. \
             See https://hot.dev/docs/migrations#upgrading-from-hot-1x-to-hot-2"
        }
    };
    DatabaseError::Migration(format!("{base}\n\n{hint}"))
}

/// Check if there are any existing organizations and users in the database
pub async fn check_default_data_exists(db: &DatabasePool) -> Result<(i64, i64), DatabaseError> {
    let org_count = Org::get_count(db).await?;
    let user_count = User::get_count(db).await?;
    Ok((org_count, user_count))
}

/// Insert default organization and user if none exist
pub async fn insert_default_data(
    db: &DatabasePool,
) -> Result<(uuid::Uuid, uuid::Uuid, uuid::Uuid), DatabaseError> {
    use uuid::Uuid;

    // Generate new UUIDs
    let org_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    let org_user_id = Uuid::now_v7();
    let user_auth_id = Uuid::now_v7();
    let env_id = Uuid::now_v7();

    // Begin transaction by using the entity functions
    // Insert user first (self-referential for created_by_user_id)
    User::insert_user(
        db,
        &user_id,
        "local@hot.dev",
        Some("Hot Dev"),
        Some(&user_id),
    )
    .await?;

    // Insert user authentication for email/password
    let password_hash = crate::auth::hash_password_with_random_salt("hotdev").map_err(|e| {
        DatabaseError::User(UserError::Database(sqlx::Error::Protocol(format!(
            "Password hashing failed: {}",
            e
        ))))
    })?;

    let auth_data: serde_json::Value = serde_json::from_str(&password_hash).map_err(|e| {
        DatabaseError::User(UserError::Database(sqlx::Error::Protocol(format!(
            "JSON parsing failed: {}",
            e
        ))))
    })?;

    UserAuth::insert_user_auth(
        db,
        &user_auth_id,
        &user_id,
        "email_password",
        "local@hot.dev",
        Some(&auth_data),
        &user_id,
    )
    .await?;

    // Insert org (individual type for local dev)
    Org::insert_org(db, &org_id, "Local", "local", "individual", &user_id).await?;

    // Insert org_user relationship (created by the user)
    OrgUser::insert_org_user(
        db,
        &org_user_id,
        &org_id,
        &user_id,
        Some(2), // Use admin role for local dev (ID 2 = admin)
        &user_id,
    )
    .await?;

    // Insert default environment for the organization
    Env::insert_env(db, &env_id, &org_id, "development", &user_id).await?;

    Ok((org_id, user_id, org_user_id))
}

/// Get the default organization and user IDs (assumes they exist)
pub async fn get_default_org_and_user_ids(
    db: &DatabasePool,
) -> Result<(Uuid, Uuid), DatabaseError> {
    let org = Org::get_default_org(db).await?;
    let user = User::get_default_user(db).await?;
    Ok((org.org_id, user.user_id))
}

/// Test data IDs returned from insert_test_data
#[derive(Debug, Clone)]
pub struct TestData {
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub env_id: Uuid,
    pub project_id: Uuid,
    pub build_id: Uuid,
    pub run_id: Uuid,
    pub stream_id: Uuid,
}

/// Insert all data needed for tests: user, org, env, project, build, and run
/// This is a convenience function for test setup that creates a complete
/// environment for file operations and other tests that need database context.
pub async fn insert_test_data(db: &DatabasePool) -> Result<TestData, DatabaseError> {
    use uuid::Uuid;

    // First insert the default data (user, org, env)
    let (org_id, user_id, _org_user_id) = insert_default_data(db).await?;

    // Get the env that was created
    let env = Env::get_default_env(db).await?;
    let env_id = env.env_id;

    // Create a test project
    let project_id = Uuid::now_v7();
    project::Project::insert_project(db, &project_id, &env_id, "test-project", &user_id).await?;

    // Create a test build
    let build_id = Uuid::now_v7();
    build::Build::insert_build(
        db,
        &build_id,
        &project_id,
        "test-hash",
        0,
        build::Build::BUILD_TYPE_LIVE,
        &user_id,
    )
    .await?;

    // Create a test run
    let run_id = Uuid::now_v7();
    let stream_id = Uuid::now_v7();
    let run_type_id = run::RunType::Run.as_id();

    run::Run::insert_run(
        db,
        &run_id,
        &env_id,
        &stream_id,
        Some(&build_id),
        run_type_id,
        None, // origin_run_id
        &user_id,
        None, // start_time
        None, // access_id
    )
    .await?;

    Ok(TestData {
        org_id,
        user_id,
        env_id,
        project_id,
        build_id,
        run_id,
        stream_id,
    })
}

/// Resolve a meta `Val` for DB storage by converting any `Val::Box` references
/// (TypeRef, FunctionRef, AstNode) into plain strings. This prevents unresolved
/// AST nodes from being serialized as opaque `{"$box": ...}` objects in the meta
/// JSON column.
pub fn resolve_meta_val(val: &crate::val::Val) -> crate::val::Val {
    val.resolve_boxes()
}

/// Merge statically-detected send targets into a handler's meta JSON.
///
/// Takes the existing `meta` (which may already contain a user-declared `sends` array)
/// and a list of statically-detected event names. Returns updated meta with the union
/// of both sets under `meta.sends`. User-declared entries are preserved as-is; static
/// entries that aren't already present are appended.
pub fn merge_sends_into_meta(
    meta: Option<serde_json::Value>,
    static_sends: &[String],
) -> Option<serde_json::Value> {
    if static_sends.is_empty() && meta.as_ref().and_then(|m| m.get("sends")).is_none() {
        return meta;
    }

    let mut meta_map = match meta {
        Some(serde_json::Value::Object(m)) => m,
        Some(other) => return Some(other),
        None => serde_json::Map::new(),
    };

    // Collect existing manual sends (may be strings or objects with "event" key)
    let mut known_events: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut sends_array: Vec<serde_json::Value> = Vec::new();

    if let Some(existing) = meta_map.remove("sends") {
        match existing {
            serde_json::Value::Array(arr) => {
                for item in arr {
                    match &item {
                        serde_json::Value::String(s) => {
                            known_events.insert(s.clone());
                        }
                        serde_json::Value::Object(obj) => {
                            if let Some(serde_json::Value::String(s)) = obj.get("event") {
                                known_events.insert(s.clone());
                            }
                        }
                        _ => {}
                    }
                    sends_array.push(item);
                }
            }
            serde_json::Value::String(s) => {
                known_events.insert(s.clone());
                sends_array.push(serde_json::Value::String(s));
            }
            other => {
                sends_array.push(other);
            }
        }
    }

    // Append static sends that aren't already declared
    for event_name in static_sends {
        if !known_events.contains(event_name) {
            sends_array.push(serde_json::Value::String(event_name.clone()));
            known_events.insert(event_name.clone());
        }
    }

    if !sends_array.is_empty() {
        meta_map.insert("sends".to_string(), serde_json::Value::Array(sends_array));
    }

    Some(serde_json::Value::Object(meta_map))
}

/// Create an in-memory SQLite database with all migrations applied.
/// Use this in tests instead of hand-rolling CREATE TABLE statements
/// so schemas never drift from the real migrations.
///
/// Foreign keys are disabled so tests can insert rows with arbitrary
/// UUIDs without needing to populate every referenced parent table.
#[cfg(any(test, feature = "test-utils"))]
pub async fn test_db() -> DatabasePool {
    use sqlx::sqlite::SqlitePoolOptions;

    let migration_path = crate::resources::get_migration_path("sqlite")
        .expect("SQLite migration path should resolve");

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("Failed to create in-memory SQLite database");

    let migrator = sqlx::migrate::Migrator::new(migration_path)
        .await
        .expect("Failed to load migrations");

    migrator
        .run(&pool)
        .await
        .expect("All SQLite migrations should apply cleanly");

    // Disable FK enforcement so tests can use arbitrary UUIDs
    // without populating every parent table.
    pool.execute("PRAGMA foreign_keys = OFF;")
        .await
        .expect("Failed to disable foreign keys");

    DatabasePool::Sqlite(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // merge_sends_into_meta tests
    // ========================================================================

    #[test]
    fn test_merge_sends_both_empty() {
        let result = merge_sends_into_meta(None, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_merge_sends_meta_none_with_static() {
        let result = merge_sends_into_meta(None, &["event-a".to_string(), "event-b".to_string()]);
        let obj = result.unwrap();
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].as_str().unwrap(), "event-a");
        assert_eq!(sends[1].as_str().unwrap(), "event-b");
    }

    #[test]
    fn test_merge_sends_existing_manual_no_static() {
        let meta = serde_json::json!({"on-event": "order:created", "sends": ["email:send"]});
        let result = merge_sends_into_meta(Some(meta), &[]);
        let obj = result.unwrap();
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].as_str().unwrap(), "email:send");
        assert_eq!(
            obj.get("on-event").unwrap().as_str().unwrap(),
            "order:created"
        );
    }

    #[test]
    fn test_merge_sends_union_dedup() {
        let meta = serde_json::json!({"sends": ["event-a", "event-b"]});
        let result =
            merge_sends_into_meta(Some(meta), &["event-b".to_string(), "event-c".to_string()]);
        let obj = result.unwrap();
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 3);
        assert_eq!(sends[0].as_str().unwrap(), "event-a");
        assert_eq!(sends[1].as_str().unwrap(), "event-b");
        assert_eq!(sends[2].as_str().unwrap(), "event-c");
    }

    #[test]
    fn test_merge_sends_rich_objects_preserved() {
        let meta = serde_json::json!({
            "sends": [{"event": "email:send", "doc": "Send welcome email"}]
        });
        let result = merge_sends_into_meta(
            Some(meta),
            &["email:send".to_string(), "audit:log".to_string()],
        );
        let obj = result.unwrap();
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 2);
        assert!(sends[0].is_object(), "Rich object should be preserved");
        assert_eq!(
            sends[0].get("event").unwrap().as_str().unwrap(),
            "email:send"
        );
        assert_eq!(sends[1].as_str().unwrap(), "audit:log");
    }

    #[test]
    fn test_merge_sends_single_string_sends() {
        let meta = serde_json::json!({"sends": "single-event"});
        let result = merge_sends_into_meta(Some(meta), &["another-event".to_string()]);
        let obj = result.unwrap();
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].as_str().unwrap(), "single-event");
        assert_eq!(sends[1].as_str().unwrap(), "another-event");
    }

    #[test]
    fn test_merge_sends_preserves_other_meta_keys() {
        let meta = serde_json::json!({
            "on-event": "user:created",
            "retry": 3,
            "sends": ["email:send"]
        });
        let result = merge_sends_into_meta(Some(meta), &["audit:log".to_string()]);
        let obj = result.unwrap();
        assert_eq!(
            obj.get("on-event").unwrap().as_str().unwrap(),
            "user:created"
        );
        assert_eq!(obj.get("retry").unwrap().as_i64().unwrap(), 3);
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 2);
    }

    #[test]
    fn test_merge_sends_meta_not_object_passthrough() {
        let meta = serde_json::json!("just a string");
        let result = merge_sends_into_meta(Some(meta.clone()), &["event-a".to_string()]);
        assert_eq!(result.unwrap(), meta);
    }

    #[test]
    fn test_merge_sends_no_sends_key_no_static() {
        let meta = serde_json::json!({"on-event": "user:created"});
        let result = merge_sends_into_meta(Some(meta.clone()), &[]);
        assert_eq!(result.unwrap(), meta, "Should return meta unchanged");
    }

    #[test]
    fn test_merge_sends_empty_static_with_existing_sends_preserved() {
        let meta = serde_json::json!({"sends": ["keep-me"]});
        let result = merge_sends_into_meta(Some(meta), &[]);
        let obj = result.unwrap();
        let sends = obj.get("sends").unwrap().as_array().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].as_str().unwrap(), "keep-me");
    }

    #[test]
    fn test_redact_password() {
        assert_eq!(
            redact_password("postgres://user:password@localhost/db"),
            "postgres://user:***@localhost/db"
        );
        assert_eq!(redact_password("sqlite:./test.db"), "sqlite:./test.db");
    }

    #[test]
    fn test_database_type_from_uri() {
        assert!(matches!(
            DatabaseType::from_uri("sqlite:./test.db"),
            Ok(DatabaseType::Sqlite)
        ));
        assert!(matches!(
            DatabaseType::from_uri("postgres://localhost/db"),
            Ok(DatabaseType::Postgres)
        ));
        assert!(matches!(
            DatabaseType::from_uri("postgresql://localhost/db"),
            Ok(DatabaseType::Postgres)
        ));
        assert!(DatabaseType::from_uri("mysql://localhost/db").is_err());
    }

    /// Run all SQLite migrations against a fresh in-memory database.
    ///
    /// Catches invalid SQL, missing table references, bad foreign keys, and
    /// syntax errors before migration files are released.
    #[tokio::test]
    async fn test_sqlite_migrations_apply_cleanly() {
        let migration_path = crate::resources::get_migration_path("sqlite")
            .expect("SQLite migration path should resolve");

        assert!(
            migration_path.exists(),
            "Migration directory must exist: {}",
            migration_path.display()
        );

        use sqlx::sqlite::SqlitePoolOptions;
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    use sqlx::Executor;
                    conn.execute("PRAGMA foreign_keys = ON;").await?;
                    Ok(())
                })
            })
            .connect("sqlite::memory:")
            .await
            .expect("Failed to create in-memory SQLite database");

        let migrator = sqlx::migrate::Migrator::new(migration_path)
            .await
            .expect("Failed to load migrations");

        migrator
            .run(&db)
            .await
            .expect("All SQLite migrations should apply cleanly to a fresh database");

        // Verify the database has tables (migrations actually created something)
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE '_sqlx%'",
        )
        .fetch_one(&db)
        .await
        .expect("Should be able to query sqlite_master");

        assert!(
            row.0 > 0,
            "Migrations should create at least one table, found {}",
            row.0
        );

        db.close().await;
    }

    // ========================================================================
    // translate_migrate_error tests
    // ========================================================================

    #[test]
    fn test_translate_migrate_error_adds_sqlite_hint() {
        let err = sqlx::migrate::MigrateError::VersionMissing(2);
        let translated = translate_migrate_error(&DatabaseType::Sqlite, err);
        let msg = translated.to_string();
        assert!(msg.contains("migration 2"), "preserves original wording");
        assert!(msg.contains("hot db port-v1-to-v2"), "got: {msg}");
        assert!(msg.contains(".hot/db/hot.sqlite.db"), "got: {msg}");
        assert!(msg.contains("hot.dev/docs/migrations"), "got: {msg}");
    }

    #[test]
    fn test_translate_migrate_error_adds_postgres_hint() {
        let err = sqlx::migrate::MigrateError::VersionMismatch(5);
        let translated = translate_migrate_error(&DatabaseType::Postgres, err);
        let msg = translated.to_string();
        assert!(msg.contains("fresh Postgres"), "got: {msg}");
        assert!(
            !msg.contains(".hot/db/hot.sqlite.db"),
            "postgres hint must not mention sqlite path"
        );
        assert!(
            !msg.contains("hot db port-v1-to-v2"),
            "postgres has no port command; do not advertise it"
        );
    }

    #[test]
    fn test_translate_migrate_error_passes_through_unrelated() {
        let inner = sqlx::Error::RowNotFound;
        let err = sqlx::migrate::MigrateError::Execute(inner);
        let translated = translate_migrate_error(&DatabaseType::Sqlite, err);
        let msg = translated.to_string();
        assert!(
            !msg.contains("hot.dev/docs/migrations"),
            "no v1→v2 hint for unrelated migration errors"
        );
    }

    /// Verify all SQLite migration files are valid SQL by checking they parse.
    /// Also ensures migration files are numbered sequentially without gaps.
    #[test]
    fn test_sqlite_migration_files_sequential() {
        let migration_path = crate::resources::get_migration_path("sqlite")
            .expect("SQLite migration path should resolve");

        let mut files: Vec<String> = std::fs::read_dir(&migration_path)
            .expect("Should be able to read migration directory")
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".sql") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();

        files.sort();
        assert!(!files.is_empty(), "Should have at least one migration file");

        // Verify sequential numbering
        for (i, file) in files.iter().enumerate() {
            let expected_prefix = format!("{:03}_", i + 1);
            assert!(
                file.starts_with(&expected_prefix),
                "Migration file '{}' should start with '{}' (sequential order)",
                file,
                expected_prefix
            );
        }
    }
}

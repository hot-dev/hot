//! `hot init` and supporting helpers (gitignore setup, profile parsing,
//! auto-init for `hot dev`).

use hot::val::Val;
use tracing::info;

use crate::profile::resolve_profile_to_uuids;

/// Make sure `.gitignore` ignores Hot's local artifacts and secrets, creating
/// the file if it doesn't exist.
pub(crate) fn setup_gitignore(use_logging: bool) -> Result<(), String> {
    use std::fs;
    use std::io::{BufRead, BufReader};

    macro_rules! gitignore_msg {
        ($($arg:tt)*) => {
            if use_logging {
                info!($($arg)*);
            } else {
                println!($($arg)*);
            }
        };
    }

    let gitignore_path = std::path::Path::new(".gitignore");
    // `hot/dev.key` decrypts local context values.
    let hot_entries = [".hot/", "hot/ctx.hot", "hot/dev.key", ".env"];

    if gitignore_path.exists() {
        let file = match fs::File::open(gitignore_path) {
            Ok(file) => file,
            Err(e) => return Err(format!("Failed to read .gitignore: {}", e)),
        };

        let reader = BufReader::new(file);
        let mut existing_lines: Vec<String> = Vec::new();
        for line in reader.lines() {
            match line {
                Ok(line) => existing_lines.push(line),
                Err(e) => return Err(format!("Failed to read .gitignore line: {}", e)),
            }
        }

        let has_entry = |entry: &str| {
            let bare = entry.trim_end_matches('/');
            existing_lines
                .iter()
                .any(|line| line.trim() == entry || line.trim() == bare)
        };
        let entries_added: Vec<&str> = hot_entries
            .iter()
            .copied()
            .filter(|entry| !has_entry(entry))
            .collect();
        existing_lines.extend(entries_added.iter().map(|e| e.to_string()));

        if !entries_added.is_empty() {
            let content = existing_lines.join("\n") + "\n";
            if let Err(e) = fs::write(gitignore_path, content) {
                return Err(format!("Failed to update .gitignore: {}", e));
            }
            gitignore_msg!(
                "Updated .gitignore with Hot entries: {}",
                entries_added.join(", ")
            );
        } else {
            gitignore_msg!(".gitignore already contains Hot entries");
        }
    } else {
        gitignore_msg!("Creating .gitignore file...");
        let content = format!("{}\n", hot_entries.join("\n"));
        if let Err(e) = fs::write(gitignore_path, content) {
            return Err(format!("Failed to create .gitignore: {}", e));
        }
        gitignore_msg!("  Added Hot entries: {}", hot_entries.join(", "));
    }

    Ok(())
}

/// A Hot identifier for a project named after its directory: lowercase
/// letters, digits and `-`, starting with a letter. `2024-app` becomes
/// `app-2024-app`, `my app` and `my.app` become `my-app`.
pub(crate) fn project_identifier(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    match out.chars().next() {
        None => "my-project".to_string(),
        Some(c) if !c.is_ascii_alphabetic() => format!("app-{}", out),
        Some(_) => out,
    }
}

/// Whether `name` is usable as a project name in hot.hot paths and as a
/// namespace segment.
fn is_project_identifier(name: &str) -> bool {
    !name.is_empty() && project_identifier(name) == name
}

/// Check if initialization is needed and run it automatically in development mode.
/// Returns `Ok(true)` if init was run, `Ok(false)` if no init was needed.
pub(crate) async fn check_and_run_init_if_needed(
    conf: &Val,
    providers: &crate::CliProviders,
) -> Result<bool, String> {
    use hot::db::{check_default_data_exists, create_db_pool};

    let db = match create_db_pool(conf).await {
        Ok(db) => db.with_scope(hot::db::DatabaseScope::Bootstrap(
            hot::db::BootstrapOperation::Onboarding,
        )),
        // Only a database that doesn't exist yet calls for initialization.
        // Any other failure (an unreachable server, rejected credentials, a
        // runtime role check) is the error to report.
        Err(hot::db::DatabaseError::NotInitialized(_)) => {
            info!("Database not initialized, running initialization...");
            run_init_impl(conf, true, None, None, providers).await?;
            return Ok(true);
        }
        Err(e) => return Err(format!("Cannot connect to the database: {e}")),
    };

    match check_default_data_exists(&db).await {
        Ok((org_count, user_count)) => {
            if org_count == 0 || user_count == 0 {
                info!("No users or organizations found, running initialization...");
                drop(db);
                run_init_impl(conf, true, None, None, providers).await?;
                return Ok(true);
            }
            Ok(false)
        }
        Err(_) => {
            info!("Error checking database state, running initialization...");
            drop(db);
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            run_init_impl(conf, true, None, None, providers).await?;
            Ok(true)
        }
    }
}

pub(crate) async fn run_init(
    conf: &Val,
    path: Option<&str>,
    name: Option<&str>,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    if let Some(name) = name
        && !is_project_identifier(name)
    {
        return Err(format!(
            "'{}' is not a valid project name: use lowercase letters, digits and '-', \
             starting with a letter (for example '{}')",
            name,
            project_identifier(name)
        ));
    }
    run_init_impl(conf, false, path, name, providers).await
}

/// Run initialization.
/// - `use_logging`: if true, output goes to `info!` logs (for `hot dev`); if
///   false, output goes to `println!` (for `hot init`).
/// - `path`: optional target directory. If provided, creates the dir if needed
///   and sets it as cwd.
async fn run_init_impl(
    conf: &Val,
    use_logging: bool,
    path: Option<&str>,
    name: Option<&str>,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    use std::fs;

    macro_rules! init_msg {
        ($($arg:tt)*) => {
            if use_logging {
                info!($($arg)*);
            } else {
                println!($($arg)*);
            }
        };
    }

    if let Some(target) = path {
        let target_path = std::path::Path::new(target);
        if !target_path.exists() {
            fs::create_dir_all(target_path)
                .map_err(|e| format!("Failed to create directory '{}': {}", target, e))?;
        } else if !target_path.is_dir() {
            return Err(format!("'{}' exists but is not a directory", target));
        }
        std::env::set_current_dir(target_path)
            .map_err(|e| format!("Failed to change to directory '{}': {}", target, e))?;
    }

    init_msg!("Initializing Hot application...");

    let hot_dir = std::path::Path::new(".hot");
    if !hot_dir.exists() {
        init_msg!("Creating .hot directory...");
        if let Err(e) = fs::create_dir_all(hot_dir) {
            return Err(format!("Failed to create .hot directory: {}", e));
        }
    } else {
        init_msg!(".hot directory already exists");
    }

    setup_gitignore(use_logging)?;

    let db_dir = std::path::Path::new(".hot/db");
    if let Err(e) = fs::create_dir_all(db_dir) {
        return Err(format!("Failed to create .hot/db directory: {}", e));
    }

    let (profile_email, profile_slug, profile_env_name) = initialize_database(
        conf,
        if use_logging {
            InitOutput::Log
        } else {
            InitOutput::Print
        },
        providers,
    )
    .await?;

    let project_name = match name {
        Some(name) => name.to_string(),
        None => {
            let dir_name = std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "my-project".to_string());
            let project_name = project_identifier(&dir_name);
            if project_name != dir_name && !std::path::Path::new("hot.hot").exists() {
                init_msg!(
                    "Project name: {} (from directory '{}'; pass --name to choose another)",
                    project_name,
                    dir_name
                );
            }
            project_name
        }
    };

    let conf_file = std::path::Path::new("hot.hot");
    let is_existing_project = conf_file.exists();
    if !is_existing_project {
        init_msg!("Creating hot.hot configuration file...");

        let template = hot::resources::read_init_template("hot.hot.minimal.template")?;

        let conf_content = template
            .replace("{{PROJECT_NAME}}", &project_name)
            .replace("{{USER_EMAIL}}", &profile_email)
            .replace("{{ORG_SLUG}}", &profile_slug)
            .replace("{{ENV_NAME}}", &profile_env_name);

        if let Err(e) = fs::write(conf_file, conf_content) {
            return Err(format!("Failed to write hot.hot: {}", e));
        }
        init_msg!("  Created hot.hot from template with environment variable patterns");
    } else {
        init_msg!("hot.hot already exists");
    }

    let env_example_file = std::path::Path::new(".env.example");
    if !env_example_file.exists() {
        init_msg!("Creating .env.example file...");
        let env_example_template = hot::resources::read_init_template("env.example.template")?;
        if let Err(e) = fs::write(env_example_file, env_example_template) {
            return Err(format!("Failed to write .env.example: {}", e));
        }
        init_msg!("  Created .env.example");
    } else {
        init_msg!(".env.example already exists");
    }

    if !is_existing_project {
        let src_dir = std::path::Path::new("hot/src").join(&project_name);
        let hi_file = src_dir.join("hi.hot");
        if !hi_file.exists() {
            init_msg!("Creating starter file...");

            if let Err(e) = fs::create_dir_all(&src_dir) {
                return Err(format!("Failed to create source directory: {}", e));
            }

            let hi_template = hot::resources::read_init_template("hi.hot.template")?;
            let hi_content = hi_template.replace("{{PROJECT_NAME}}", &project_name);

            if let Err(e) = fs::write(&hi_file, hi_content) {
                return Err(format!("Failed to write hi.hot: {}", e));
            }
            init_msg!("  Created hot/src/{}/hi.hot", project_name);
        } else {
            init_msg!("hot/src/{}/hi.hot already exists", project_name);
        }

        let test_dir = std::path::Path::new("hot/test").join(&project_name);
        if !test_dir.exists() {
            if let Err(e) = fs::create_dir_all(&test_dir) {
                return Err(format!("Failed to create test directory: {}", e));
            }
            init_msg!("  Created hot/test/{}/", project_name);
        }
    }

    init_msg!("Hot application initialized successfully!");

    if !use_logging {
        println!("\nStart the dev server:");
        println!("  hot dev");
        println!("\nTo add AI coding support (AGENTS.md + skills):");
        println!("  hot ai add               # Add to this project");
        println!("  hot ai add --global      # Install skills globally (~/.skills/)");
    }

    Ok(())
}

/// Where `initialize_database` reports its progress.
#[derive(Clone, Copy)]
enum InitOutput {
    /// `hot init`: printed.
    Print,
    /// `hot dev`'s automatic init: logged.
    Log,
    /// A command preparing its database on first use: debug logs only, so
    /// stdout stays the command's own (`KEY=$(hot key create)`).
    Quiet,
}

/// The database part of `hot init`: run the migrations, then make sure the
/// selected profile's organization, user and environment exist.
async fn initialize_database(
    conf: &Val,
    output: InitOutput,
    providers: &crate::CliProviders,
) -> Result<hot::profile::ProfileIdentifiers, String> {
    use hot::db::{create_db_pool, get_db_uri_from_conf, redact_password};

    macro_rules! init_msg {
        ($($arg:tt)*) => {
            match output {
                InitOutput::Print => println!($($arg)*),
                InitOutput::Log => info!($($arg)*),
                InitOutput::Quiet => tracing::debug!($($arg)*),
            }
        };
    }

    init_msg!("Running database migrations...");
    init_msg!(
        "  Database URI: {}",
        redact_password(&get_db_uri_from_conf(conf))
    );

    match crate::run_migrations_with_bootstrap(conf, providers).await {
        Ok(_) => init_msg!("  Migrations completed successfully"),
        Err(e) => {
            crate::report_migration_failure("Migration failed", &e);
            return Err("Migration failed".to_string());
        }
    }

    init_msg!("Checking for default organization, user, and environment...");

    let db = match create_db_pool(conf).await {
        Ok(db) => db.with_scope(hot::db::DatabaseScope::Bootstrap(
            hot::db::BootstrapOperation::Onboarding,
        )),
        Err(e) => return Err(format!("Failed to connect to database: {}", e)),
    };

    let identity = initialize_profile(&db, conf).await?;
    init_msg!(
        "  Using authorized profile - User: {}, Org: {}, Env: {}",
        identity.0,
        identity.1,
        identity.2
    );
    Ok(identity)
}

/// Prepare a project's local SQLite database the first time a command needs
/// it, as `hot init` would, so `hot run` works in a fresh checkout and after
/// an upgrade: create a missing database, migrate an outdated one, and give an
/// empty one the local identity. Only a project's local SQLite file
/// qualifies. PostgreSQL is never migrated implicitly, and authorization or
/// profile errors still fail.
pub(crate) async fn prepare_local_database(
    conf: &Val,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    prepare_local_database_in(
        conf,
        hot::lang::cache::paths::has_project_config(),
        providers,
    )
    .await
}

async fn prepare_local_database_in(
    conf: &Val,
    in_project: bool,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    let Some(path) = local_sqlite_database(conf, in_project) else {
        return Ok(());
    };
    if !path.exists() {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
        }
        eprintln!("Creating the project database at {}", path.display());
        initialize_database(conf, InitOutput::Quiet, providers).await?;
        return Ok(());
    }
    let pending = hot::db::pending_sqlite_migrations(&hot::db::get_db_uri_from_conf(conf))
        .await
        .map_err(|e| {
            format!(
                "Cannot read the project database at {}: {e}",
                path.display()
            )
        })?;
    if !pending.is_empty() {
        eprintln!(
            "Migrating the project database at {} ({} pending migration{})",
            path.display(),
            pending.len(),
            if pending.len() == 1 { "" } else { "s" }
        );
        initialize_database(conf, InitOutput::Quiet, providers).await?;
        return Ok(());
    }
    seed_empty_database(conf).await
}

/// Give a migrated database with no organizations or users the local
/// identity, as `hot init` does. A database migrated by `hot db migrate`
/// alone has none yet. A database with any data is left alone.
pub(crate) async fn seed_empty_database(conf: &Val) -> Result<(), String> {
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to database: {e}"))?
        .with_scope(hot::db::DatabaseScope::Bootstrap(
            hot::db::BootstrapOperation::Onboarding,
        ));
    let (org_count, user_count) = hot::db::check_default_data_exists(&db)
        .await
        .map_err(|e| format!("Failed to check existing data: {e}"))?;
    if org_count == 0 && user_count == 0 {
        initialize_profile(&db, conf).await?;
    }
    Ok(())
}

/// The configured database file to create on first use: a local SQLite file,
/// inside a project, that doesn't exist yet. The path is read as
/// `hot::db::create_db_pool` reads it.
#[cfg(test)]
fn missing_local_sqlite_database(conf: &Val, in_project: bool) -> Option<std::path::PathBuf> {
    local_sqlite_database(conf, in_project).filter(|path| !path.exists())
}

/// The project's local SQLite database file, whether or not it exists yet.
fn local_sqlite_database(conf: &Val, in_project: bool) -> Option<std::path::PathBuf> {
    if !in_project {
        return None;
    }
    let uri = hot::db::get_db_uri_from_conf(conf);
    let path = uri.strip_prefix("sqlite:")?;
    let path = path.split('?').next().unwrap_or(path);
    if path.is_empty() || path.contains(":memory:") {
        return None;
    }
    Some(std::path::PathBuf::from(path))
}

/// Initialization may seed the documented local development identity into an
/// empty database. Every other profile must already have its exact membership
/// and environment; init never grants access to an existing organization.
async fn initialize_profile(
    db: &hot::db::DatabasePool,
    conf: &Val,
) -> Result<hot::profile::ProfileIdentifiers, String> {
    let identity = crate::profile::extract_profile_identifiers(conf)?
        .unwrap_or_else(|| ("local@hot.dev".into(), "local".into(), "development".into()));
    let (org_count, user_count) = hot::db::check_default_data_exists(db)
        .await
        .map_err(|e| format!("Failed to check existing data: {e}"))?;
    if org_count == 0 && user_count == 0 {
        if identity != ("local@hot.dev".into(), "local".into(), "development".into()) {
            return Err("The selected profile must be provisioned before initialization. An empty database can bootstrap only the local-dev identity (local@hot.dev / local / development); select it explicitly or provision the selected account first".into());
        }
        hot::db::insert_default_data(db)
            .await
            .map_err(|e| format!("Failed to insert local development data: {e}"))?;
    }
    resolve_profile_to_uuids(db, &identity.0, &identity.1, &identity.2)
        .await
        .map_err(|e| format!("Profile validation failed: {e}"))?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::db::{Org, User, check_default_data_exists};
    use uuid::Uuid;

    #[test]
    fn project_identifier_makes_a_hot_identifier() {
        assert_eq!(project_identifier("my-app"), "my-app");
        assert_eq!(project_identifier("2024-app"), "app-2024-app");
        assert_eq!(project_identifier("my app"), "my-app");
        assert_eq!(project_identifier("my.app"), "my-app");
        assert_eq!(project_identifier("My_App!"), "my-app");
        assert_eq!(project_identifier("..."), "my-project");
        assert!(is_project_identifier("app-2"));
        assert!(!is_project_identifier("2app"));
        assert!(!is_project_identifier("my app"));
    }

    fn profile(email: &str, org: &str, env: &str) -> Val {
        hot::val!({"set":{"profile":"selected"},"profile":{"selected":{
            "user":{"email":email},"org":{"slug":org},"env":{"name":env}
        }}})
    }

    fn sqlite_conf(conf: Val, path: &std::path::Path) -> Val {
        conf.merge(&hot::val!({"db": {"uri": format!("sqlite:{}", path.display())}}))
    }

    #[test]
    fn only_a_missing_local_sqlite_file_in_a_project_is_created_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(".hot/db/hot.sqlite.db");
        let conf = sqlite_conf(hot::val!({}), &missing);
        assert_eq!(missing_local_sqlite_database(&conf, true), Some(missing));
        assert_eq!(
            missing_local_sqlite_database(&conf, false),
            None,
            "outside a project"
        );
        let existing = dir.path().join("existing.db");
        std::fs::write(&existing, b"").unwrap();
        let conf = sqlite_conf(hot::val!({}), &existing);
        assert_eq!(
            missing_local_sqlite_database(&conf, true),
            None,
            "an existing file"
        );
        for uri in ["sqlite::memory:", "postgres://127.0.0.1:1/hot"] {
            let conf = hot::val!({"db": {"uri": uri}});
            assert_eq!(missing_local_sqlite_database(&conf, true), None, "{uri}");
        }
    }

    #[tokio::test]
    async fn first_use_creates_the_database_and_the_local_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".hot/db/hot.sqlite.db");
        let conf = sqlite_conf(hot::profile::get_resolved_conf(hot::val!({})), &path);
        let providers = crate::CliProviders::default();
        prepare_local_database_in(&conf, true, &providers)
            .await
            .unwrap();
        assert!(path.exists(), "the database file is created");
        let db = crate::profile::create_cli_db_pool(&conf).await.unwrap();
        crate::profile::resolve_cli_identity(&db, &conf)
            .await
            .expect("the local profile resolves, as commands resolve it");

        // A second run leaves the existing database alone.
        prepare_local_database_in(&conf, true, &providers)
            .await
            .unwrap();
        let bootstrap = db.with_scope(hot::db::DatabaseScope::Bootstrap(
            hot::db::BootstrapOperation::Onboarding,
        ));
        assert_eq!(check_default_data_exists(&bootstrap).await.unwrap(), (1, 1));
    }

    #[tokio::test]
    async fn first_use_still_fails_for_an_unprovisioned_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".hot/db/hot.sqlite.db");
        let conf = sqlite_conf(
            profile("custom@example.test", "custom", "production"),
            &path,
        );
        let error = prepare_local_database_in(&conf, true, &crate::CliProviders::default())
            .await
            .unwrap_err();
        assert!(error.contains("must be provisioned"), "{error}");
    }

    #[tokio::test]
    async fn init_honors_loaded_profile_and_never_seeds_a_different_identity() {
        let db = hot::db::test_db().await;
        let custom = profile("custom@example.test", "custom", "production");
        assert!(
            initialize_profile(&db, &custom)
                .await
                .unwrap_err()
                .contains("must be provisioned")
        );
        assert_eq!(check_default_data_exists(&db).await.unwrap(), (0, 0));
        let local = hot::profile::get_resolved_conf(hot::val!({}));
        assert_eq!(initialize_profile(&db, &local).await.unwrap().1, "local");
        assert!(initialize_profile(&db, &custom).await.is_err());
        assert_eq!(check_default_data_exists(&db).await.unwrap(), (1, 1));
    }

    #[tokio::test]
    async fn init_requires_existing_membership_for_the_selected_profile() {
        let db = hot::db::test_db().await;
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let user = User::get_user(&db, &fixture.user_id).await.unwrap();
        let outsider = Uuid::now_v7();
        User::insert_user(
            &db,
            &outsider,
            "outsider@example.test",
            None,
            Some(&outsider),
        )
        .await
        .unwrap();
        let org = Org::get_org_uncached(&db, &fixture.org_id).await.unwrap();
        let selected = profile("outsider@example.test", &org.slug, "development");
        assert!(initialize_profile(&db, &selected).await.is_err());
        assert!(
            Org::resolve_user_access(&db, &org.org_id, &outsider)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            initialize_profile(&db, &profile(&user.email, &org.slug, "development"))
                .await
                .unwrap()
                .0,
            user.email
        );
        // Existing membership is retained, and a missing environment is never
        // silently replaced by the organization's default.
        assert!(
            initialize_profile(&db, &profile(&user.email, &org.slug, "missing"))
                .await
                .is_err()
        );
    }
}

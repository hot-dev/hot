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
    let hot_entries = [".hot/", "hot/ctx.hot", ".env"];

    if gitignore_path.exists() {
        let file = match fs::File::open(gitignore_path) {
            Ok(file) => file,
            Err(e) => return Err(format!("Failed to read .gitignore: {}", e)),
        };

        let reader = BufReader::new(file);
        let mut existing_lines: Vec<String> = Vec::new();
        let mut has_hot_dir = false;
        let mut has_ctx_hot = false;
        let mut has_env = false;

        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(e) => return Err(format!("Failed to read .gitignore line: {}", e)),
            };
            existing_lines.push(line.clone());

            let trimmed = line.trim();
            if trimmed == ".hot/" || trimmed == ".hot" {
                has_hot_dir = true;
            }
            if trimmed == "hot/ctx.hot" {
                has_ctx_hot = true;
            }
            if trimmed == ".env" {
                has_env = true;
            }
        }

        let mut entries_added = Vec::new();
        if !has_hot_dir {
            existing_lines.push(".hot/".to_string());
            entries_added.push(".hot/");
        }
        if !has_ctx_hot {
            existing_lines.push("hot/ctx.hot".to_string());
            entries_added.push("hot/ctx.hot");
        }
        if !has_env {
            existing_lines.push(".env".to_string());
            entries_added.push(".env");
        }

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
        Err(_) => {
            info!("Database connection failed, running initialization...");
            run_init_impl(conf, true, None, providers).await?;
            return Ok(true);
        }
    };

    match check_default_data_exists(&db).await {
        Ok((org_count, user_count)) => {
            if org_count == 0 || user_count == 0 {
                info!("No users or organizations found, running initialization...");
                drop(db);
                run_init_impl(conf, true, None, providers).await?;
                return Ok(true);
            }
            Ok(false)
        }
        Err(_) => {
            info!("Error checking database state, running initialization...");
            drop(db);
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            run_init_impl(conf, true, None, providers).await?;
            Ok(true)
        }
    }
}

pub(crate) async fn run_init(
    conf: &Val,
    path: Option<&str>,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    run_init_impl(conf, false, path, providers).await
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
    providers: &crate::CliProviders,
) -> Result<(), String> {
    use hot::db::{create_db_pool, get_db_uri_from_conf, redact_password};
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

    let (profile_email, profile_slug, profile_env_name) = initialize_profile(&db, conf).await?;
    init_msg!(
        "  Using authorized profile - User: {}, Org: {}, Env: {}",
        profile_email,
        profile_slug,
        profile_env_name
    );

    let project_name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "my-project".to_string());

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

    fn profile(email: &str, org: &str, env: &str) -> Val {
        hot::val!({"set":{"profile":"selected"},"profile":{"selected":{
            "user":{"email":email},"org":{"slug":org},"env":{"name":env}
        }}})
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

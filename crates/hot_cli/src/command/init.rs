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
        Ok(db) => db,
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
    use hot::db::{
        Env, Org, User, check_default_data_exists, create_db_pool, get_db_uri_from_conf,
        insert_default_data, redact_password,
    };
    use std::fs;
    use uuid;

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
        Ok(db) => db,
        Err(e) => return Err(format!("Failed to connect to database: {}", e)),
    };

    let (org_count, user_count) = match check_default_data_exists(&db).await {
        Ok((org_count, user_count)) => (org_count, user_count),
        Err(e) => return Err(format!("Failed to check existing data: {}", e)),
    };

    let existing_profile = parse_existing_conf_profile();

    let profile_email = existing_profile
        .as_ref()
        .and_then(|p| p.user_email.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("local@hot.dev");
    let profile_slug = existing_profile
        .as_ref()
        .and_then(|p| p.org_slug.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("local");
    let profile_env_name = existing_profile
        .as_ref()
        .and_then(|p| p.env_name.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("development");

    if org_count == 0 && user_count == 0 {
        init_msg!("  No organizations or users found, creating defaults...");

        let (org_id, user_id, _org_user_id) = match insert_default_data(&db).await {
            Ok(ids) => ids,
            Err(e) => return Err(format!("Failed to insert default data: {}", e)),
        };

        let default_env = match Env::get_default_env_by_org(&db, &org_id).await {
            Ok(env) => env,
            Err(e) => return Err(format!("Failed to get default environment: {}", e)),
        };

        init_msg!("  Created default organization: Local ({})", org_id);
        init_msg!("  Created default user: local@hot.dev ({})", user_id);
        init_msg!(
            "  Created default environment: development ({})",
            default_env.env_id
        );
        init_msg!("  Linked user to organization");
    } else {
        let env_count = match Env::get_count(&db).await {
            Ok(count) => count,
            Err(e) => return Err(format!("Failed to check environment count: {}", e)),
        };

        if env_count == 0 {
            init_msg!(
                "  Found existing organizations and users, but no environments. Creating default environment..."
            );

            let org = match Org::get_org_by_slug(&db, profile_slug).await {
                Ok(org) => org,
                Err(_) => return Err(format!("Organization '{}' not found", profile_slug)),
            };

            let user = match User::get_user_by_email(&db, profile_email).await {
                Ok(user) => user,
                Err(_) => return Err(format!("User '{}' not found", profile_email)),
            };

            let env_id = uuid::Uuid::now_v7();
            match Env::insert_env(&db, &env_id, &org.org_id, profile_env_name, &user.user_id).await
            {
                Ok(_) => {
                    init_msg!(
                        "  Created default environment: {} ({})",
                        profile_env_name,
                        env_id
                    );
                }
                Err(e) => return Err(format!("Failed to create default environment: {}", e)),
            }
        }

        init_msg!(
            "  Found existing organizations ({}) and users ({}), validating profile configuration...",
            org_count,
            user_count
        );

        if let Err(e) =
            resolve_profile_to_uuids(&db, profile_email, profile_slug, profile_env_name).await
        {
            return Err(format!("Profile validation failed: {}", e));
        }

        init_msg!(
            "  Using existing data - User: {}, Org: {}, Env: {}",
            profile_email,
            profile_slug,
            profile_env_name
        );
    }

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
            .replace("{{USER_EMAIL}}", profile_email)
            .replace("{{ORG_SLUG}}", profile_slug)
            .replace("{{ENV_NAME}}", profile_env_name);

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

/// Profile information parsed from an existing `hot.hot` file (best-effort).
#[derive(Debug)]
struct ProfileInfo {
    #[allow(dead_code)]
    user_id: Option<String>,
    user_email: Option<String>,
    #[allow(dead_code)]
    org_id: Option<String>,
    org_slug: Option<String>,
    #[allow(dead_code)]
    env_id: Option<String>,
    env_name: Option<String>,
}

/// Best-effort parse of an existing `hot.hot` file's `profile.local-dev`
/// section so `hot init` can re-use the user's existing identity instead of
/// silently overwriting it with defaults.
fn parse_existing_conf_profile() -> Option<ProfileInfo> {
    use std::fs;

    let conf_file = std::path::Path::new("hot.hot");
    if !conf_file.exists() {
        return None;
    }

    let content = match fs::read_to_string(conf_file) {
        Ok(content) => content,
        Err(_) => return None,
    };

    let mut profile = ProfileInfo {
        user_id: None,
        user_email: None,
        org_id: None,
        org_slug: None,
        env_id: None,
        env_name: None,
    };

    if let Some(user_start) = content.find(r#""user": {"#) {
        let user_section = &content[user_start..];
        if let Some(id_start) = user_section.find(r#""id": ""#) {
            let id_start = id_start + 7;
            if let Some(id_end) = user_section[id_start..].find('"') {
                profile.user_id = Some(user_section[id_start..id_start + id_end].to_string());
            }
        }
        if let Some(email_start) = user_section.find(r#""email": ""#) {
            let email_start = email_start + 10;
            if let Some(email_end) = user_section[email_start..].find('"') {
                profile.user_email =
                    Some(user_section[email_start..email_start + email_end].to_string());
            }
        }
    }

    if let Some(org_start) = content.find(r#""org": {"#) {
        let org_section = &content[org_start..];
        if let Some(id_start) = org_section.find(r#""id": ""#) {
            let id_start = id_start + 7;
            if let Some(id_end) = org_section[id_start..].find('"') {
                profile.org_id = Some(org_section[id_start..id_start + id_end].to_string());
            }
        }
        if let Some(slug_start) = org_section.find(r#""slug": ""#) {
            let slug_start = slug_start + 9;
            if let Some(slug_end) = org_section[slug_start..].find('"') {
                profile.org_slug = Some(org_section[slug_start..slug_start + slug_end].to_string());
            }
        }
    }

    if let Some(env_start) = content.find(r#""env": {"#) {
        let env_section = &content[env_start..];
        if let Some(id_start) = env_section.find(r#""id": ""#) {
            let id_start = id_start + 7;
            if let Some(id_end) = env_section[id_start..].find('"') {
                profile.env_id = Some(env_section[id_start..id_start + id_end].to_string());
            }
        }
        if let Some(name_start) = env_section.find(r#""name": ""#) {
            let name_start = name_start + 9;
            if let Some(name_end) = env_section[name_start..].find('"') {
                profile.env_name = Some(env_section[name_start..name_start + name_end].to_string());
            }
        }
    }

    Some(profile)
}

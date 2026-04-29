//! `hot context` — manage encrypted per-project context variables (local-only).

use hot::val::Val;

use crate::cli::{ContextAction, GlobalOptions};

pub(crate) async fn run_context(
    action: &ContextAction,
    conf: &Val,
    global: &GlobalOptions,
    local: bool,
) -> Result<(), String> {
    if !local {
        return Err(
            "Remote context management via API is not yet implemented.\n\
             Use --local to manage context variables through direct database access."
                .to_string(),
        );
    }

    // Local context management implementation
    use hot::context_encryption::ContextEncryption;
    use hot::db::{Context, Env, Org, Project, create_db_pool};

    // Load encryption key (auto-generate for local dev if not configured)
    // For CLI, we default to "local-dev" profile to make development easier
    let encryption = ContextEncryption::from_env_or_generate_for_dev("local-dev").map_err(|e| {
        format!(
            "Failed to load encryption key: {}. Set HOT_ENCRYPTION_KEY environment variable for production.",
            e
        )
    })?;

    // Get database connection
    let db = create_db_pool(conf)
        .await
        .map_err(|e| format!("Database connection failed: {}", e))?;

    // Get default org and env for project lookup
    let count = Org::get_count(&db)
        .await
        .map_err(|e| format!("Failed to get org count: {}", e))?;
    if count == 0 {
        return Err("No organization found. Please run 'hot init' first.".to_string());
    }

    let orgs = Org::get_all_orgs(&db, Some(1), Some(0))
        .await
        .map_err(|e| format!("Failed to get organization: {}", e))?;

    let org = orgs
        .into_iter()
        .next()
        .ok_or_else(|| "No organization found".to_string())?;

    let env = Env::get_default_env_by_org(&db, &org.org_id)
        .await
        .map_err(|e| format!("Failed to get environment: {}", e))?;

    let org_id = org.org_id;

    // Get project name from global options or default from config
    let default_project_name = hot::project::get_default_project_name(conf);
    let project_name = global.project.as_deref().unwrap_or(&default_project_name);

    // Get a user_id for creating/updating context variables
    // For CLI operations, we use the first user in the org
    let users = hot::db::org::OrgUser::get_users_with_roles_by_org(&db, &org_id)
        .await
        .map_err(|e| format!("Failed to get users: {}", e))?;
    let user_id = users
        .first()
        .ok_or_else(|| "No users found in organization".to_string())?
        .user_id;

    match action {
        ContextAction::List => {
            let proj = Project::get_project_by_env_and_name(&db, &env.env_id, project_name)
                .await
                .map_err(|e| format!("Failed to get project '{}': {}", project_name, e))?
                .ok_or_else(|| {
                    format!(
                        "Project '{}' not found in environment '{}'",
                        project_name, env.name
                    )
                })?;

            let contexts = Context::get_by_project(&db, &proj.project_id)
                .await
                .map_err(|e| format!("Failed to list context variables: {}", e))?;

            if contexts.is_empty() {
                println!("No context variables found for project '{}'", project_name);
            } else {
                println!("Context variables for project '{}':", project_name);
                println!();
                for ctx in contexts {
                    if let Some(desc) = &ctx.description {
                        println!("  {} - {}", ctx.key, desc);
                    } else {
                        println!("  {}", ctx.key);
                    }
                }
            }
        }
        ContextAction::Get { key } => {
            let proj = Project::get_project_by_env_and_name(&db, &env.env_id, project_name)
                .await
                .map_err(|e| format!("Failed to get project '{}': {}", project_name, e))?
                .ok_or_else(|| {
                    format!(
                        "Project '{}' not found in environment '{}'",
                        project_name, env.name
                    )
                })?;

            let ctx = Context::get_by_project_and_key(&db, &proj.project_id, key)
                .await
                .map_err(|e| format!("Failed to get context variable: {}", e))?
                .ok_or_else(|| format!("Context variable '{}' not found", key))?;

            let decrypted_val = ctx
                .get_decrypted_value(&encryption, &org_id)
                .map_err(|e| format!("Failed to decrypt value: {}", e))?;

            println!("{}", decrypted_val.pretty_print());
        }
        ContextAction::Set {
            key,
            value,
            description,
        } => {
            let proj = Project::get_project_by_env_and_name(&db, &env.env_id, project_name)
                .await
                .map_err(|e| format!("Failed to get project '{}': {}", project_name, e))?
                .ok_or_else(|| {
                    format!(
                        "Project '{}' not found in environment '{}'",
                        project_name, env.name
                    )
                })?;

            // Validate that the value is valid Hot code
            let validated_val = Context::validate_hot_value(value)
                .map_err(|e| format!("Invalid Hot code: {}", e))?;

            // Encrypt the value
            let encrypted_value = Context::set_value_from_val(&validated_val, &encryption, &org_id)
                .map_err(|e| format!("Failed to encrypt value: {}", e))?;

            // Check if context variable already exists
            let existing = Context::get_by_project_and_key(&db, &proj.project_id, key)
                .await
                .map_err(|e| format!("Failed to check existing context variable: {}", e))?;

            if let Some(existing_ctx) = existing {
                // Update existing
                Context::update(
                    &db,
                    &existing_ctx.context_id,
                    &encrypted_value,
                    description.as_deref(),
                    &user_id,
                )
                .await
                .map_err(|e| format!("Failed to update context variable: {}", e))?;

                println!(
                    "Updated context variable '{}' for project '{}'",
                    key, project_name
                );
            } else {
                // Create new
                #[allow(deprecated)]
                Context::insert(
                    &db,
                    &uuid::Uuid::now_v7(),
                    &proj.project_id,
                    key,
                    &encrypted_value,
                    description.as_deref(),
                    &user_id,
                )
                .await
                .map_err(|e| format!("Failed to create context variable: {}", e))?;

                println!(
                    "Created context variable '{}' for project '{}'",
                    key, project_name
                );
            }
        }
        ContextAction::Required {
            project_name: project_arg,
        } => {
            let target_project = project_arg.as_deref().unwrap_or(project_name);
            run_context_required(conf, &db, &org_id, &env, target_project).await?;
        }
        ContextAction::Delete { key } => {
            let proj = Project::get_project_by_env_and_name(&db, &env.env_id, project_name)
                .await
                .map_err(|e| format!("Failed to get project '{}': {}", project_name, e))?
                .ok_or_else(|| {
                    format!(
                        "Project '{}' not found in environment '{}'",
                        project_name, env.name
                    )
                })?;

            let ctx = Context::get_by_project_and_key(&db, &proj.project_id, key)
                .await
                .map_err(|e| format!("Failed to get context variable: {}", e))?
                .ok_or_else(|| format!("Context variable '{}' not found", key))?;

            Context::delete(&db, &ctx.context_id)
                .await
                .map_err(|e| format!("Failed to delete context variable: {}", e))?;

            println!(
                "Deleted context variable '{}' from project '{}'",
                key, project_name
            );
        }
    }

    Ok(())
}

/// Implementation of `hot context required [<project>]`.
///
/// Builds the project's call graph from source (no DB writes), resolves
/// the set of required ctx keys reachable from user code, and cross-checks
/// against the env-level + project-level ctx vars currently set in the
/// local DB. Prints a single table with status per key plus the exact
/// `hot ctx set` invocation for any unset keys.
async fn run_context_required(
    conf: &Val,
    db: &hot::db::DatabasePool,
    org_id: &uuid::Uuid,
    env: &hot::db::Env,
    project_name: &str,
) -> Result<(), String> {
    use hot::context_encryption::ContextEncryption;
    use hot::db::Project;

    // Resolve src paths from project conf, falling back to "hot/src".
    let src_paths = {
        let paths = hot::project::get_project_src_paths(conf, project_name);
        if paths.is_empty() {
            vec!["hot/src".to_string()]
        } else {
            paths
        }
    };

    // Build call graph and resolve user ctx requirements. This is the same
    // path the bundler uses, so the answer matches what `hot deploy` would
    // gate on.
    let extracted = hot::lang::engine::Engine::extract_handlers_and_scheduled_functions(
        &src_paths,
        Some(project_name),
        Some(conf),
        false,
    )
    .map_err(|e| format!("Compilation failed: {}", e))?;

    let requirements = extracted.ctx_requirements;

    // Flatten to a single ordered list of (key, declared_by, source_file).
    let mut entries: Vec<(String, String, Option<String>)> = Vec::new();
    let mut seen: ahash::AHashSet<String> = ahash::AHashSet::new();
    for ns in &requirements.namespaces {
        for k in ns.required_keys() {
            if seen.insert(k.key.clone()) {
                entries.push((k.key.clone(), ns.namespace.clone(), ns.source_file.clone()));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Look up the project (if it exists in the DB) so we can read its
    // project-level ctx vars. If there's no project record yet (fresh
    // checkout, never deployed) we still print the requirements with all
    // env-level keys cross-referenced.
    let project = Project::get_project_by_env_and_name(db, &env.env_id, project_name)
        .await
        .map_err(|e| format!("Failed to look up project '{}': {}", project_name, e))?;

    let encryption = ContextEncryption::from_env_or_existing_dev_key().ok();

    let available_keys = if let Some(project) = &project {
        hot::build::load_available_ctx_keys(
            db,
            &env.env_id,
            &project.project_id,
            org_id,
            encryption.as_ref(),
        )
        .await?
    } else {
        // No project record yet — only env-level ctx vars exist.
        let env_ctx = hot::db::Context::get_by_env(db, &env.env_id)
            .await
            .map_err(|e| format!("Failed to load env-level context variables: {}", e))?;
        let mut keys = ahash::AHashSet::new();
        for cv in &env_ctx {
            if !cv.active {
                continue;
            }
            if let Some(enc) = &encryption {
                if cv.get_decrypted_value(enc, org_id).is_ok() {
                    keys.insert(cv.key.clone());
                }
            } else {
                keys.insert(cv.key.clone());
            }
        }
        keys
    };

    if entries.is_empty() {
        println!(
            "No required context variables reachable from project '{}'.",
            project_name
        );
        return Ok(());
    }

    println!(
        "Required ctx vars reachable from your code (project '{}'):\n",
        project_name
    );

    let key_col_width = entries.iter().map(|(k, _, _)| k.len()).max().unwrap_or(0);
    let key_col_width = key_col_width.max(20);

    let mut unset_count = 0usize;
    for (key, declared_by, _src) in &entries {
        let (status, is_set) = if available_keys.contains(key) {
            ("SET  ", true)
        } else {
            unset_count += 1;
            ("UNSET", false)
        };
        println!(
            "  {}  {:<width$}  (required by {})",
            status,
            key,
            declared_by,
            width = key_col_width
        );
        if !is_set {
            println!("         hot ctx set {} <value>", key,);
        }
    }

    println!();
    let strict_default = conf.get_bool("deploy.ctx.strict");
    if unset_count == 0 {
        println!("Deploy will: succeed");
    } else if strict_default {
        println!(
            "Deploy will: BLOCK on {} unset key(s) (hot.deploy.ctx.strict is true)",
            unset_count
        );
    } else {
        println!(
            "Deploy will: succeed with warning ({} unset key(s); --strict would block)",
            unset_count
        );
    }

    Ok(())
}

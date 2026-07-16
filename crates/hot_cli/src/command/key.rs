//! `hot key` — API key management against the local environment database.
//!
//! API keys are root credentials: the Hot API deliberately has no endpoint to
//! create them, and hot-cloud keys are created in the dashboard. This command
//! writes directly to the database the current configuration points at
//! (`hot.db.uri` — the project's embedded SQLite by default), which makes it
//! useful anywhere the operator has direct database access: local development,
//! CI against a scratch `hot dev`, and self-hosted deployments.

use hot::val::Val;

/// Create a full-access API key in the configured environment database and
/// print it to stdout (context goes to stderr so the key is scriptable:
/// `KEY=$(hot key create)`).
pub(crate) async fn run_key_create(
    description: &str,
    conf: &Val,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    use hot::db::api_key::ApiKey;
    use hot::db::env::Env;
    use hot::db::user::User;
    use hot::db::{create_db_pool, get_db_uri_from_conf, redact_password};

    // Idempotent: brings a fresh project's database up to schema and seeds the
    // default org/env/user, so `hot key create` works before the first
    // `hot dev`.
    crate::run_migrations_with_bootstrap(conf, providers)
        .await
        .map_err(|e| format!("Failed to prepare database: {}", e))?;

    let db = create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to open database: {}", e))?;

    let env = Env::get_default_env(&db).await.map_err(|e| {
        format!(
            "Failed to load default environment: {} (is this an initialized Hot project? run `hot init` first)",
            e
        )
    })?;
    let user = User::get_default_user(&db)
        .await
        .map_err(|e| format!("Failed to load default user: {}", e))?;

    let api_key_id = uuid::Uuid::now_v7();
    let (key, key_data) = ApiKey::generate_api_key(&api_key_id)
        .map_err(|e| format!("Failed to generate API key: {}", e))?;
    let key_data: serde_json::Value = serde_json::from_str(&key_data)
        .map_err(|e| format!("Failed to encode API key hash: {}", e))?;
    let permissions = serde_json::json!({ "*:*": ["*"] });

    ApiKey::insert_api_key(
        &db,
        &api_key_id,
        &env.env_id,
        description,
        &key_data,
        &user.user_id,
        &permissions,
    )
    .await
    .map_err(|e| format!("Failed to store API key: {}", e))?;

    eprintln!(
        "Created full-access API key '{}' for environment '{}' in {}.",
        description,
        env.name,
        redact_password(&get_db_uri_from_conf(conf)),
    );
    eprintln!("The key is shown once and cannot be recovered — store it now:");
    println!("{}", key);

    Ok(())
}

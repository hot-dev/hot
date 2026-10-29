//! `hot admin` — operator access for self-hosted installs that works without
//! email: create the first administrator and reset a forgotten password.
//!
//! Both commands write directly to the database the configuration points at
//! (`hot.db.uri`) and never create tables, so run `hot db migrate` first. They
//! are idempotent, which lets a deployment agent run `bootstrap` on every
//! start.

use hot::db::DatabasePool;
use hot::db::user::{User, UserAuth};
use hot::val::Val;
use std::path::Path;

const PASSWORD_AUTH: &str = "email_password";
/// The same minimum the sign-up form enforces.
const MIN_PASSWORD_LEN: usize = 8;

/// Create a password sign-in for `email` unless one exists. An existing user
/// or password is left unchanged, so retries and repeated runs are safe.
pub(crate) async fn run_admin_bootstrap(
    email: &str,
    name: Option<&str>,
    password_file: &Path,
    conf: &Val,
) -> Result<(), String> {
    let email = checked_email(email)?;
    let auth_data = password_auth_data(password_file)?;
    let db = onboarding_db(conf).await?;

    let user_id = match User::get_user_by_email(&db, &email).await {
        Ok(user) => user.user_id,
        Err(error) if error.is_not_found() => {
            let user_id = uuid::Uuid::now_v7();
            User::insert_user(&db, &user_id, &email, name, Some(&user_id))
                .await
                .map_err(|e| format!("Failed to create user {email}: {e}"))?;
            user_id
        }
        Err(error) => return Err(format!("Failed to look up {email}: {error}")),
    };
    // A user can exist without a password sign-in: created through OAuth, or
    // by an earlier run that stopped between the two writes.
    match UserAuth::get_user_auth(&db, PASSWORD_AUTH, &email).await {
        Ok(_) => {
            eprintln!("{email} can already sign in with a password; nothing changed.");
            return Ok(());
        }
        Err(error) if error.is_not_found() => {}
        Err(error) => return Err(format!("Failed to look up {email}: {error}")),
    }
    UserAuth::insert_user_auth(
        &db,
        &uuid::Uuid::now_v7(),
        &user_id,
        PASSWORD_AUTH,
        &email,
        Some(&auth_data),
        &user_id,
    )
    .await
    .map_err(|e| format!("Failed to store the password for {email}: {e}"))?;
    eprintln!(
        "{email} can now sign in at {}/signin.",
        hot::env::get_app_url()
    );
    Ok(())
}

/// Replace the password of an existing password sign-in.
pub(crate) async fn run_admin_reset_password(
    email: &str,
    password_file: &Path,
    conf: &Val,
) -> Result<(), String> {
    let email = checked_email(email)?;
    let auth_data = password_auth_data(password_file)?;
    let db = onboarding_db(conf).await?;
    let auth = match UserAuth::get_user_auth(&db, PASSWORD_AUTH, &email).await {
        Ok(auth) => auth,
        Err(error) if error.is_not_found() => {
            return Err(format!(
                "{email} has no password sign-in; create one with `hot admin bootstrap`"
            ));
        }
        Err(error) => return Err(format!("Failed to look up {email}: {error}")),
    };
    UserAuth::update_auth_data(&db, &auth.user_auth_id, &auth_data, &auth.user_id)
        .await
        .map_err(|e| format!("Failed to store the password for {email}: {e}"))?;
    eprintln!("Reset the password for {email}.");
    Ok(())
}

/// User and credential rows are global; the sign-up flow writes them under
/// the same onboarding scope.
async fn onboarding_db(conf: &Val) -> Result<DatabasePool, String> {
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to open database: {e}"))?;
    Ok(db.with_scope(hot::db::DatabaseScope::Bootstrap(
        hot::db::BootstrapOperation::Onboarding,
    )))
}

fn checked_email(email: &str) -> Result<String, String> {
    let email = email.trim();
    if !email.contains('@') {
        return Err(format!("{email:?} is not an email address"));
    }
    Ok(email.to_string())
}

/// Read a password from a file, never from the command line, where it would
/// be visible to other processes.
fn password_auth_data(path: &Path) -> Result<serde_json::Value, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read the password file {path:?}: {e}"))?;
    let password = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .unwrap_or(&contents);
    if password.trim().is_empty() || password.len() < MIN_PASSWORD_LEN {
        return Err(format!(
            "The password in {path:?} must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    let hash = hot::auth::hash_password_with_random_salt(password)
        .map_err(|e| format!("Failed to hash the password: {e}"))?;
    serde_json::from_str(&hash).map_err(|e| format!("Failed to encode the password hash: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn password_file(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), contents).unwrap();
        file
    }

    async fn sqlite_conf() -> (tempfile::TempDir, Val) {
        let dir = tempfile::tempdir().unwrap();
        let uri = format!("sqlite:{}", dir.path().join("hot.db").display());
        let conf = hot::val!({"db": {"uri": uri.as_str()}});
        hot::db::run_migrations(&conf).await.unwrap();
        (dir, conf)
    }

    async fn can_sign_in(conf: &Val, email: &str, password: &str) -> bool {
        let db = onboarding_db(conf).await.unwrap();
        let auth = UserAuth::get_user_auth(&db, PASSWORD_AUTH, email)
            .await
            .unwrap();
        let stored = serde_json::to_string(&auth.auth_data.unwrap()).unwrap();
        hot::auth::verify_password(password, &stored).unwrap()
    }

    #[tokio::test]
    async fn bootstrap_creates_a_password_sign_in_once() {
        let (_dir, conf) = sqlite_conf().await;
        let first = password_file("first-password\n");
        run_admin_bootstrap(" admin@example.com ", Some("Admin"), first.path(), &conf)
            .await
            .unwrap();
        assert!(can_sign_in(&conf, "admin@example.com", "first-password").await);

        // Re-running, even with another password, changes nothing.
        let second = password_file("second-password");
        run_admin_bootstrap("admin@example.com", None, second.path(), &conf)
            .await
            .unwrap();
        assert!(can_sign_in(&conf, "admin@example.com", "first-password").await);

        run_admin_reset_password("admin@example.com", second.path(), &conf)
            .await
            .unwrap();
        assert!(can_sign_in(&conf, "admin@example.com", "second-password").await);
        assert!(!can_sign_in(&conf, "admin@example.com", "first-password").await);
    }

    #[tokio::test]
    async fn bootstrap_adds_a_password_to_an_existing_user() {
        let (_dir, conf) = sqlite_conf().await;
        let db = onboarding_db(&conf).await.unwrap();
        let user_id = uuid::Uuid::now_v7();
        User::insert_user(&db, &user_id, "oauth@example.com", None, Some(&user_id))
            .await
            .unwrap();
        let file = password_file("a-password");
        run_admin_bootstrap("oauth@example.com", None, file.path(), &conf)
            .await
            .unwrap();
        let auth = UserAuth::get_user_auth(&db, PASSWORD_AUTH, "oauth@example.com")
            .await
            .unwrap();
        assert_eq!(auth.user_id, user_id);
    }

    #[tokio::test]
    async fn rejects_weak_passwords_and_unknown_users() {
        let (_dir, conf) = sqlite_conf().await;
        let short = password_file("short\n");
        let error = run_admin_bootstrap("a@example.com", None, short.path(), &conf)
            .await
            .unwrap_err();
        assert!(error.contains("at least 8"), "{error}");
        let file = password_file("long-enough");
        let error = run_admin_reset_password("nobody@example.com", file.path(), &conf)
            .await
            .unwrap_err();
        assert!(error.contains("hot admin bootstrap"), "{error}");
        let error = run_admin_bootstrap("not-an-email", None, file.path(), &conf)
            .await
            .unwrap_err();
        assert!(error.contains("not an email address"), "{error}");
    }
}

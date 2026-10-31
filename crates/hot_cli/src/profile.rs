//! Profile resolution for `hot.profile.<name>` configuration.
//!
//! A profile bundles a user/org/env identity (typically loaded from
//! `~/.hot/profile.hot`) and is used by commands that touch the local DB
//! directly (`--local` mode). These helpers extract the human-readable
//! identifiers and resolve them to UUIDs against the database.

use hot::val::Val;
use uuid::Uuid;

/// Pull the exact selected identity, honoring canonical `hot.set.profile`.
pub(crate) fn extract_profile_identifiers(
    conf: &Val,
) -> Result<Option<hot::profile::ProfileIdentifiers>, String> {
    hot::profile::selected_profile_identifiers(conf)
}

/// Resolve `(user_email, org_slug, env_name)` to `(user_id, env_id, org_id)`
/// by checking the user exists, is a member of the org, and the env is active.
pub(crate) async fn resolve_profile_to_uuids(
    db: &hot::db::DatabasePool,
    user_email: &str,
    org_slug: &str,
    env_name: &str,
) -> Result<(Uuid, Uuid, Uuid), String> {
    use hot::db::{BootstrapOperation, DatabaseScope, Env, Org, User};
    let scoped = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let db = &scoped;

    let user = match User::get_user_by_email(db, user_email).await {
        Ok(user) => user,
        Err(_) => {
            return Err(format!(
                "Profile error: User with email '{}' not found or not active. Run `hot init` \
                 to create the local identity; on PostgreSQL, pass the schema owner's \
                 --db.migration-uri",
                user_email
            ));
        }
    };

    let org = match Org::get_org_by_slug(db, org_slug).await {
        Ok(org) => org,
        Err(_) => {
            return Err(format!(
                "Profile error: Organization with slug '{}' not found",
                org_slug
            ));
        }
    };

    Org::resolve_user_access(db, &org.org_id, &user.user_id)
        .await
        .map_err(|e| format!("Profile access lookup failed: {e}"))?
        .filter(|access| access.runtime_allowed)
        .ok_or_else(|| {
            format!("Profile user '{user_email}' cannot access active organization '{org_slug}'")
        })?;

    let env = match Env::get_env_by_org_and_name(db, &org.org_id, env_name).await {
        Ok(env) => env,
        Err(_) => {
            return Err(format!(
                "Profile error: Environment '{}' not found in organization '{}'",
                env_name, org_slug
            ));
        }
    };

    if !env.active {
        return Err(format!(
            "Profile error: Environment '{}' in organization '{}' is not active",
            env_name, org_slug
        ));
    }

    Ok((user.user_id, env.env_id, org.org_id))
}

/// Direct-database commands are trusted host operations, but still bind every
/// query to the selected profile. A shared database must never pick its first
/// organization or environment when no profile was supplied.
pub(crate) async fn resolve_cli_identity(
    db: &hot::db::DatabasePool,
    conf: &Val,
) -> Result<(Uuid, Uuid, Uuid), String> {
    use hot::db::{BootstrapOperation, DatabaseScope, Env, Org, User};
    if let Some((user, org, env)) = extract_profile_identifiers(conf)? {
        return resolve_profile_to_uuids(db, &user, &org, &env).await;
    }
    if conf.get("profile").is_some() {
        return Err("Configured profile must include user.email, org.slug, and env.name; refusing to select a default identity".into());
    }
    let metadata = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let orgs = Org::get_all_orgs(&metadata, Some(2), Some(0))
        .await
        .map_err(|e| e.to_string())?;
    if orgs.len() != 1 {
        return Err("Configure a profile with user.email, org.slug, and env.name for this database; the organization is ambiguous or missing".into());
    }
    let org = &orgs[0];
    let envs = Env::get_envs_by_org(&metadata, &org.org_id)
        .await
        .map_err(|e| e.to_string())?;
    if envs.len() != 1 {
        return Err("Configure profile env.name; the selected organization's environment is ambiguous or missing".into());
    }
    let user = User::get_default_user(&metadata)
        .await
        .map_err(|e| e.to_string())?;
    resolve_profile_to_uuids(&metadata, &user.email, &org.slug, &envs[0].name).await
}

pub(crate) async fn create_cli_db_pool(conf: &Val) -> Result<hot::db::DatabasePool, String> {
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| e.to_string())?;
    let (_, env_id, org_id) = resolve_cli_identity(&db, conf).await?;
    Ok(db.with_scope(hot::db::DatabaseScope::Environment { org_id, env_id }))
}

/// Load the exact selected environment, never the organization's default.
pub(crate) async fn resolve_cli_environment(
    db: &hot::db::DatabasePool,
    conf: &Val,
) -> Result<(Uuid, hot::db::Env, hot::db::Org), String> {
    let (user_id, env_id, org_id) = resolve_cli_identity(db, conf).await?;
    let exact = db.with_scope(hot::db::DatabaseScope::Environment { org_id, env_id });
    let db = &exact;
    let env = hot::db::Env::get_env_uncached(db, &env_id)
        .await
        .map_err(|e| e.to_string())?;
    let org = hot::db::Org::get_org_uncached(db, &org_id)
        .await
        .map_err(|e| e.to_string())?;
    if env.org_id != org_id || !env.active {
        return Err("Selected environment is inactive or belongs to another organization".into());
    }
    Ok((user_id, env, org))
}

pub(crate) async fn ensure_build_environment(
    db: &hot::db::DatabasePool,
    build_id: &Uuid,
    env_id: &Uuid,
) -> Result<(), String> {
    let owner = hot::db::Build::get_env_id_for_build(db, build_id)
        .await
        .map_err(|_| "Build not found in the selected environment".to_string())?;
    if owner != *env_id {
        return Err("Build not found in the selected environment".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profile_selects_second_environment_without_falling_back_to_default() {
        let db = hot::db::test_db().await;
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let user = hot::db::User::get_user(&db, &fixture.user_id)
            .await
            .unwrap();
        let org = hot::db::Org::get_org_uncached(&db, &fixture.org_id)
            .await
            .unwrap();
        let selected = Uuid::new_v4();
        hot::db::Env::insert_env(&db, &selected, &org.org_id, "second", &user.user_id)
            .await
            .unwrap();
        let conf = hot::val!({"set":{"profile":"chosen"},"profile":{"chosen":{
            "user":{"email":user.email},"org":{"slug":org.slug},"env":{"name":"second"}
        }}});
        let (resolved_user, env, resolved_org) = resolve_cli_environment(&db, &conf).await.unwrap();
        assert_eq!(
            (resolved_user, env.env_id, resolved_org.org_id),
            (fixture.user_id, selected, fixture.org_id)
        );
        assert_ne!(env.env_id, fixture.env_id);
        assert!(resolve_cli_identity(&db, &hot::val!({})).await.is_err());
        assert!(
            ensure_build_environment(&db, &fixture.build_id, &selected)
                .await
                .is_err()
        );
        ensure_build_environment(&db, &fixture.build_id, &fixture.env_id)
            .await
            .unwrap();
        assert!(
            resolve_cli_identity(&db, &hot::val!({"set":{"profile":"missing"}}))
                .await
                .unwrap_err()
                .contains("Configured profile")
        );
    }
    #[tokio::test]
    async fn canonical_child_profile_and_conflicting_legacy_selector_fail_closed() {
        let db = hot::db::test_db().await;
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let user = hot::db::User::get_user(&db, &fixture.user_id)
            .await
            .unwrap();
        let account_id = uuid::Uuid::new_v4();
        hot::db::Org::insert_org(
            &db,
            &account_id,
            "Test account",
            "profile-account",
            "organization",
            &fixture.user_id,
        )
        .await
        .unwrap();
        hot::db::OrgUser::insert_org_user(
            &db,
            &uuid::Uuid::new_v4(),
            &account_id,
            &fixture.user_id,
            Some(2),
            &fixture.user_id,
        )
        .await
        .unwrap();
        let child = hot::db::Org::create_child(
            &db,
            &account_id,
            &fixture.user_id,
            &hot::db::org::CreateChild {
                name: "Child".into(),
                slug: "profile-child".into(),
                external_id: None,
                idempotency_key: "profile-child".into(),
                hosted: false,
            },
        )
        .await
        .unwrap();
        let conf = hot::val!({"set":{"profile":"child"},"profile":{"child":{
            "user":{"email":user.email},"org":{"slug":child.org.slug},"env":{"name":"development"}
        }}});
        let (_, env, org) = resolve_cli_environment(&db, &conf).await.unwrap();
        assert_eq!((env.env_id, org.org_id), (child.env_id, child.org.org_id));
        let conflicting = conf.merge(&hot::val!({"profile":{"set":"local-dev"}}));
        assert!(
            resolve_cli_environment(&db, &conflicting)
                .await
                .unwrap_err()
                .contains("Conflicting profile")
        );
        let malformed = conf.merge(&hot::val!({"set":{"profile":42}}));
        assert!(resolve_cli_environment(&db, &malformed).await.is_err());
    }
}

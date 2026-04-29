//! Profile resolution for `hot.profile.<name>` configuration.
//!
//! A profile bundles a user/org/env identity (typically loaded from
//! `~/.hot/profile.hot`) and is used by commands that touch the local DB
//! directly (`--local` mode). These helpers extract the human-readable
//! identifiers and resolve them to UUIDs against the database.

use hot::val::Val;
use uuid::Uuid;

/// Pull `(user_email, org_slug, env_name)` out of the resolved
/// `hot.profile.<name>` config, honoring `hot.profile.set` (defaults to
/// `local-dev`). Returns `None` if any piece is missing.
pub(crate) fn extract_profile_identifiers(conf: &Val) -> Option<(String, String, String)> {
    let profile_conf = conf.get("profile")?;

    let profile_to_use = if let Some(default_profile_name) = profile_conf.get("set") {
        let profile_name = match default_profile_name {
            Val::Str(name) => (*name).to_string(),
            _ => return None,
        };

        profile_conf.get(&profile_name)?
    } else {
        profile_conf.get("local-dev")?
    };

    let user_email: String = match profile_to_use.get("user") {
        Some(user) => match user.get("email") {
            Some(email) => match email {
                Val::Str(s) => (*s).to_string(),
                _ => email.to_string(),
            },
            _ => return None,
        },
        _ => return None,
    };

    let org_slug: String = match profile_to_use.get("org") {
        Some(org) => match org.get("slug") {
            Some(slug) => match slug {
                Val::Str(s) => (*s).to_string(),
                _ => slug.to_string(),
            },
            _ => return None,
        },
        _ => return None,
    };

    let env_name: String = match profile_to_use.get("env") {
        Some(env) => match env.get("name") {
            Some(name) => match name {
                Val::Str(s) => (*s).to_string(),
                _ => name.to_string(),
            },
            _ => return None,
        },
        _ => return None,
    };

    Some((user_email, org_slug, env_name))
}

/// Resolve `(user_email, org_slug, env_name)` to `(user_id, env_id, org_id)`
/// by checking the user exists, is a member of the org, and the env is active.
pub(crate) async fn resolve_profile_to_uuids(
    db: &hot::db::DatabasePool,
    user_email: &str,
    org_slug: &str,
    env_name: &str,
) -> Result<(Uuid, Uuid, Uuid), String> {
    use hot::db::{Env, Org, OrgUser, User};

    let user = match User::get_user_by_email(db, user_email).await {
        Ok(user) => user,
        Err(_) => {
            return Err(format!(
                "Profile error: User with email '{}' not found or not active",
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

    match OrgUser::get_org_user(db, &org.org_id, &user.user_id).await {
        Ok(_) => {}
        Err(_) => {
            return Err(format!(
                "Profile error: User '{}' is not a member of organization '{}'",
                user_email, org_slug
            ));
        }
    }

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

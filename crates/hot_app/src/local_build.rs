//! Authorization for reading this host's live project sources.
//!
//! A database project name is not authority to read a similarly named project
//! on the app host. Only the configured local profile can identify that source.

use hot::db::{BootstrapOperation, Build, DatabasePool, DatabaseScope, Env, Org, Project, User};
use hot::val::Val;

pub(crate) async fn authorized_live_project(
    db: &DatabasePool,
    conf: &Val,
    build: &Build,
) -> Result<Project, String> {
    if !hot::env::is_local_dev() || !build.is_live() {
        return Err("Host source is available only for the selected local live build".into());
    }
    // Recheck visibility before any filesystem or cache access. On PostgreSQL
    // this executes under the caller's scope rather than trusting a Build value.
    let current = Build::get_build(db, &build.build_id)
        .await
        .map_err(|_| "Live build is not visible in the current scope".to_string())?;
    if !current.is_live() || current.project_id != build.project_id {
        return Err("Live build identity changed".into());
    }
    let project = Project::get_project(db, &current.project_id)
        .await
        .map_err(|e| format!("Failed to load live project: {e}"))?;
    if !project.active || hot::project::get_project_conf(conf, &project.name).is_none() {
        return Err("Live project is not configured on this host".into());
    }

    let (email, org_slug, env_name) = hot::profile::selected_profile_identifiers(conf)?
        .ok_or_else(|| {
            "An explicit local profile is required before reading host source".to_string()
        })?;
    let metadata = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let user = User::get_user_by_email(&metadata, &email)
        .await
        .map_err(|e| format!("Cannot resolve local profile user: {e}"))?;
    let org = Org::get_org_by_slug(&metadata, &org_slug)
        .await
        .map_err(|e| format!("Cannot resolve local profile organization: {e}"))?;
    let env = Env::get_env_by_org_and_name(&metadata, &org.org_id, &env_name)
        .await
        .map_err(|e| format!("Cannot resolve local profile environment: {e}"))?;
    if !env.active || env.env_id != project.env_id {
        return Err("Live build does not belong to this host's selected environment".into());
    }
    if !Org::resolve_user_access(&metadata, &org.org_id, &user.user_id)
        .await
        .map_err(|e| format!("Cannot authorize local profile: {e}"))?
        .is_some_and(|access| access.runtime_allowed)
    {
        return Err("Local profile cannot access this active organization".into());
    }
    let live = Build::get_live_build_by_project(db, &project.project_id)
        .await
        .map_err(|e| format!("Cannot resolve current live build: {e}"))?;
    if !live.is_some_and(|live| live.build_id == current.build_id) {
        return Err("Requested build is not the current local live build".into());
    }
    Ok(project)
}

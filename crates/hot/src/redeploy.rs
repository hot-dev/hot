//! Redeploy the deployments that another major version of Hot built, after
//! an upgrade (`hot db redeploy`).
//!
//! A runtime doesn't run a build from another major version. For each such
//! deployment, the source and packages in its stored bundle are built again
//! and checked with this version, as `hot deploy` would. One that passes
//! becomes a new build, deployed blue/green: the old build serves until a
//! worker activates the new one. One that fails stays as it is, and its
//! work waits until its project is redeployed.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::db::{
    BootstrapOperation, Build, DatabasePool, DatabaseScope, Env, MaintenanceOperation, Org, Project,
};
use crate::val::Val;

/// What `redeploy` does.
#[derive(Debug, Clone, Default)]
pub struct RedeployOptions {
    /// Build and check each deployment without changing anything.
    pub dry_run: bool,
    /// For a deployment that fails, email its account's admins and raise
    /// a `deploy:failed` alert.
    pub notify: bool,
    /// Only the deployments of this organization (slug), or of its
    /// sub-organizations.
    pub org: Option<String>,
}

/// What happened to one deployment.
#[derive(Debug, Clone)]
pub enum RedeployResult {
    /// Built and checked with this version, and requested for deployment,
    /// as the new build (`None` on a dry run).
    Redeployed { build_id: Option<Uuid> },
    /// Building, checking or deploying it failed. Its work waits until its
    /// project is redeployed.
    Failed {
        reason: String,
        /// Admins emailed about it.
        notified: usize,
    },
    /// Left alone: a newer deployment of the project is already underway.
    Skipped { reason: String },
}

/// One deployment that another major version of Hot built.
#[derive(Debug, Clone)]
pub struct RedeployReport {
    pub org_slug: String,
    pub env_name: String,
    pub project_name: String,
    pub build_id: Uuid,
    pub engine_version: String,
    pub result: RedeployResult,
}

/// Redeploy every active deployment that another major version of Hot built.
pub async fn redeploy(
    db: &DatabasePool,
    conf: &Val,
    options: &RedeployOptions,
) -> Result<Vec<RedeployReport>, String> {
    let storage = crate::storage::build_storage_from_config(conf).await?;
    redeploy_with_storage(db, conf, storage.as_ref(), options).await
}

async fn redeploy_with_storage(
    db: &DatabasePool,
    conf: &Val,
    storage: &dyn crate::storage::BuildStorage,
    options: &RedeployOptions,
) -> Result<Vec<RedeployReport>, String> {
    // Workers read every deployment under the queue's maintenance scope.
    let all = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Queue,
        org_id: None,
    });
    let builds = Build::get_ready_bundle_runtime_builds(&all, i64::MAX)
        .await
        .map_err(|e| format!("Cannot list deployments: {e}"))?;

    let mut reports = Vec::new();
    for build in builds {
        let project = Project::get_project(&all, &build.project_id)
            .await
            .map_err(|e| format!("Cannot load project {}: {e}", build.project_id))?;
        let env = Env::get_env(&all, &project.env_id)
            .await
            .map_err(|e| format!("Cannot load environment {}: {e}", project.env_id))?;
        let org = Org::get_org(&all, &env.org_id)
            .await
            .map_err(|e| format!("Cannot load organization {}: {e}", env.org_id))?;
        if let Some(slug) = &options.org
            && !organization_matches(&all, &org, slug).await
        {
            continue;
        }
        let db = db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id: env.env_id,
        });

        // A build deployed before its versions were recorded is read from
        // its bundle.
        let mut build = build;
        if build.is_bundle() && build.engine_version.is_none() {
            match recorded_versions(&db, storage, &build, &env).await {
                Ok(versions) => (build.engine_version, build.hot_std_version) = versions,
                Err(error) => {
                    tracing::warn!(build_id = %build.build_id, %error, "Cannot read the bundle's versions")
                }
            }
        }
        if build.runtime_version_error_message().is_none() {
            continue;
        }

        let mut report = RedeployReport {
            org_slug: org.slug.clone(),
            env_name: env.name.clone(),
            project_name: project.name.clone(),
            build_id: build.build_id,
            engine_version: build.engine_version.clone().unwrap_or_default(),
            result: RedeployResult::Skipped {
                reason: String::new(),
            },
        };
        let sequence = Project::get_deployment_sequence(&db, &project.project_id)
            .await
            .map_err(|e| format!("Cannot load project {}: {e}", project.project_id))?;
        if sequence > build.deployment_sequence {
            report.result = RedeployResult::Skipped {
                reason: "a newer deployment of the project is underway".to_string(),
            };
            reports.push(report);
            continue;
        }

        report.result = match rebuild(&db, conf, storage, &build, &project, &env, options).await {
            Ok(build_id) => RedeployResult::Redeployed { build_id },
            Err(reason) => {
                let notified = if options.notify && !options.dry_run {
                    notify(db.clone(), conf, &build, &project, &env, &org, &reason).await
                } else {
                    0
                };
                RedeployResult::Failed { reason, notified }
            }
        };
        reports.push(report);
    }
    Ok(reports)
}

/// Whether `org` is the organization `slug` names, or one of its
/// sub-organizations.
async fn organization_matches(db: &DatabasePool, org: &Org, slug: &str) -> bool {
    if org.slug == slug {
        return true;
    }
    match org.parent_org_id {
        Some(parent) => Org::get_org(db, &parent)
            .await
            .is_ok_and(|parent| parent.slug == slug),
        None => false,
    }
}

/// The engine and hot-std versions a build's bundle records, which are then
/// stored on the build.
async fn recorded_versions(
    db: &DatabasePool,
    storage: &dyn crate::storage::BuildStorage,
    build: &Build,
    env: &Env,
) -> Result<(Option<String>, Option<String>), String> {
    let data = storage
        .retrieve_build(&build.build_id, &env.org_id, &env.env_id)
        .await?;
    let dir = Workspace::new(build.build_id)?;
    crate::bundle::extract_bundle_from_bytes(&data, dir.path())?;
    let manifest = crate::bundle::read_bundle_manifest_unchecked(dir.path())?;
    let versions = (
        Some(manifest.engine_version).filter(|v| !v.is_empty()),
        Some(manifest.hot_std_version).filter(|v| !v.is_empty()),
    );
    Build::update_manifest_versions(
        db,
        &build.build_id,
        versions.0.as_deref(),
        versions.1.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(versions)
}

/// Build the deployed bundle's source again with this version, check it,
/// and request the new build's deployment. Returns the new build, or `None`
/// on a dry run.
async fn rebuild(
    db: &DatabasePool,
    conf: &Val,
    storage: &dyn crate::storage::BuildStorage,
    build: &Build,
    project: &Project,
    env: &Env,
    options: &RedeployOptions,
) -> Result<Option<Uuid>, String> {
    let data = storage
        .retrieve_build(&build.build_id, &env.org_id, &env.env_id)
        .await?;
    let workspace = Workspace::new(build.build_id)?;
    let dir = workspace.path().join("bundle");
    crate::bundle::extract_bundle_from_bytes(&data, &dir)?;
    let manifest = crate::bundle::read_bundle_manifest_unchecked(&dir)?;
    if manifest.bundle_name != project.name {
        return Err(format!(
            "The bundle is for project '{}', not '{}'",
            manifest.bundle_name, project.name
        ));
    }

    let build_conf = bundle_project_conf(conf, &project.name, &dir)?;
    let resources = dir.join(crate::bundle::RESOURCE_BUNDLE_PREFIX);
    let context = crate::build::BuildContext::new(
        Some(build.created_by_user_id),
        Some(env.env_id),
        Some(env.org_id),
        Some(project.project_id),
        project.name.clone(),
        vec![dir.join("hot/src").to_string_lossy().into_owned()],
        Vec::new(),
    )
    .with_resources(
        if resources.is_dir() {
            vec![resources]
        } else {
            Vec::new()
        },
        false,
        Vec::new(),
    );

    if options.dry_run {
        crate::build::create_build(db, context, Some(&build_conf)).await?;
        return Ok(None);
    }

    let build_dir = workspace.path().join("build");
    let result = crate::build::build_create(
        db,
        Some(&build_dir.to_string_lossy()),
        context,
        Some(&build_conf),
    )
    .await?;
    let new_build_id = result.build.build_id;
    let version = crate::version::current_runtime_version();
    Build::update_manifest_versions(db, &new_build_id, Some(version), Some(version))
        .await
        .map_err(|e| format!("Cannot record the new build's versions: {e}"))?;
    let storage_path = storage
        .store_build_from_path(&new_build_id, &env.org_id, &env.env_id, &result.zip_path)
        .await?;
    Build::update_build_storage(db, &new_build_id, &storage_path, storage.storage_type())
        .await
        .map_err(|e| format!("Cannot record the new build's storage: {e}"))?;

    let strict = conf.get_bool_or_default("deploy.ctx.strict", false);
    crate::build::validate_ctx_requirements_for_deploy(
        db,
        &new_build_id,
        &project.project_id,
        &env.org_id,
        &env.env_id,
        storage,
        strict,
    )
    .await?;
    crate::build::validate_box_requirements_for_deploy(
        db,
        &new_build_id,
        &env.org_id,
        &env.env_id,
        storage,
    )
    .await?;
    crate::build::validate_schedule_requirements_for_deploy(
        db,
        &new_build_id,
        &env.org_id,
        &env.env_id,
        conf,
        storage,
    )
    .await?;

    let deployed_by = build.updated_by_user_id.unwrap_or(build.created_by_user_id);
    Build::request_bundle_deployment(db, &new_build_id, &deployed_by)
        .await
        .map_err(|e| format!("Cannot request the deployment: {e}"))?;
    if let Err(error) = crate::lang::event::enqueue_deployment_message(conf, new_build_id).await {
        let error = format!("Cannot queue the deployment: {error}");
        let _ = Build::mark_runtime_failed(db, &new_build_id, &error).await;
        return Err(error);
    }
    Ok(Some(new_build_id))
}

/// `conf` with the project `name` built from the bundle extracted at `dir`:
/// its source, and each package it carries as a local dependency at the
/// path the bundle had it, so the new bundle lays them out the same way.
fn bundle_project_conf(conf: &Val, name: &str, dir: &Path) -> Result<Val, String> {
    let mut deps = indexmap::IndexMap::new();
    for (package, path) in bundled_packages(&dir.join("hot/pkg"))? {
        deps.insert(
            Val::from(package),
            crate::val!({"local": path.to_string_lossy().into_owned()}),
        );
    }
    let project = crate::val!({
        "src": {"paths": [dir.join("hot/src").to_string_lossy().into_owned()]},
        "deps": Val::Map(Box::new(deps)),
    });
    let mut root = match conf {
        Val::Map(map) => (**map).clone(),
        _ => indexmap::IndexMap::new(),
    };
    let mut projects = match root.get(&Val::from("project")) {
        Some(Val::Map(map)) => (**map).clone(),
        _ => indexmap::IndexMap::new(),
    };
    projects.insert(Val::from(name), project);
    root.insert(Val::from("project"), Val::Map(Box::new(projects)));
    Ok(Val::Map(Box::new(root)))
}

/// The packages a bundle carries under `hot/pkg`, named as the build named
/// them: `<registry>/<package>` under a directory with a dot in its name,
/// such as `hot.dev/anthropic`, or the directory alone.
fn bundled_packages(pkg_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut packages = Vec::new();
    if !pkg_dir.is_dir() {
        return Ok(packages);
    }
    for entry in subdirectories(pkg_dir)? {
        let name = entry
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.contains('.') {
            for package in subdirectories(&entry)? {
                let package_name = package
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                packages.push((format!("{name}/{package_name}"), package));
            }
        } else {
            packages.push((name, entry));
        }
    }
    packages.sort();
    Ok(packages)
}

fn subdirectories(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| format!("Cannot read {}: {e}", dir.display()))?
            .path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Tell the admins of the deployment's account that it failed, by email
/// and with a `deploy:failed` alert. Returns how many were emailed.
async fn notify(
    db: DatabasePool,
    conf: &Val,
    build: &Build,
    project: &Project,
    env: &Env,
    org: &Org,
    reason: &str,
) -> usize {
    let runtime = crate::version::current_runtime_version();
    let version = build
        .engine_version
        .as_deref()
        .unwrap_or("an earlier version");
    let alert = format!(
        "{} was built with Hot {version} and couldn't be built with Hot {runtime}: {reason}",
        project.name
    );
    crate::db::alert::publish_deploy_failed_alert(
        &db,
        &env.org_id,
        &env.env_id,
        &build.build_id,
        &project.name,
        &alert,
    )
    .await;

    let sender = crate::email::EmailSender::from_conf(conf);
    if !sender.is_available() {
        tracing::warn!(
            "No email provider is configured; the admins of '{}' weren't emailed",
            org.slug
        );
        return 0;
    }
    let membership = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let account = Org::billing_owner_id_for(&membership, &org.org_id)
        .await
        .unwrap_or(org.org_id);
    let admins = match crate::db::OrgUser::get_admins_for_org(&membership, &account).await {
        Ok(admins) => admins,
        Err(error) => {
            tracing::warn!(org = %org.slug, %error, "Cannot list the account's admins");
            return 0;
        }
    };

    let (subject, text) = failure_email(project, env, org, version, runtime, reason);
    let mut sent = 0;
    for admin in admins {
        let user = match crate::db::User::get_user(&membership, &admin.user_id).await {
            Ok(user) => user,
            Err(error) => {
                tracing::warn!(user_id = %admin.user_id, %error, "Cannot load an admin");
                continue;
            }
        };
        let email = crate::email::Email::new(&user.email, &subject).with_text(&text);
        match sender.send_email(&email).await {
            Ok(()) => sent += 1,
            Err(error) => tracing::warn!(user_id = %admin.user_id, %error, "Cannot email an admin"),
        }
    }
    sent
}

fn failure_email(
    project: &Project,
    env: &Env,
    org: &Org,
    version: &str,
    runtime: &str,
    reason: &str,
) -> (String, String) {
    let subject = format!("Redeploy {} for Hot {runtime}", project.name);
    let reason: String = reason.lines().take(40).collect::<Vec<_>>().join("\n");
    let text = format!(
        "Your project {project} ({org}/{env}) is deployed with Hot {version}. This server now \
         runs Hot {runtime}, which doesn't run builds from another major version, and \
         building the project again with Hot {runtime} failed:\n\n{reason}\n\n\
         Until you redeploy it, its events and calls wait, its schedules are paused, and \
         its webhooks and MCP tools are unavailable. With the Hot {runtime} CLI, run \
         `hot check` and `hot test`, fix what they report, then run `hot deploy`.\n\n\
         Upgrade guide: https://hot.dev/docs/migrations\n",
        project = project.name,
        org = org.slug,
        env = env.name,
    );
    (subject, text)
}

/// A scratch directory for one deployment, removed when dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new(build_id: Uuid) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "hot-redeploy-{}-{}",
            build_id.simple(),
            Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BuildStorage;
    use std::io::{Read, Write};

    const SOURCE: &str = r#"::app ns

greet fn (name: Str): Str { `hi ${name}` }

on-ping
meta {on-event: "ping"}
fn (event) { greet("there") }
"#;

    /// Deploy `name` from SOURCE, then make it look built by Hot 1.0.0, with
    /// `stored_source` in its bundle. Returns the build and its stored bundle.
    #[allow(clippy::too_many_arguments)]
    async fn deploy_earlier_major(
        db: &DatabasePool,
        conf: &Val,
        storage: &dyn BuildStorage,
        work: &Path,
        org: Uuid,
        env: Uuid,
        user: Uuid,
        name: &str,
        stored_source: &str,
    ) -> (Uuid, Vec<u8>) {
        let src = work.join(name).join("hot/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.hot"), SOURCE).unwrap();
        let context = crate::build::BuildContext::new(
            Some(user),
            Some(env),
            Some(org),
            None,
            name.to_string(),
            vec![src.to_string_lossy().into_owned()],
            Vec::new(),
        );
        let build_dir = work.join(name).join("build");
        let result =
            crate::build::build_create(db, Some(&build_dir.to_string_lossy()), context, Some(conf))
                .await
                .unwrap();
        let build_id = result.build.build_id;
        let bytes = std::fs::read(&result.zip_path).unwrap();
        Build::request_bundle_deployment(db, &build_id, &user)
            .await
            .unwrap();
        crate::build::load_build_manifest_data(db, &build_id, &env, &bytes)
            .await
            .unwrap();
        assert!(
            Build::activate_prepared_build(db, &build_id, &user)
                .await
                .unwrap()
        );

        // The bundle an earlier major version would have stored.
        let version = crate::version::current_runtime_version();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let entry_name = entry.name().unwrap().to_string();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            if entry_name == "manifest.hot" {
                content = String::from_utf8(content)
                    .unwrap()
                    .replace(&format!("\"{version}\""), "\"1.0.0\"")
                    .into_bytes();
            } else if entry_name == "hot/src/main.hot" {
                content = stored_source.as_bytes().to_vec();
            }
            writer
                .start_file(entry_name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&content).unwrap();
        }
        let earlier = writer.finish().unwrap().into_inner();
        let path = storage
            .store_build(&build_id, &org, &env, earlier.clone())
            .await
            .unwrap();
        Build::update_build_storage(db, &build_id, &path, storage.storage_type())
            .await
            .unwrap();
        Build::update_manifest_versions(db, &build_id, Some("1.0.0"), Some("1.0.0"))
            .await
            .unwrap();
        (build_id, earlier)
    }

    async fn handler_count(db: &DatabasePool, build_id: &Uuid) -> usize {
        crate::db::EventHandler::get_event_handlers_by_build(db, build_id, None, None)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn redeploys_what_builds_and_leaves_what_fails() {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let (org, env, user) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO org(org_id,name,slug,org_type,created_by_user_id) VALUES (?,'Acme','acme','organization',?)")
            .bind(org).bind(user).execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES (?,?,'production',?)",
        )
        .bind(env)
        .bind(org)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
        let work = tempfile::TempDir::new().unwrap();
        let storage = crate::storage::LocalBuildStorage::new(work.path().join("builds"));
        let conf = crate::val!({"queue": {"type": "memory"}});

        let (good, good_bundle) = deploy_earlier_major(
            &db,
            &conf,
            &storage,
            work.path(),
            org,
            env,
            user,
            "good",
            SOURCE,
        )
        .await;
        let broken = SOURCE.replace(r#"greet("there")"#, "no-such-fn()");
        let (bad, _) = deploy_earlier_major(
            &db,
            &conf,
            &storage,
            work.path(),
            org,
            env,
            user,
            "bad",
            &broken,
        )
        .await;

        // Loading a build this runtime can't run changes nothing.
        let handlers = handler_count(&db, &good).await;
        assert!(handlers > 0);
        let refused = crate::build::load_build_manifest_data(&db, &good, &env, &good_bundle)
            .await
            .unwrap_err();
        assert!(refused.contains("major versions must match"), "{refused}");
        assert_eq!(handler_count(&db, &good).await, handlers);

        let result_for = |reports: &[RedeployReport], build: Uuid| {
            reports
                .iter()
                .find(|report| report.build_id == build)
                .map(|report| report.result.clone())
                .unwrap()
        };
        let dry_run = RedeployOptions {
            dry_run: true,
            ..Default::default()
        };
        let reports = redeploy_with_storage(&db, &conf, &storage, &dry_run)
            .await
            .unwrap();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(matches!(
            result_for(&reports, good),
            RedeployResult::Redeployed { build_id: None }
        ));
        let RedeployResult::Failed { reason, .. } = result_for(&reports, bad) else {
            panic!("{reports:?}")
        };
        assert!(reason.contains("no-such-fn"), "{reason}");
        let project = Build::get_build(&db, &good).await.unwrap().project_id;
        let builds = Build::get_builds_by_project(&db, &project, None, None)
            .await
            .unwrap();
        assert_eq!(builds.len(), 1, "a dry run makes no build");

        let reports = redeploy_with_storage(&db, &conf, &storage, &RedeployOptions::default())
            .await
            .unwrap();
        let RedeployResult::Redeployed {
            build_id: Some(rebuilt),
        } = result_for(&reports, good)
        else {
            panic!("{reports:?}")
        };
        let rebuilt = Build::get_build(&db, &rebuilt).await.unwrap();
        assert_eq!(
            rebuilt.engine_version.as_deref(),
            Some(crate::version::current_runtime_version())
        );
        assert_eq!(rebuilt.runtime_status, Build::RUNTIME_STATUS_PENDING);
        assert_eq!(handler_count(&db, &rebuilt.build_id).await, handlers);
        assert!(storage.exists(&rebuilt.build_id, &org, &env).await.unwrap());
        assert!(matches!(
            result_for(&reports, bad),
            RedeployResult::Failed { .. }
        ));
        // Blue/green: the old builds serve until a worker activates the new.
        for build in [good, bad] {
            let build = Build::get_build(&db, &build).await.unwrap();
            assert!(build.deployed, "{build:?}");
            assert_eq!(build.runtime_status, Build::RUNTIME_STATUS_READY);
        }

        // A second run leaves the redeployed project alone.
        let reports = redeploy_with_storage(&db, &conf, &storage, &RedeployOptions::default())
            .await
            .unwrap();
        assert!(matches!(
            result_for(&reports, good),
            RedeployResult::Skipped { .. }
        ));
        assert!(matches!(
            result_for(&reports, bad),
            RedeployResult::Failed { .. }
        ));

        // Another organization's deployments are left out.
        let other = RedeployOptions {
            dry_run: true,
            org: Some("other".to_string()),
            ..Default::default()
        };
        assert!(
            redeploy_with_storage(&db, &conf, &storage, &other)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn bundled_packages_keep_registry_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let pkg = dir.path().join("hot/pkg");
        for package in ["hot.dev/anthropic/src", "hot.dev/hot-ai/src", "local-lib"] {
            std::fs::create_dir_all(pkg.join(package)).unwrap();
        }
        let names: Vec<String> = bundled_packages(&pkg)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["hot.dev/anthropic", "hot.dev/hot-ai", "local-lib"]);
        assert!(
            bundled_packages(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }
}

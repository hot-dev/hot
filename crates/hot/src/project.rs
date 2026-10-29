//! Project configuration helpers. The conf readers and dependency resolution
//! live in `hot_lang::project`; installing the resource registry needs the
//! runtime, so it stays here.

use crate::val::Val;

pub use hot_lang::project::*;

/// Build a `ResourceRegistry` from project config and install it
/// process-globally so that `::hot::resource/*` bindings can read from it.
///
/// `extra_paths` are appended after the project-config paths (typically
/// from `--resource.path` CLI flags). When `force_no_gitignore` is true
/// it overrides the project's `respect-gitignore` setting.
pub fn install_resource_registry(
    conf: &Val,
    project_name: &str,
    extra_paths: &[String],
    force_no_gitignore: bool,
) -> crate::lang::hot::resource::ResourceRegistry {
    let mut paths: Vec<std::path::PathBuf> = get_project_resource_paths(conf, project_name)
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    for p in extra_paths {
        paths.push(std::path::PathBuf::from(p));
    }
    let respect = if force_no_gitignore {
        false
    } else {
        get_project_respect_gitignore(conf, project_name)
    };
    let mut excludes = get_project_ignore_excludes(conf, project_name);
    excludes.extend(get_project_resource_excludes(conf, project_name));
    let registry = crate::lang::hot::resource::build_registry(&paths, respect, &excludes);
    crate::lang::hot::resource::set_registry(registry.clone());
    registry
}

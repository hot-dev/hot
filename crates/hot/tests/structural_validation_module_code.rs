//! Structural validation must not execute a module's top-level code.
//!
//! This lives in its own integration binary for one reason: the canary calls
//! hot-std's `fail`, so the engine's resolver has to find hot-std. `cargo test`
//! runs with the cwd set to the package root, where the dev path
//! `./hot/pkg/hot-std` does not resolve, and CI has no installed Hot — so the
//! equivalent unit test could only skip there, silently losing the coverage.
//!
//! A dedicated binary owns its process, which makes setting `HOT_HOME`
//! race-free: no other test can observe the mutated environment. Pointing it at
//! a temp directory that links the in-repo hot-std also keeps the test's caches
//! out of the repo and off the developer's real cache.

use std::path::{Path, PathBuf};

/// The hot-std shipped in this checkout, located relative to the crate.
fn repo_hot_std() -> Option<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest.join("../../hot/pkg/hot-std");
    candidate.join("src").is_dir().then_some(candidate)
}

/// Build a `HOT_HOME` whose `pkg/hot-std` resolves to the in-repo copy.
fn link_hot_std(home: &Path, hot_std: &Path) {
    let pkg = home.join("pkg");
    std::fs::create_dir_all(&pkg).expect("create pkg dir");
    let link = pkg.join("hot-std");
    #[cfg(unix)]
    std::os::unix::fs::symlink(hot_std, &link).expect("link hot-std");
    #[cfg(not(unix))]
    {
        fn copy_dir(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap().flatten() {
                let (src, dst) = (entry.path(), to.join(entry.file_name()));
                if src.is_dir() {
                    copy_dir(&src, &dst);
                } else {
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }
        copy_dir(hot_std, &link);
    }
}

#[test]
fn structural_validation_does_not_execute_module_code() {
    let Some(hot_std) = repo_hot_std() else {
        panic!(
            "in-repo hot-std not found relative to {}; this test must not be \
             silently skipped — it exists to cover a gap CI previously had",
            env!("CARGO_MANIFEST_DIR")
        );
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("hot-home");
    link_hot_std(&home, &hot_std);

    // SAFETY: this binary contains exactly one test, so nothing else in the
    // process can race on the environment.
    unsafe {
        std::env::set_var("HOT_HOME", &home);
    }

    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("required_ctx.hot"),
        r#"::required_ctx ns

needs-key
meta {ctx: {"api.key": {required: true}}}
fn (): Str { "unused" }
"#,
    )
    .unwrap();

    // If module code runs, `fail` aborts with this message.
    let target_file = temp.path().join("validation_target.hot");
    std::fs::write(
        &target_file,
        r#"::validation_target ns

must-not-run fail("structural validation executed module code")
"#,
    )
    .unwrap();

    let src_paths = vec![src_dir.to_string_lossy().to_string()];
    let target_file = target_file.to_string_lossy().to_string();

    // Structural validation collects the ctx requirement without demanding a
    // value, and without running the module's top-level expression.
    hot::lang::engine::Engine::run_file_pipeline_with_deps(
        &target_file,
        &src_paths,
        &[],
        None,
        None,
        false,
    )
    .expect("structural validation must not execute module code");

    // Execute mode rejects the missing key *before* module code runs.
    let error = hot::lang::engine::Engine::run_unified_pipeline(
        &src_paths,
        &[],
        None,
        None,
        Some(&target_file),
        None,
        hot::lang::engine::PipelineMode::Execute,
        None,
        None,
        None,
        Some(ahash::AHashMap::new()),
        None,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
    )
    .expect_err("execute must reject the missing context key");

    assert!(
        error.contains("Missing required context variable 'api.key'"),
        "expected the ctx pre-flight error, got: {error}"
    );
    assert!(
        !error.contains("structural validation executed module code"),
        "module code must not have executed: {error}"
    );
}

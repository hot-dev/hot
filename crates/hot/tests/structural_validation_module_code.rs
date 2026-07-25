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

use std::path::Path;

/// Write a minimal hot-std into `home/pkg/hot-std`.
///
/// The canary needs exactly one hot-std symbol — `fail` — and hot-std's own
/// definition is a thin wrapper over a Rust native
/// (`call-lib(::hot::exec/fail, ...)`), so a fixture reproduces it faithfully
/// in a few lines. Synthesizing it keeps this test independent of repository
/// layout: it behaves identically in the monorepo, in CI, and against a
/// packaged or vendored `hot` crate, with no path probing and no skip branch
/// that could quietly delete the coverage.
fn write_hot_std_fixture(home: &Path) {
    let root = home.join("pkg").join("hot-std");
    let src = root.join("src").join("hot");
    std::fs::create_dir_all(&src).expect("create fixture dirs");

    std::fs::write(
        root.join("pkg.hot"),
        r#"::hot::pkg ns

hot.pkg.hot-std {
  name: "hot.dev/hot-std",
  version: "0.0.0-fixture",
  src-paths: ["src/"]
}
"#,
    )
    .expect("write fixture pkg.hot");

    // `core: true` makes `fail` resolvable unqualified, as in real hot-std.
    std::fs::write(
        src.join("exec.hot"),
        r#"::hot::exec ns

fail
meta { core: true }
fn (lazy err: Any): Any {
    call-lib(::hot::exec/fail, [err])
}
"#,
    )
    .expect("write fixture exec.hot");
}

#[test]
fn structural_validation_does_not_execute_module_code() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("hot-home");
    write_hot_std_fixture(&home);

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

    // Positive control: with the context satisfied, module code *does* run and
    // the canary fires. Without this, a fixture whose `fail` silently stopped
    // aborting would make every assertion above vacuously true.
    let mut context = ahash::AHashMap::new();
    context.insert("api.key".to_string(), hot::val::Val::from("present"));
    let fired = hot::lang::engine::Engine::run_unified_pipeline(
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
        Some(context),
        None,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
    );
    let fired = fired.expect_err("module code should run once context is satisfied");
    assert!(
        fired.contains("structural validation executed module code"),
        "the canary must be armed — expected the fail() message, got: {fired}"
    );
}

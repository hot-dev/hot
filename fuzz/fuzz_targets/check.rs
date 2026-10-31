//! `hot check` on any one-file project reports diagnostics instead of
//! panicking. Set HOT_HOME to a checkout's `hot` directory so hot-std loads
//! from it, as the nightly workflow does.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("fuzz.hot"), source).expect("write fuzz.hot");
    let src = vec![dir.path().to_string_lossy().into_owned()];
    let _ = hot::lang::engine::Engine::check_sources_pipeline_diagnostics(&src, &[], None, None);
});

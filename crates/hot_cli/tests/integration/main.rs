// Consolidated integration test binary to avoid linking dozens of
// separate test executables in CI and local builds.

#[cfg(unix)]
mod check_command;
#[cfg(unix)]
mod cli_regressions;
mod configuration_precedence;
#[cfg(unix)]
mod csv_records;
#[cfg(unix)]
mod deploy_local;
#[cfg(unix)]
mod event_handler_retries;
#[cfg(unix)]
mod live_build_cache;
#[cfg(unix)]
mod model_stream;
mod skill_legacy_migration;
mod undefined_project;
#[cfg(unix)]
mod uninstall;

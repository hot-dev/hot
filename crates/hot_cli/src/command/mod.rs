//! Per-command handler functions, one module per `hot` subcommand.
//!
//! Modules in here are invoked by the dispatch in `main.rs`'s `async_main`.
//! They own the implementation of a single command (or a tightly related
//! group, e.g. `init`'s setup helpers); shared plumbing lives elsewhere
//! (`crate::cli` for clap types, `crate::remote::ApiClient` for HTTP,
//! `crate::profile` for profile resolution, `crate::conf` for the config
//! pipeline and runtime emitter/event-publisher constructors).

pub(crate) mod ai;
pub(crate) mod api;
pub(crate) mod app;
pub(crate) mod build;
pub(crate) mod builds;
pub(crate) mod check;
pub(crate) mod compile;
pub(crate) mod conf;
pub(crate) mod context;
pub(crate) mod db;
pub(crate) mod deploy;
pub(crate) mod deps;
pub(crate) mod dev;
pub(crate) mod eval;
pub(crate) mod extract;
pub(crate) mod fmt;
pub(crate) mod init;
pub(crate) mod key;
pub(crate) mod project;
pub(crate) mod queue;
pub(crate) mod repl;
pub(crate) mod run;
pub(crate) mod scheduler;
pub(crate) mod test;
pub(crate) mod worker;

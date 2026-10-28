//! The parts of Hot's caching layer the compiler needs.
//!
//! * [`ast_cache`] — lossless AST and `Val` encoding used by the AST and
//!   bytecode caches (the cache files themselves are managed by `hot`).
//! * [`paths`]     — canonical filesystem layout used by the caches.

pub mod ast_cache;
pub mod paths;

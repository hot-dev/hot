//! Hot's compiled-artifact caching layer.
//!
//! * [`bytecode_cache`] — the on-disk bytecode/program cache (file format, IO).
//! * [`paths`]          — canonical filesystem layout used by the cache.
//! * [`unit_cache`]     — per-source-unit cache entries and invalidation.
//! * [`ast_cache`]      — pre-built `HotAst` caching for fast cached execution.

pub mod ast_cache;
pub mod bytecode_cache;
pub mod paths;
pub mod unit_cache;

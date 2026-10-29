//! The Hot language without its runtime.
//!
//! Lexer, parser, AST, diagnostics, `hot fmt`, and the compiler: core-variable
//! extraction, `%` lowering, `where` constraint checks, name resolution, type
//! checking, artifact validation and bytecode generation. Tools that parse and
//! check Hot build on this crate without compiling the VM, JIT, storage or
//! network stack, which live in `hot`.
//!
//! It also checks whole projects the way `hot check` does ([`check`]):
//! package discovery and dependency resolution ([`project`],
//! [`lang::discover`]), and hot-std from this crate's own commit
//! ([`lang::hot_std`]).
//!
//! The module layout mirrors `hot::lang`, which re-exports these modules, so
//! `hot::lang::parser` and `hot_lang::lang::parser` are the same module.

pub mod check;
pub mod discovery;
pub mod lang;
pub mod project;
pub mod val;
pub mod version;

/// Fingerprint of this crate's sources, which `hot` folds into its cache keys
/// so a compiler change invalidates cached ASTs and bytecode.
pub const SOURCE_FINGERPRINT: &str = env!("HOT_LANG_SOURCE_FINGERPRINT");

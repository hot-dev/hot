//! Remote API client and shared error formatting for control-plane CLI commands.
//!
//! All commands that talk to a hot.dev API server (`hot deploy`, `hot builds`,
//! `hot project`, `hot context`, ...) build their requests through
//! [`ApiClient`]. Local-only commands never touch this module.

mod client;
mod error;

pub(crate) use client::ApiClient;

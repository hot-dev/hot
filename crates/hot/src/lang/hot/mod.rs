// Hot library functions used by the bytecode engine.
//
// This module provides versions of Hot library functions that are optimized
// for the bytecode engine with tighter integration and no SharedEngine dependency.
//
// Each module contains functions for a specific domain, similar to lang/hot structure.

pub mod alert;
pub mod base64;
pub mod bit;
pub mod bool;
pub mod r#box;
pub mod bytes;
pub mod cmp;
pub mod coll;
pub mod core;
pub mod ctx;
pub mod env;
pub mod event;
pub mod exec;
pub mod file;
pub mod hash;
pub mod hex;
pub mod hmac;
pub mod http;
pub mod info;
pub mod internal_mcp;
pub mod internal_skill;
pub mod internal_tokenizer;
pub mod io;
pub mod iter;
pub mod json;
pub mod lambda;
pub mod lang;
pub mod lib_util;
pub mod libmap;
pub mod map;
pub mod math;
pub mod md;
pub mod meta;
pub mod mime;
pub mod random;
pub mod regex;
pub mod resource;
pub mod run;
pub mod schedule;
pub mod store;
pub mod str;
pub mod stream;
pub mod task;
pub mod test;
pub mod time;
pub mod r#type;
pub mod uri;
pub mod uuid;
pub mod ws;
pub mod xml;

// Re-export the Hot library map.
pub use libmap::{HotLibFn, HotLibMap, get_hotlib_map};

use crate::val;
use crate::val::Val;

/// Resolve configuration for LSP settings by merging defaults into the full config
/// Defaults:
/// - lsp.transport = "stdio"
pub fn get_resolved_conf(conf: Val) -> Val {
    // Provide nested defaults under the `lsp` key and merge with the full conf
    let defaults = val!({
        "lsp": {
            "transport": "stdio"
        }
    });
    // Merge so that existing user-provided values in `conf` override defaults
    defaults.merge(&conf)
}

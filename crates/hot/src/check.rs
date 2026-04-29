use crate::val;
use crate::val::Val;

/// Resolve configuration for analyzer-related settings (check and watch)
/// Provides defaults, and merges user configuration on top so user conf overrides defaults.
pub fn get_resolved_conf(conf: Val) -> Val {
    // Defaults for check and watch
    let defaults = val!({
        "check": {
            "format": "pretty",
            "raw": false,
            // Type-check test files by default so arity/type errors in tests are
            // caught at `hot check` time. Override with `--with-tests false` or
            // `check.with-tests: false` in hot.hot if the test sweep gets noisy.
            "with-tests": true
        },
        "watch": {
            "debounce": 200,
            "with-tests": false
        }
    });

    // Merge defaults with existing conf; existing conf wins on conflicts
    defaults.merge(&conf)
}

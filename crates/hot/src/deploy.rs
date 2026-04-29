use crate::val;
use crate::val::Val;

/// Resolve deploy-related configuration.
///
/// Defaults keep the existing permissive deploy behavior while allowing
/// projects and CI to opt into stricter checks through `hot.deploy.ctx.strict`.
pub fn get_resolved_conf(conf: Val) -> Val {
    let defaults = val!({
        "auto": true,
        "auto_create_project": true,
        "auto_upload_build": true,
        "auto_bundle_live_build": true,
        "ctx": {
            "strict": false,
        },
    });

    defaults.merge(&conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_ctx_defaults_to_false() {
        let conf = get_resolved_conf(Val::map_empty());

        assert!(!conf.get_bool("ctx.strict"));
    }

    #[test]
    fn user_conf_overrides_defaults() {
        let conf = get_resolved_conf(val!({
            "ctx": {
                "strict": true,
            },
        }));

        assert!(conf.get_bool("ctx.strict"));
    }
}

use crate::val;
use crate::val::Val;

/// Get resolved configuration for custom domain provisioning.
///
/// Defaults: mode="none" (disabled). Provider-specific settings live in
/// the deployment composition layer, such as `hot-cloud`.
pub fn get_resolved_conf(conf: Val) -> Val {
    let default_conf = val!({
        "mode": "none",
    });

    default_conf.merge(&conf)
}

pub fn mode(conf: &Val) -> String {
    conf.get_str_or_default("domain.mode", "none")
}

pub fn custom_domains_enabled(conf: &Val) -> bool {
    mode(conf) != "none"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_domain_mode_disables_custom_domain_provisioning() {
        let conf = crate::val!({});

        assert!(!custom_domains_enabled(&conf));
    }

    #[test]
    fn non_none_domain_mode_enables_custom_domain_provisioning() {
        let conf = crate::val!({
            "domain": {"mode": "provider"},
        });

        assert!(custom_domains_enabled(&conf));
    }
}

use crate::val::Val;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductExperienceMode {
    LocalDev,
    SelfHost,
    HotCloud,
}

impl ProductExperienceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalDev => "local-dev",
            Self::SelfHost => "self-host",
            Self::HotCloud => "hot-cloud",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "self-host" | "self_host" | "selfhost" => Self::SelfHost,
            "hot-cloud" | "hot_cloud" | "cloud" => Self::HotCloud,
            _ => Self::LocalDev,
        }
    }
}

pub fn get_resolved_conf(conf: Val) -> Val {
    let defaults = crate::val!({
        "experience": ProductExperienceMode::LocalDev.as_str(),
        "web-url": "https://hot.dev",
        "pricing-url": "https://hot.dev/pricing",
        "support-email": "support@hot.dev",
    });

    defaults.merge(&conf)
}

pub fn experience(conf: &Val) -> ProductExperienceMode {
    ProductExperienceMode::parse(&conf.get_str_or_default("product.experience", "local-dev"))
}

pub fn experience_mode(conf: &Val) -> ProductExperienceMode {
    experience(conf)
}

pub fn is_hot_cloud(conf: &Val) -> bool {
    matches!(experience(conf), ProductExperienceMode::HotCloud)
}

pub fn is_self_host(conf: &Val) -> bool {
    matches!(experience(conf), ProductExperienceMode::SelfHost)
}

pub fn is_local_dev_experience(conf: &Val) -> bool {
    matches!(experience(conf), ProductExperienceMode::LocalDev)
}

pub fn billing_enabled(conf: &Val) -> bool {
    is_hot_cloud(conf) && conf.get_bool_or_default("billing.enabled", false)
}

pub fn should_show_cloud_upsells(conf: &Val) -> bool {
    is_local_dev_experience(conf)
}

pub fn is_no_nag(conf: &Val) -> bool {
    is_self_host(conf)
}

fn configured_value(conf: &Val, key: &str, default: &str) -> String {
    let value = conf.get_str_or_default(key, default);
    if value.trim().is_empty() {
        default.to_string()
    } else {
        value.trim().to_string()
    }
}

pub fn web_url(conf: &Val) -> String {
    configured_value(conf, "product.web-url", "https://hot.dev")
        .trim_end_matches('/')
        .to_string()
}

pub fn pricing_url(conf: &Val) -> String {
    let default = format!("{}/pricing", web_url(conf));
    configured_value(conf, "product.pricing-url", &default)
}

pub fn support_email(conf: &Val) -> String {
    configured_value(conf, "product.support-email", "support@hot.dev")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_cloud_mode_enables_billing_only_when_configured() {
        let conf = crate::val!({
            "product": {"experience": "hot-cloud"},
            "billing": {"enabled": true},
        });

        assert!(billing_enabled(&conf));
    }

    #[test]
    fn public_default_keeps_billing_disabled_and_upsells_available() {
        let conf = crate::val!({});

        assert!(!billing_enabled(&conf));
        assert!(should_show_cloud_upsells(&conf));
    }

    #[test]
    fn self_host_mode_disables_nag_surfaces() {
        let conf = crate::val!({
            "product": {"experience": "self-host"},
            "billing": {"enabled": true},
        });

        assert!(!billing_enabled(&conf));
        assert!(is_no_nag(&conf));
    }

    #[test]
    fn product_urls_default_to_official_site() {
        let conf = crate::val!({});

        assert_eq!(web_url(&conf), "https://hot.dev");
        assert_eq!(pricing_url(&conf), "https://hot.dev/pricing");
        assert_eq!(support_email(&conf), "support@hot.dev");
    }

    #[test]
    fn product_urls_are_configurable() {
        let conf = crate::val!({
            "product": {
                "web-url": "https://example.com/",
                "pricing-url": "https://example.com/plans",
                "support-email": "help@example.com",
            },
        });

        assert_eq!(web_url(&conf), "https://example.com");
        assert_eq!(pricing_url(&conf), "https://example.com/plans");
        assert_eq!(support_email(&conf), "help@example.com");
    }
}

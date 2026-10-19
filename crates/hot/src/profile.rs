use crate::val;
use crate::val::Val;

/// Resolve the canonical `set.profile` selector. The legacy `profile.set`
/// spelling is accepted only when the canonical selector is absent or agrees.
/// Invalid or conflicting selectors must never silently select another scope.
pub fn selected_profile_name(conf: &Val) -> Result<String, String> {
    fn selector(conf: &Val, section: &str, key: &str) -> Result<Option<String>, String> {
        let Some(container) = conf.get(section) else {
            return Ok(None);
        };
        if !matches!(container, Val::Map(_)) {
            return Err(format!("Profile configuration '{section}' must be a map"));
        }
        match container.get(key) {
            None => Ok(None),
            Some(Val::Str(value)) if !value.trim().is_empty() => Ok(Some(value.to_string())),
            Some(_) => Err(format!(
                "Profile selector '{section}.{key}' must be a nonempty string"
            )),
        }
    }
    let canonical = selector(conf, "set", "profile")?;
    let legacy = selector(conf, "profile", "set")?;
    match (canonical, legacy) {
        (Some(canonical), Some(legacy)) if canonical != legacy => Err(
            "Conflicting profile selectors: use set.profile and remove legacy profile.set".into(),
        ),
        (Some(name), _) | (None, Some(name)) => Ok(name),
        (None, None) => Ok("local-dev".into()),
    }
}

pub type ProfileIdentifiers = (String, String, String);

/// Extract the exact selected profile, without coercing invalid identity values.
/// Only a configuration with no profiles and no selector can return `None`.
pub fn selected_profile_identifiers(conf: &Val) -> Result<Option<ProfileIdentifiers>, String> {
    let name = selected_profile_name(conf)?;
    let Some(profiles) = conf.get("profile") else {
        if conf.get("set").and_then(|set| set.get("profile")).is_some() {
            return Err(format!("Configured profile '{name}' does not exist"));
        }
        return Ok(None);
    };
    let profile = profiles
        .get(&name)
        .ok_or_else(|| format!("Configured profile '{name}' does not exist"))?;
    let read = |section: &str, key: &str| -> Result<String, String> {
        match profile.get(section).and_then(|section| section.get(key)) {
            Some(Val::Str(value)) if !value.trim().is_empty() => Ok(value.to_string()),
            _ => Err(format!(
                "Configured profile '{name}' must include a nonempty string {section}.{key}"
            )),
        }
    };
    Ok(Some((
        read("user", "email")?,
        read("org", "slug")?,
        read("env", "name")?,
    )))
}

pub fn get_resolved_conf(conf: Val) -> Val {
    let resolved_conf = conf.clone();

    // Invalid selectors are preserved by the merge below and rejected by the
    // strict identity resolver before any database or host-source operation.
    let default_profile_name =
        selected_profile_name(&resolved_conf).unwrap_or_else(|_| "local-dev".to_string());

    // Create the default profile configurations
    let default_profiles_conf = create_default_profiles_conf();

    // Merge with any existing profile configurations
    let mut profiles_conf = default_profiles_conf;
    if let Some(existing_profiles) = resolved_conf.get("profile") {
        profiles_conf = profiles_conf.merge(&existing_profiles);
    }

    // Create the full configuration structure
    let mut full_conf = val!({
        "profile": profiles_conf,
        "set": {
            "profile": default_profile_name
        }
    });

    // Merge with existing configuration
    full_conf = full_conf.merge(&resolved_conf);

    full_conf
}

fn create_default_profiles_conf() -> Val {
    val!({
        "local-dev": {
            "user": {
                "email": "local@hot.dev"
            },
            "org": {
                "slug": "local",
            },
            "env": {
                "name": "development"
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_profile_selector_and_legacy_conflicts_are_explicit() {
        assert_eq!(selected_profile_name(&val!({})).unwrap(), "local-dev");
        assert_eq!(
            selected_profile_name(&val!({"set":{"profile":"child"}})).unwrap(),
            "child"
        );
        assert_eq!(
            selected_profile_name(&val!({"profile":{"set":"legacy"}})).unwrap(),
            "legacy"
        );
        assert_eq!(
            selected_profile_name(&val!({"set":{"profile":"same"},"profile":{"set":"same"}}))
                .unwrap(),
            "same"
        );
        for conf in [
            val!({"set":{"profile":"child"},"profile":{"set":"parent"}}),
            val!({"set":{"profile":42}}),
            val!({"set":{"profile":""}}),
            val!({"set":{"profile":"child"},"profile":{"set":false}}),
            val!({"set":"child"}),
        ] {
            assert!(selected_profile_name(&conf).is_err());
            assert!(selected_profile_name(&get_resolved_conf(conf)).is_err());
        }
    }

    #[test]
    fn selected_identity_never_falls_back_from_missing_or_malformed_profile() {
        let conf = val!({"set":{"profile":"child"},"profile":{
            "local-dev":{"user":{"email":"root@example.test"},"org":{"slug":"root"},"env":{"name":"development"}},
            "child":{"user":{"email":"child@example.test"},"org":{"slug":"child"},"env":{"name":"production"}}
        }});
        assert_eq!(
            selected_profile_identifiers(&conf).unwrap().unwrap(),
            (
                "child@example.test".into(),
                "child".into(),
                "production".into()
            )
        );
        for conf in [
            val!({"set":{"profile":"missing"}}),
            val!({"set":{"profile":"child"},"profile":{"child":{"user":{"email":42},"org":{"slug":"child"},"env":{"name":"production"}}}}),
        ] {
            assert!(selected_profile_identifiers(&conf).is_err());
        }
    }
}

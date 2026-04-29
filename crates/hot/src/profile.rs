use crate::val;
use crate::val::Val;

pub fn get_resolved_conf(conf: Val) -> Val {
    let resolved_conf = conf.clone();

    // Get the default profile name from configuration, fallback to "local-dev"
    let default_profile_name = resolved_conf
        .get("set")
        .and_then(|default| default.get("profile"))
        .map(|profile| profile.to_string())
        .unwrap_or_else(|| "local-dev".to_string());

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

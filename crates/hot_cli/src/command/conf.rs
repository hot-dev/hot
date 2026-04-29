//! `hot conf generate` — emit a `hot.hot` template, substituting profile data.

use hot::val::Val;

pub(crate) fn run_conf_generate(
    template: &str,
    output: &Option<String>,
    conf: &Val,
) -> Result<(), String> {
    let template_file = match template.to_lowercase().as_str() {
        "minimal" | "" => "hot.hot.minimal.template",
        "all" => "hot.hot.all.template",
        "api" => "hot.hot.api.template",
        "app" => "hot.hot.app.template",
        "worker" => "hot.hot.worker.template",
        "scheduler" => "hot.hot.scheduler.template",
        other => {
            return Err(format!(
                "Unknown template '{}'. Available templates: all, api, app, worker, scheduler (default: minimal)",
                other
            ));
        }
    };

    let template_content = hot::resources::read_init_template(template_file)?;

    let project_name = hot::project::get_default_project_name(conf);
    let profile_email = conf
        .get("profile")
        .and_then(|p| p.get("local-dev"))
        .and_then(|ld| ld.get("user"))
        .and_then(|u| u.get("email"))
        .map(|v| match v {
            Val::Str(s) => (*s).to_string(),
            _ => v.to_string(),
        })
        .unwrap_or_else(|| "dev@example.com".to_string());
    let profile_slug = conf
        .get("profile")
        .and_then(|p| p.get("local-dev"))
        .and_then(|ld| ld.get("org"))
        .and_then(|o| o.get("slug"))
        .map(|v| match v {
            Val::Str(s) => (*s).to_string(),
            _ => v.to_string(),
        })
        .unwrap_or_else(|| "dev".to_string());
    let profile_env_name = conf
        .get("profile")
        .and_then(|p| p.get("local-dev"))
        .and_then(|ld| ld.get("env"))
        .and_then(|e| e.get("name"))
        .map(|v| match v {
            Val::Str(s) => (*s).to_string(),
            _ => v.to_string(),
        })
        .unwrap_or_else(|| "development".to_string());

    let content = template_content
        .replace("{{PROJECT_NAME}}", &project_name)
        .replace("{{USER_EMAIL}}", &profile_email)
        .replace("{{ORG_SLUG}}", &profile_slug)
        .replace("{{ENV_NAME}}", &profile_env_name);

    match output {
        Some(path) => {
            std::fs::write(path, &content)
                .map_err(|e| format!("Failed to write to {}: {}", path, e))?;
            println!("Generated {} template to {}", template, path);
        }
        None => {
            print!("{}", content);
        }
    }

    Ok(())
}

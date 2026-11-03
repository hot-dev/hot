//! How the app reads Google and GitHub client settings from its environment,
//! and what its startup check and sign-in pages make of them. This binary has a
//! single test because it changes the process's environment.

use hot_app::oauth::OAuthConfig;
use hot_app::templates::PublicPageContext;

const NAMES: [&str; 5] = [
    "HOT_GOOGLE_CLIENT_ID",
    "HOT_GOOGLE_CLIENT_SECRET",
    "HOT_GITHUB_CLIENT_ID",
    "HOT_GITHUB_CLIENT_SECRET",
    "HOT_OAUTH_REDIRECT_BASE_URL",
];

/// Set exactly these variables among the OAuth settings.
fn set_only(variables: &[(&str, &str)]) {
    for name in NAMES {
        // SAFETY: nothing else runs in this binary, for the whole of the test.
        unsafe {
            std::env::remove_var(name);
            std::env::remove_var(format!("{name}_FILE"));
        }
    }
    for (name, value) in variables {
        // SAFETY: as above.
        unsafe { std::env::set_var(name, value) };
    }
}

#[test]
fn half_set_client_settings_are_an_error_that_names_what_is_missing() {
    let self_host = hot::val!({"product": {"experience": "self-host"}});
    let buttons = || {
        let page = PublicPageContext::new_with_conf("login", &self_host);
        (page.google_signin, page.github_signin)
    };

    set_only(&[]);
    let config = OAuthConfig::from_env().unwrap();
    assert!(!config.is_any_configured());
    assert_eq!(config.redirect_base_url, "http://localhost:4680");
    assert_eq!(buttons(), (false, false));

    set_only(&[
        ("HOT_GOOGLE_CLIENT_ID", "google-id"),
        ("HOT_GOOGLE_CLIENT_SECRET", "google-hush"),
        ("HOT_OAUTH_REDIRECT_BASE_URL", "https://hot.acme.com"),
    ]);
    let config = OAuthConfig::from_env().unwrap();
    assert_eq!(config.google.unwrap().client_secret, "google-hush");
    assert!(config.github.is_none());
    assert_eq!(config.redirect_base_url, "https://hot.acme.com");
    assert_eq!(buttons(), (true, false));

    // One alone, or the other blank, is what the startup check panics with.
    // A page has no use for the providers while either of them is broken.
    for (present, value, missing) in [
        (
            "HOT_GOOGLE_CLIENT_ID",
            "google-id",
            "HOT_GOOGLE_CLIENT_SECRET",
        ),
        (
            "HOT_GOOGLE_CLIENT_SECRET",
            "google-hush",
            "HOT_GOOGLE_CLIENT_ID",
        ),
        (
            "HOT_GITHUB_CLIENT_ID",
            "github-id",
            "HOT_GITHUB_CLIENT_SECRET",
        ),
        (
            "HOT_GITHUB_CLIENT_SECRET",
            "github-hush",
            "HOT_GITHUB_CLIENT_ID",
        ),
    ] {
        for blank in [None, Some(""), Some(" ")] {
            let mut variables = vec![(present, value)];
            variables.extend(blank.map(|blank| (missing, blank)));
            set_only(&variables);
            let error = OAuthConfig::from_env().unwrap_err();
            assert!(
                error.contains(missing) && error.contains(present),
                "{error}"
            );
            assert!(!error.contains(value), "{error}");
            assert_eq!(buttons(), (false, false));
        }
    }

    // Blank on both sides is the same as unset.
    set_only(&[
        ("HOT_GITHUB_CLIENT_ID", ""),
        ("HOT_GITHUB_CLIENT_SECRET", ""),
        ("HOT_OAUTH_REDIRECT_BASE_URL", ""),
    ]);
    let config = OAuthConfig::from_env().unwrap();
    assert!(!config.is_any_configured());
    assert_eq!(config.redirect_base_url, "http://localhost:4680");

    // The `_FILE` forms count the same, and an empty file stays an error.
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, contents: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path.to_str().unwrap().to_string()
    };
    let (id, secret, base, empty) = (
        file("id", "google-id\n"),
        file("secret", "google-hush\n"),
        file("base", "https://files.acme.com\n"),
        file("empty", ""),
    );
    set_only(&[
        ("HOT_GOOGLE_CLIENT_ID_FILE", &id),
        ("HOT_GOOGLE_CLIENT_SECRET_FILE", &secret),
        ("HOT_OAUTH_REDIRECT_BASE_URL_FILE", &base),
    ]);
    let config = OAuthConfig::from_env().unwrap();
    assert_eq!(config.google.unwrap().client_id, "google-id");
    assert_eq!(config.redirect_base_url, "https://files.acme.com");
    assert_eq!(buttons(), (true, false));

    // The default experience offers them too once automatic sign-in is off,
    // which is what a deployment that only set `HOT_ENV=production` runs.
    let default_buttons = || {
        let page = PublicPageContext::new_with_conf("login", &hot::val!({}));
        (page.google_signin, page.github_signin)
    };
    // SAFETY: as in `set_only`.
    unsafe { std::env::set_var("HOT_ENV", "development") };
    assert_eq!(default_buttons(), (false, false));
    // SAFETY: as in `set_only`.
    unsafe { std::env::set_var("HOT_ENV", "production") };
    assert_eq!(default_buttons(), (true, false));
    // SAFETY: as in `set_only`.
    unsafe { std::env::remove_var("HOT_ENV") };

    set_only(&[("HOT_GOOGLE_CLIENT_ID_FILE", &id)]);
    let error = OAuthConfig::from_env().unwrap_err();
    assert!(
        error.starts_with("HOT_GOOGLE_CLIENT_SECRET has no value"),
        "{error}"
    );

    set_only(&[
        ("HOT_GOOGLE_CLIENT_ID_FILE", &id),
        ("HOT_GOOGLE_CLIENT_SECRET_FILE", &empty),
    ]);
    let error = OAuthConfig::from_env().unwrap_err();
    assert!(error.contains("HOT_GOOGLE_CLIENT_SECRET_FILE"), "{error}");
    assert!(error.contains("is empty"), "{error}");

    set_only(&[]);
}

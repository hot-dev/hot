//! Private registry credentials for `::hot::box/start`.
//!
//! A `BoxConf` names a context secret with `registry-auth: {secret: "name"}`;
//! it never holds a token itself. `box/start` resolves that name in the run's
//! own context (the environment's and project's context variables, plus
//! `hot/ctx.hot` in development), checks that the secret's registry serves the
//! image, and seals the credential with the org's context-encryption key.
//! Task args, the task row and the queue carry only the secret's name, its
//! registry and that ciphertext. The task worker opens it just before the
//! pull, checks it was sealed for the task's own environment, and hands it to
//! the container runtime for that one pull.

use crate::context_encryption::ContextEncryption;
use crate::val::Val;
use ahash::AHashMap;
use uuid::Uuid;

/// Marks a sealed blob as a box registry credential, so no other ciphertext
/// under the same org key (a context variable, say) can stand in for one.
const SEALED_PURPOSE: &str = "hot.box.registry-auth";
const SEALED_VERSION: u64 = 1;

/// Docker Hub's canonical name. Images without a registry host pull from it.
pub const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_ALIASES: &[&str] = &[
    "docker.io",
    "index.docker.io",
    "registry-1.docker.io",
    "registry.hub.docker.com",
];

/// Parse the `registry-auth` field of a BoxConf into the context secret name.
///
/// The field only takes `{secret: Str}`. Any other key is refused, so a token
/// can never ride in source as a literal.
pub(super) fn parse(raw: Option<&Val>) -> Result<Option<String>, String> {
    let map = match raw {
        None | Some(Val::Null) => return Ok(None),
        Some(Val::Map(map)) => map,
        Some(_) => {
            return Err(
                "'registry-auth' must be a map like {secret: \"ghcr-pull\"} \
                 naming a context secret"
                    .to_string(),
            );
        }
    };
    for (key, _) in map.iter() {
        match key {
            Val::Str(k) if &**k == "secret" => {}
            Val::Str(k) => {
                return Err(format!(
                    "'registry-auth' takes only `secret`, the name of a context secret, \
                     not `{}`; store the registry, username and token in that secret \
                     with ::hot::ctx/set-secret or the environment's context variables",
                    k
                ));
            }
            _ => return Err("'registry-auth' keys must be strings".to_string()),
        }
    }
    match map.get(&Val::from("secret")) {
        Some(Val::Str(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        _ => Err(
            "'registry-auth.secret' must be a non-empty string naming a context secret".to_string(),
        ),
    }
}

/// The registry host an image reference pulls from, following Docker's
/// reference rules: the first path component is a registry host when it
/// contains `.` or `:` or is `localhost`; otherwise the image is on Docker Hub.
pub fn image_registry(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            normalize_registry(first)
        }
        _ => DOCKER_HUB.to_string(),
    }
}

/// Canonical form of a registry name given in a secret: no scheme, no path,
/// lowercase, and Docker Hub's aliases folded into `docker.io`.
pub fn normalize_registry(registry: &str) -> String {
    let trimmed = registry.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host = without_scheme
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if DOCKER_HUB_ALIASES.contains(&host.as_str()) {
        DOCKER_HUB.to_string()
    } else {
        host
    }
}

/// Resolve the named secret in the run's context and seal it for the task
/// worker. Returns the `registry_auth` value for the task args. Errors name
/// the secret and the registry, never a username or token.
pub(super) fn resolve_and_seal(
    context: &AHashMap<String, Val>,
    secret: &str,
    image: &str,
    env_id: &Uuid,
    org_id: Option<&Uuid>,
    encryption: impl FnOnce() -> Result<ContextEncryption, String>,
) -> Result<serde_json::Value, String> {
    let value = match context.get(secret) {
        Some(Val::Null) | None => {
            return Err(format!(
                "registry-auth secret '{}' is not set in this environment's context",
                secret
            ));
        }
        Some(value) => value,
    };
    let fields = match value {
        Val::Map(map) => map,
        _ => {
            return Err(format!(
                "registry-auth secret '{}' must be a map with `registry`, `username` and `token`",
                secret
            ));
        }
    };
    let field = |name: &str| -> Result<String, String> {
        match fields.get(&Val::from(name)) {
            Some(Val::Str(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            _ => Err(format!(
                "registry-auth secret '{}' needs a non-empty string `{}` \
                 (expected `registry`, `username` and `token`)",
                secret, name
            )),
        }
    };
    let registry = normalize_registry(&field("registry")?);
    let username = field("username")?;
    let token = field("token")?;

    let image_registry = image_registry(image);
    if registry != image_registry {
        return Err(format!(
            "registry-auth secret '{}' is for registry '{}', but image '{}' is on '{}'",
            secret, registry, image, image_registry
        ));
    }

    let plaintext = serde_json::json!({
        "purpose": SEALED_PURPOSE,
        "v": SEALED_VERSION,
        "env_id": env_id.to_string(),
        "registry": registry,
        "username": username,
        "token": token,
    })
    .to_string();
    let encryption = encryption()?;
    let sealed = encryption
        .encrypt(&plaintext, &org_id.copied().unwrap_or_else(Uuid::nil))
        .map_err(|e| format!("could not seal registry-auth secret '{}': {}", secret, e))?;

    Ok(serde_json::json!({
        "secret": secret,
        "registry": registry,
        "sealed": sealed,
    }))
}

/// A registry credential for one image pull. Built only on the task worker by
/// [`open`]; it is never serialized, and its `Debug` output hides the
/// username and token.
pub struct RegistryCredential {
    /// Canonical registry host (see [`normalize_registry`]).
    pub registry: String,
    pub username: String,
    pub token: String,
}

impl std::fmt::Debug for RegistryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryCredential")
            .field("registry", &self.registry)
            .field("username", &"<redacted>")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl RegistryCredential {
    /// The server address Docker keys credentials by: Docker Hub's legacy
    /// index URL, or the registry host for every other registry.
    pub fn docker_server_address(&self) -> String {
        if self.registry == DOCKER_HUB {
            "https://index.docker.io/v1/".to_string()
        } else {
            self.registry.clone()
        }
    }

    /// Whether `host` (as a container runtime reports it while authenticating)
    /// is this credential's registry. Credentials are answered for that host
    /// only.
    pub fn serves_host(&self, host: &str) -> bool {
        normalize_registry(host) == self.registry
    }

    /// Remove the username and token from text bound for an error message or
    /// log, in case a runtime echoes them.
    pub fn redact(&self, text: &str) -> String {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.username, self.token));
        let mut out = text.replace(&basic, "<redacted>");
        out = out.replace(&self.token, "<redacted>");
        out
    }
}

/// Open the `registry_auth` of a container task's args, if it has one.
///
/// Refuses a credential sealed for another environment, for another registry
/// than the image's, or that no longer decrypts. Errors carry no username or
/// token.
pub fn open(
    args: &serde_json::Value,
    image: &str,
    env_id: &Uuid,
    org_id: Option<&Uuid>,
    encryption: impl FnOnce() -> Result<ContextEncryption, String>,
) -> Result<Option<RegistryCredential>, String> {
    let auth = match args.get("registry_auth") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(auth) => auth,
    };
    let secret = auth
        .get("secret")
        .and_then(|v| v.as_str())
        .unwrap_or("<unnamed>");
    let sealed = auth
        .get("sealed")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "registry-auth secret '{}' has no sealed credential in the task request",
                secret
            )
        })?;
    let unreadable = || format!("registry-auth secret '{}' could not be opened", secret);

    let encryption = encryption()?;
    let plaintext = encryption
        .decrypt(sealed, &org_id.copied().unwrap_or_else(Uuid::nil))
        .map_err(|_| unreadable())?;
    let payload: serde_json::Value = serde_json::from_str(&plaintext).map_err(|_| unreadable())?;
    let text = |name: &str| payload.get(name).and_then(|v| v.as_str());
    if text("purpose") != Some(SEALED_PURPOSE)
        || payload.get("v").and_then(|v| v.as_u64()) != Some(SEALED_VERSION)
    {
        return Err(unreadable());
    }
    if text("env_id") != Some(env_id.to_string().as_str()) {
        return Err(format!(
            "registry-auth secret '{}' was sealed for another environment",
            secret
        ));
    }
    let (Some(registry), Some(username), Some(token)) =
        (text("registry"), text("username"), text("token"))
    else {
        return Err(unreadable());
    };
    let image_registry = image_registry(image);
    if registry != image_registry || auth.get("registry").and_then(|v| v.as_str()) != Some(registry)
    {
        return Err(format!(
            "registry-auth secret '{}' is for registry '{}', but image '{}' is on '{}'",
            secret, registry, image, image_registry
        ));
    }
    Ok(Some(RegistryCredential {
        registry: registry.to_string(),
        username: username.to_string(),
        token: token.to_string(),
    }))
}

/// The context-encryption key for sealing and opening registry credentials:
/// `HOT_ENCRYPTION_KEY`, or in local development the project's existing
/// `hot/dev.key` (the key the run worker reads context variables with).
pub fn encryption_key() -> Result<ContextEncryption, String> {
    ContextEncryption::from_env_or_existing_dev_key().map_err(|e| {
        format!(
            "registry-auth needs the context-encryption key on this worker: {}",
            e
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_s3cr3tT0kenValue";

    fn key() -> Result<ContextEncryption, String> {
        Ok(ContextEncryption::from_master_key([7u8; 32]))
    }

    fn context_with(value: Val) -> AHashMap<String, Val> {
        let mut context = AHashMap::new();
        context.insert("ghcr-pull".to_string(), value);
        context
    }

    fn ghcr_secret() -> Val {
        crate::val!({"registry": "ghcr.io", "username": "leela-bot", "token": TOKEN})
    }

    #[test]
    fn parse_takes_only_a_secret_name() {
        assert_eq!(parse(None).unwrap(), None);
        assert_eq!(parse(Some(&Val::Null)).unwrap(), None);
        assert_eq!(
            parse(Some(&crate::val!({"secret": "ghcr-pull"}))).unwrap(),
            Some("ghcr-pull".to_string())
        );
        let literal = parse(Some(&crate::val!({"secret": "x", "token": "abc"}))).unwrap_err();
        assert!(literal.contains("not `token`"), "{literal}");
        assert!(parse(Some(&Val::from("ghcr-pull"))).is_err());
        assert!(parse(Some(&crate::val!({"secret": ""}))).is_err());
        assert!(parse(Some(&crate::val!({"secret": 3}))).is_err());
    }

    #[test]
    fn image_registry_follows_docker_reference_rules() {
        assert_eq!(image_registry("alpine"), "docker.io");
        assert_eq!(image_registry("alpine:3.21"), "docker.io");
        assert_eq!(image_registry("library/alpine"), "docker.io");
        assert_eq!(image_registry("withleela/reglang:1"), "docker.io");
        assert_eq!(image_registry("ghcr.io/withleela/reglang:1"), "ghcr.io");
        assert_eq!(image_registry("GHCR.io/withleela/reglang"), "ghcr.io");
        assert_eq!(image_registry("localhost:5000/reglang:1"), "localhost:5000");
        assert_eq!(image_registry("localhost/reglang"), "localhost");
        assert_eq!(
            image_registry("index.docker.io/library/alpine"),
            "docker.io"
        );
        assert_eq!(
            image_registry("ghcr.io/withleela/reglang@sha256:abcd"),
            "ghcr.io"
        );
    }

    #[test]
    fn normalize_registry_strips_scheme_and_path() {
        assert_eq!(normalize_registry("https://ghcr.io/"), "ghcr.io");
        assert_eq!(normalize_registry(" GHCR.IO "), "ghcr.io");
        assert_eq!(
            normalize_registry("https://index.docker.io/v1/"),
            "docker.io"
        );
        assert_eq!(normalize_registry("localhost:5000"), "localhost:5000");
    }

    #[test]
    fn sealed_credential_round_trips_and_hides_the_token() {
        let env_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let image = "ghcr.io/withleela/reglang:1";
        let auth = resolve_and_seal(
            &context_with(ghcr_secret()),
            "ghcr-pull",
            image,
            &env_id,
            Some(&org_id),
            key,
        )
        .unwrap();
        let rendered = auth.to_string();
        assert!(!rendered.contains(TOKEN), "{rendered}");
        assert!(!rendered.contains("leela-bot"), "{rendered}");
        assert_eq!(auth["secret"], "ghcr-pull");
        assert_eq!(auth["registry"], "ghcr.io");

        let args = serde_json::json!({"image": image, "registry_auth": auth});
        let credential = open(&args, image, &env_id, Some(&org_id), key)
            .unwrap()
            .unwrap();
        assert_eq!(credential.registry, "ghcr.io");
        assert_eq!(credential.username, "leela-bot");
        assert_eq!(credential.token, TOKEN);
        assert!(!format!("{credential:?}").contains(TOKEN));
        assert_eq!(
            credential.redact(&format!("denied for {TOKEN}")),
            "denied for <redacted>"
        );
    }

    #[test]
    fn open_refuses_another_environment_or_org() {
        let env_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let image = "ghcr.io/withleela/reglang:1";
        let auth = resolve_and_seal(
            &context_with(ghcr_secret()),
            "ghcr-pull",
            image,
            &env_id,
            Some(&org_id),
            key,
        )
        .unwrap();
        let args = serde_json::json!({"image": image, "registry_auth": auth});

        let other_env = open(&args, image, &Uuid::now_v7(), Some(&org_id), key).unwrap_err();
        assert!(other_env.contains("another environment"), "{other_env}");

        let other_org = open(&args, image, &env_id, Some(&Uuid::now_v7()), key).unwrap_err();
        assert!(other_org.contains("could not be opened"), "{other_org}");
        assert!(!other_org.contains(TOKEN));
    }

    #[test]
    fn open_refuses_a_swapped_image() {
        let env_id = Uuid::now_v7();
        let image = "ghcr.io/withleela/reglang:1";
        let auth = resolve_and_seal(
            &context_with(ghcr_secret()),
            "ghcr-pull",
            image,
            &env_id,
            None,
            key,
        )
        .unwrap();
        let args = serde_json::json!({"registry_auth": auth});
        let err = open(&args, "evil.example/steal:1", &env_id, None, key).unwrap_err();
        assert!(err.contains("is on 'evil.example'"), "{err}");
    }

    #[test]
    fn open_refuses_a_context_variable_ciphertext() {
        let env_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let other = key()
            .unwrap()
            .encrypt(&serde_json::json!({"token": TOKEN}).to_string(), &org_id)
            .unwrap();
        let args = serde_json::json!({
            "registry_auth": {"secret": "ghcr-pull", "registry": "ghcr.io", "sealed": other}
        });
        let err = open(&args, "ghcr.io/a/b", &env_id, Some(&org_id), key).unwrap_err();
        assert!(err.contains("could not be opened"), "{err}");
    }

    #[test]
    fn open_without_registry_auth_is_none() {
        let args = serde_json::json!({"image": "alpine"});
        assert!(
            open(&args, "alpine", &Uuid::now_v7(), None, key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_reports_missing_and_malformed_secrets_without_values() {
        let env_id = Uuid::now_v7();
        let image = "ghcr.io/withleela/reglang:1";
        let missing =
            resolve_and_seal(&AHashMap::new(), "ghcr-pull", image, &env_id, None, key).unwrap_err();
        assert!(missing.contains("is not set"), "{missing}");

        let not_a_map = resolve_and_seal(
            &context_with(Val::from(TOKEN)),
            "ghcr-pull",
            image,
            &env_id,
            None,
            key,
        )
        .unwrap_err();
        assert!(not_a_map.contains("must be a map"), "{not_a_map}");
        assert!(!not_a_map.contains(TOKEN));

        let no_token = resolve_and_seal(
            &context_with(crate::val!({"registry": "ghcr.io", "username": "u"})),
            "ghcr-pull",
            image,
            &env_id,
            None,
            key,
        )
        .unwrap_err();
        assert!(no_token.contains("`token`"), "{no_token}");

        let wrong_registry = resolve_and_seal(
            &context_with(ghcr_secret()),
            "ghcr-pull",
            "alpine:3.21",
            &env_id,
            None,
            key,
        )
        .unwrap_err();
        assert!(
            wrong_registry
                .contains("is for registry 'ghcr.io', but image 'alpine:3.21' is on 'docker.io'"),
            "{wrong_registry}"
        );
        assert!(!wrong_registry.contains(TOKEN));
    }
}

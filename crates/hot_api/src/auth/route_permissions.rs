//! Private API routes must authorize actions as well as credential class.
//! Collection endpoints require collection-wide grants because their handlers
//! return complete collections rather than filtering individual resource grants.
use super::AuthContext;
use axum::http::{Method, StatusCode};
use std::collections::HashMap;

pub(crate) fn authorize(
    auth: &AuthContext,
    method: &Method,
    route: &str,
    params: &HashMap<String, String>,
) -> Result<(), StatusCode> {
    let method = if *method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    // These handlers authorize the actual event type or durable stream owner,
    // including service/session principals. Do not replace that with URL IDs.
    match (route, method) {
        ("/v1/events", "GET" | "POST")
        | ("/v1/events/{event_id}" | "/v1/events/{event_id}/runs", "GET")
        | ("/v1/tasks/{task_id}" | "/v1/tasks/{task_id}/subscribe", "GET")
        | ("/v1/streams/{stream_id}/subscribe", "GET" | "POST")
        | ("/v1/streams/subscribe-with-event", "POST") => return Ok(()),
        _ => {}
    }
    if !auth.is_api_key() {
        return Err(StatusCode::FORBIDDEN);
    }
    let param = |name: &str| {
        params
            .get(name)
            .map(String::as_str)
            .ok_or(StatusCode::BAD_REQUEST)
    };
    let (resource, action) = match (route, method) {
        ("/v1/projects", "GET") => ("project:*".into(), "read"),
        ("/v1/projects", "POST") => ("project:*".into(), "create"),
        ("/v1/projects/{project_id_or_slug}", "GET") => {
            (format!("project:{}", param("project_id_or_slug")?), "read")
        }
        ("/v1/projects/{project_id_or_slug}", "PATCH") => (
            format!("project:{}", param("project_id_or_slug")?),
            "update",
        ),
        ("/v1/projects/{project_id_or_slug}", "DELETE") => (
            format!("project:{}", param("project_id_or_slug")?),
            "delete",
        ),
        ("/v1/projects/{project_id_or_slug}/activate", "POST") => {
            // Activation can redeploy the existing build.
            if !auth.has_permission("build:*", "execute") {
                return Err(StatusCode::FORBIDDEN);
            }
            (
                format!("project:{}", param("project_id_or_slug")?),
                "update",
            )
        }
        ("/v1/projects/{project_id_or_slug}/deactivate", "POST") => (
            format!("project:{}", param("project_id_or_slug")?),
            "update",
        ),
        (
            "/v1/builds"
            | "/v1/projects/{project_id_or_slug}/builds"
            | "/v1/projects/{project_id_or_slug}/builds/deployed"
            | "/v1/projects/{project_id_or_slug}/builds/live"
            | "/v1/projects/{project_id_or_slug}/schedules"
            | "/v1/projects/{project_id_or_slug}/event-handlers",
            "GET",
        ) => ("build:*".into(), "read"),
        ("/v1/projects/{project_id_or_slug}/builds", "POST") => ("build:*".into(), "create"),
        (
            "/v1/projects/{project_id_or_slug}/builds/{build_id}"
            | "/v1/projects/{project_id_or_slug}/builds/{build_id}/download",
            "GET",
        ) => (format!("build:{}", param("build_id")?), "read"),
        ("/v1/projects/{project_id_or_slug}/builds/{build_id}/deploy", "POST") => {
            (format!("build:{}", param("build_id")?), "execute")
        }
        ("/v1/projects/{project_id_or_slug}/context", "GET") => (
            format!("context:{}/*", param("project_id_or_slug")?),
            "read",
        ),
        ("/v1/projects/{project_id_or_slug}/context", "POST") => (
            format!("context:{}/*", param("project_id_or_slug")?),
            "create",
        ),
        ("/v1/projects/{project_id_or_slug}/context/{key}", "PUT") => (
            format!("context:{}/{}", param("project_id_or_slug")?, param("key")?),
            "update",
        ),
        ("/v1/projects/{project_id_or_slug}/context/{key}", "DELETE") => (
            format!("context:{}/{}", param("project_id_or_slug")?, param("key")?),
            "delete",
        ),
        ("/v1/runs" | "/v1/runs/stats", "GET") => ("run:*".into(), "read"),
        ("/v1/runs/{run_id}" | "/v1/runs/{run_id}/subscribe", "GET") => {
            (format!("run:{}", param("run_id")?), "read")
        }
        ("/v1/runs/{run_id}/calls", "DELETE") => (format!("call:{}/*", param("run_id")?), "delete"),
        // A stream's or environment's calls span runs the request does not name.
        ("/v1/streams/{stream_id}/calls" | "/v1/env/calls", "DELETE") => {
            ("call:*".into(), "delete")
        }
        ("/v1/sessions", "GET") => ("session:*".into(), "read"),
        ("/v1/sessions", "POST") => ("session:*".into(), "create"),
        ("/v1/sessions", "DELETE") => ("session:*".into(), "delete"),
        ("/v1/sessions/{session_id}", "DELETE") => {
            (format!("session:{}", param("session_id")?), "delete")
        }
        ("/v1/service-keys", "GET") => ("key:*".into(), "read"),
        ("/v1/service-keys", "POST") => ("key:*".into(), "create"),
        ("/v1/service-keys", "DELETE") => ("key:*".into(), "delete"),
        ("/v1/service-keys/{service_key_id}", "GET") => {
            (format!("key:{}", param("service_key_id")?), "read")
        }
        ("/v1/service-keys/{service_key_id}", "PATCH") => {
            (format!("key:{}", param("service_key_id")?), "update")
        }
        ("/v1/service-keys/{service_key_id}", "DELETE") => {
            (format!("key:{}", param("service_key_id")?), "delete")
        }
        ("/v1/domains", "GET") => ("domain:*".into(), "read"),
        ("/v1/domains", "POST") => ("domain:*".into(), "create"),
        ("/v1/domains/{domain_id}", "GET") => (format!("domain:{}", param("domain_id")?), "read"),
        ("/v1/domains/{domain_id}", "DELETE") => {
            (format!("domain:{}", param("domain_id")?), "delete")
        }
        ("/v1/domains/{domain_id}/verify", "POST") => {
            (format!("domain:{}", param("domain_id")?), "update")
        }
        ("/v1/env" | "/v1/env/subscribe" | "/v1/org/usage", "GET") => {
            (format!("env:{}", auth.env_id()), "read")
        }
        ("/v1/files", "GET") => ("file:*".into(), "read"),
        ("/v1/files/{file_id}" | "/v1/files/{file_id}/download", "GET") => {
            (format!("file:{}", param("file_id")?), "read")
        }
        ("/v1/files/{file_id}", "DELETE") => (format!("file:{}", param("file_id")?), "delete"),
        ("/v1/files/upload/{*path}", "PUT") => {
            // Authorize the same identity that storage will write. Checking the
            // raw URL lets a prefix grant escape via `allowed/../other`.
            let path = hot::file_storage::normalize_path(param("path")?)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            hot::file_storage::validate_path_security(&path)
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            (format!("file:{path}"), "create")
        }
        // Multipart IDs are upload sessions, not file identities. Until their
        // eventual target is resolved, require authority over all file uploads.
        ("/v1/files/uploads" | "/v1/files/uploads/{upload_id}/complete", "POST")
        | ("/v1/files/uploads/{upload_id}/{part_number}", "PUT") => ("file:*".into(), "create"),
        ("/v1/files/uploads/{upload_id}", "DELETE") => ("file:*".into(), "delete"),
        ("/v1/blobs/{blob_ref_id}/download", "GET") => {
            (format!("blob:{}", param("blob_ref_id")?), "read")
        }
        // New routes must declare an action policy before being exposed.
        _ => return Err(StatusCode::FORBIDDEN),
    };
    if auth.has_permission(&resource, action) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::db::api_key::ApiKey;
    use serde_json::json;
    use uuid::Uuid;

    fn principal(grants: serde_json::Value) -> AuthContext {
        let now = chrono::Utc::now();
        AuthContext::ApiKey(ApiKey {
            api_key_id: Uuid::now_v7(),
            env_id: Uuid::nil(),
            description: "policy".into(),
            key_data: json!({}),
            active: true,
            created_by_user_id: Uuid::now_v7(),
            created_at: now,
            updated_at: now,
            updated_by_user_id: None,
            active_toggle_at: None,
            active_toggle_by_user_id: None,
            permissions: grants,
        })
    }

    #[test]
    fn file_upload_grants_apply_to_the_normalized_storage_path() {
        let auth = principal(json!({"file:allowed/*": ["create"]}));
        for (path, expected) in [
            ("allowed/report.json", Ok(())),
            ("allowed/nested/../report.json", Ok(())),
            ("hot://allowed/report.json", Ok(())),
            ("allowed/../private/report.json", Err(StatusCode::FORBIDDEN)),
            (
                "allowed/../../private/report.json",
                Err(StatusCode::BAD_REQUEST),
            ),
        ] {
            let params = HashMap::from([("path".into(), path.into())]);
            assert_eq!(
                authorize(&auth, &Method::PUT, "/v1/files/upload/{*path}", &params),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn exact_resource_grants_do_not_authorize_collections_or_mutations() {
        let auth = principal(
            json!({"project:one":["read"], "run:r1":["read"], "context:one/secret":["update"]}),
        );
        let params = HashMap::from([
            ("project_id_or_slug".into(), "one".into()),
            ("run_id".into(), "r1".into()),
            ("key".into(), "secret".into()),
        ]);
        assert!(
            authorize(
                &auth,
                &Method::GET,
                "/v1/projects/{project_id_or_slug}",
                &params
            )
            .is_ok()
        );
        assert!(authorize(&auth, &Method::HEAD, "/v1/runs/{run_id}", &params).is_ok());
        assert!(
            authorize(
                &auth,
                &Method::PUT,
                "/v1/projects/{project_id_or_slug}/context/{key}",
                &params
            )
            .is_ok()
        );
        for (method, path) in [
            (Method::GET, "/v1/projects"),
            (Method::PATCH, "/v1/projects/{project_id_or_slug}"),
            (Method::GET, "/v1/runs"),
            (Method::DELETE, "/v1/runs/{run_id}/calls"),
            (Method::GET, "/v1/projects/{project_id_or_slug}/context"),
        ] {
            assert_eq!(
                authorize(&auth, &method, path, &params),
                Err(StatusCode::FORBIDDEN)
            );
        }
        let other = HashMap::from([("project_id_or_slug".into(), "two".into())]);
        assert_eq!(
            authorize(
                &auth,
                &Method::GET,
                "/v1/projects/{project_id_or_slug}",
                &other
            ),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn narrow_runtime_grants_never_imply_environment_administration() {
        let narrow = principal(json!({"event:*": ["read"]}));
        let full = principal(json!({"*:*": ["*"]}));
        let params = HashMap::from([
            ("project_id_or_slug".into(), "p".into()),
            ("build_id".into(), "b".into()),
            ("file_id".into(), "f".into()),
            ("domain_id".into(), "d".into()),
            ("blob_ref_id".into(), "blob".into()),
            ("run_id".into(), "r".into()),
        ]);
        for (method, path) in [
            (Method::GET, "/v1/org/usage"),
            (Method::GET, "/v1/env/subscribe"),
            (Method::POST, "/v1/projects"),
            (Method::GET, "/v1/projects/{project_id_or_slug}/context"),
            (
                Method::POST,
                "/v1/projects/{project_id_or_slug}/builds/{build_id}/deploy",
            ),
            (Method::POST, "/v1/sessions"),
            (Method::POST, "/v1/service-keys"),
            (Method::DELETE, "/v1/files/{file_id}"),
            (Method::GET, "/v1/blobs/{blob_ref_id}/download"),
            (Method::POST, "/v1/domains/{domain_id}/verify"),
            (Method::GET, "/v1/runs/{run_id}/subscribe"),
            (Method::DELETE, "/v1/runs/{run_id}/calls"),
            (Method::DELETE, "/v1/streams/{stream_id}/calls"),
            (Method::DELETE, "/v1/env/calls"),
        ] {
            assert_eq!(
                authorize(&narrow, &method, path, &params),
                Err(StatusCode::FORBIDDEN),
                "{method} {path}"
            );
            assert!(
                authorize(&full, &method, path, &params).is_ok(),
                "{method} {path}"
            );
        }
        assert_eq!(
            authorize(&full, &Method::GET, "/v1/unregistered", &params),
            Err(StatusCode::FORBIDDEN)
        );
    }
}

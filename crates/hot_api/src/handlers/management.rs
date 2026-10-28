//! Explicit root management API. Runtime credentials cannot authenticate here,
//! and management credentials cannot be used at runtime endpoints.
use super::ListQueryParams;
use crate::{
    ApiStateData,
    domain_resolver::ResolvedDomain,
    models::{ApiErrorResponse, ApiResponse},
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, patch, post},
};
use hot::db::management_key::{ManagementKey, ManagementKeyError};
use hot::db::org::{CreateChild, OrgLifecycle};
use hot::db::{BootstrapOperation, DatabaseScope, Org};
use serde::Deserialize;
use uuid::Uuid;

pub(crate) fn router(state: ApiStateData) -> Router<ApiStateData> {
    Router::new()
        .route(
            "/v1/management/children",
            get(list_children).post(create_child),
        )
        .route(
            "/v1/management/children/{child_id}/state",
            patch(set_child_state),
        )
        .route(
            "/v1/management/children/{child_id}/environments",
            post(create_child_environment),
        )
        .route(
            "/v1/management/children/{child_id}/environments/{env_id}/api-keys",
            post(issue_child_runtime_key),
        )
        .route("/v1/management/usage", get(get_account_usage))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .route_layer(middleware::from_fn_with_state(
            state,
            management_auth_middleware,
        ))
}

type ApiError = (StatusCode, Json<ApiErrorResponse>);
fn denied() -> ApiError {
    (
        StatusCode::FORBIDDEN,
        Json(ApiErrorResponse::new(
            "forbidden",
            "Management capability or target is not authorized",
        )),
    )
}
fn management_error(error: ManagementKeyError) -> ApiError {
    match error {
        ManagementKeyError::Unauthorized => denied(),
        ManagementKeyError::Invalid(message) => (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(&message)),
        ),
        ManagementKeyError::Database(error) => {
            tracing::error!(%error, "management operation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(
                    "Management operation failed",
                )),
            )
        }
    }
}
fn org_error(error: hot::db::org::OrgError) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiErrorResponse::bad_request(&error.to_string())),
    )
}

pub async fn management_auth_middleware(
    State((db, _, _, _)): State<ApiStateData>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.extensions().get::<ResolvedDomain>().is_some() {
        return Err(StatusCode::FORBIDDEN);
    }
    let token = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let key = ManagementKey::verify_token(&db, token)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    // Management calls spend the account's request budget like runtime calls.
    if let Err(exceeded) = crate::rate_limit::check_org_rate_limit(
        &db,
        &key.root_org_id,
        crate::rate_limit::PublicRateLimitMode::Enforce,
        "management",
    )
    .await
    {
        return Ok(crate::rate_limit::rate_limit_response(exceeded));
    }
    let audit_db = db.with_scope(DatabaseScope::Organization {
        org_id: key.root_org_id,
    });
    let metadata = serde_json::json!({
        "management_key_id": key.management_key_id,
        "root_org_id": key.root_org_id,
        "method": request.method().as_str(),
        "path": request.uri().path(),
    });
    hot::db::org_note::OrgNote::create_system(
        &audit_db,
        &key.root_org_id,
        "security",
        "management.request",
        "Management credential operation requested",
        Some(&metadata),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let root_id = key.root_org_id;
    request.extensions_mut().insert(key);
    let response = next.run(request).await;
    let mut result_metadata = metadata;
    result_metadata["status"] = serde_json::json!(response.status().as_u16());
    if let Err(error) = hot::db::org_note::OrgNote::create_system(
        &audit_db,
        &root_id,
        "security",
        "management.response",
        "Management credential operation completed",
        Some(&result_metadata),
    )
    .await
    {
        tracing::error!(%error, %root_id, "Failed to persist management response audit; request audit retained");
    }
    Ok(response)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateChildRequest {
    pub name: String,
    pub slug: String,
    pub external_id: Option<String>,
    pub idempotency_key: String,
}

pub async fn create_child(
    State((db, _, conf, _)): State<ApiStateData>,
    Extension(key): Extension<ManagementKey>,
    Json(body): Json<CreateChildRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    if !key.has_capability("children:create") || !key.all_children {
        return Err(denied());
    }
    hot::slug::validate_format_with_extra(&body.slug, &hot::slug::extra_reserved_from_conf(&conf))
        .map_err(|error| org_error(hot::db::org::OrgError::Invalid(error.message().to_string())))?;
    let request = CreateChild {
        name: body.name,
        slug: body.slug,
        external_id: body.external_id,
        idempotency_key: body.idempotency_key,
        hosted: hot::product::billing_enabled(&conf),
    };
    let child =
        Org::create_child_authorized(&db, &key.root_org_id, &key.created_by_user_id, &request)
            .await
            .map_err(org_error)?;
    tracing::info!(management_key_id = %key.management_key_id, root_org_id = %key.root_org_id, action = "child.create", "management mutation");
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(
            serde_json::to_value(child).map_err(|_| denied())?,
        )),
    ))
}

pub async fn list_children(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(key): Extension<ManagementKey>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    if !key.has_capability("children:read") {
        return Err(denied());
    }
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let children = Org::list_children(&lookup, &key.root_org_id)
        .await
        .map_err(org_error)?;
    let children: Vec<_> = children
        .into_iter()
        .filter(|child| key.all_children || key.target_org_ids.contains(&child.org_id))
        .collect();
    let total = children.len();
    let children: Vec<_> = children
        .into_iter()
        .skip(params.offset.max(0) as usize)
        .take(params.limit as usize)
        .collect();
    Ok(Json(ApiResponse::new(
        serde_json::json!({"children": children, "total": total}),
    )))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ChildStateRequest {
    pub state: String,
}

pub async fn set_child_state(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(key): Extension<ManagementKey>,
    Path(child_id): Path<Uuid>,
    Json(body): Json<ChildStateRequest>,
) -> Result<StatusCode, ApiError> {
    key.authorize_child(&db, child_id, "children:update", false)
        .await
        .map_err(management_error)?;
    let state = match body.state.as_str() {
        "active" => OrgLifecycle::Active,
        "suspended" => OrgLifecycle::Suspended,
        "archived" => OrgLifecycle::Archived,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(
                    "state must be active, suspended, or archived",
                )),
            ));
        }
    };
    Org::set_lifecycle_authorized(&db, &child_id, &key.created_by_user_id, state)
        .await
        .map_err(org_error)?;
    tracing::info!(management_key_id = %key.management_key_id, root_org_id = %key.root_org_id, target_org_id = %child_id, action = "child.lifecycle", "management mutation");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct IssueRuntimeKeyRequest {
    pub description: String,
    pub permissions: serde_json::Value,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateEnvironmentRequest {
    pub name: String,
}

pub async fn create_child_environment(
    State((db, _, conf, _)): State<ApiStateData>,
    Extension(key): Extension<ManagementKey>,
    Path(child_id): Path<Uuid>,
    Json(body): Json<CreateEnvironmentRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    key.authorize_child(&db, child_id, "environments:create", true)
        .await
        .map_err(management_error)?;
    let env = Org::provision_environment_authorized(
        &db,
        &child_id,
        &key.created_by_user_id,
        &body.name,
        hot::product::billing_enabled(&conf),
    )
    .await
    .map_err(org_error)?;
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(
            serde_json::json!({"env_id": env.env_id, "org_id": env.org_id, "name": env.name, "active": env.active}),
        )),
    ))
}

pub async fn issue_child_runtime_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(key): Extension<ManagementKey>,
    Path((child_id, env_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<IssueRuntimeKeyRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    let (id, token) = key
        .issue_runtime_key(&db, child_id, env_id, &body.description, &body.permissions)
        .await
        .map_err(management_error)?;
    tracing::info!(management_key_id = %key.management_key_id, root_org_id = %key.root_org_id, target_org_id = %child_id, %env_id, api_key_id = %id, action = "runtime-key.create", "management mutation");
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(
            serde_json::json!({"api_key_id": id, "env_id": env_id, "org_id": child_id, "token": token, "permissions": body.permissions}),
        )),
    ))
}

pub async fn get_account_usage(
    State((db, _, conf, _)): State<ApiStateData>,
    Extension(key): Extension<ManagementKey>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    if !key.has_capability("usage:read") || !key.all_children {
        return Err(denied());
    }
    let usage_db = db.with_scope(DatabaseScope::Maintenance {
        operation: hot::db::MaintenanceOperation::Billing,
        org_id: Some(key.root_org_id),
    });
    let (_, features, period_start, period_end) = super::org::account_plan_details(
        &usage_db,
        &key.root_org_id,
        hot::product::billing_enabled(&conf),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(
                "Account plan is unavailable",
            )),
        )
    })?;
    let usage = hot::db::subscription::OrgUsageStats::calculate_account(
        &usage_db,
        &key.root_org_id,
        period_start,
        features.call_retention_days(),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(
                "Account usage is unavailable",
            )),
        )
    })?;
    Ok(Json(ApiResponse::new(
        serde_json::json!({"root_org_id": key.root_org_id, "period_start": period_start, "period_end": period_end, "usage": usage}),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use hot::db::management_key::CreateManagementKey;
    use hot::db::{DatabasePool, Run};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn request(
        app: &Router,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    #[tokio::test]
    async fn management_router_rejects_reserved_child_slugs() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = db.as_ref() else {
            unreachable!("SQLite fixture");
        };
        sqlx::query("UPDATE org SET org_type = 'organization' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let (_, manager_token) = ManagementKey::create_for_user(
            &db,
            data.org_id,
            data.user_id,
            &CreateManagementKey {
                name: "reserved slugs".into(),
                capabilities: vec!["children:create".into()],
                target_org_ids: Vec::new(),
                all_children: true,
                delegation_permissions: None,
                expires_in_seconds: 3600,
            },
        )
        .await
        .unwrap();
        let storage_dir = tempfile::tempdir().unwrap();
        let state: ApiStateData = (
            db.clone(),
            Arc::new(Box::new(hot::storage::LocalBuildStorage::new(
                storage_dir.path().to_path_buf(),
            ))),
            Arc::new(hot::val!({"org": {"reserved-slugs": ["acme-internal"]}})),
            None,
        );
        let app = router(state.clone()).with_state(state);
        for slug in ["api", "acme-internal"] {
            let (status, body) = request(
                &app,
                "POST",
                "/v1/management/children",
                &manager_token,
                Some(json!({
                    "name": "Reserved",
                    "slug": slug,
                    "idempotency_key": format!("reserved-{slug}")
                })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{slug}: {body}");
        }
    }

    #[tokio::test]
    async fn management_router_provisions_idempotently_and_runtime_keys_stay_in_child_scope() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = db.as_ref() else {
            unreachable!("SQLite fixture");
        };
        sqlx::query("UPDATE org SET org_type = 'organization' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(pool)
            .await
            .unwrap();
        let permissions = json!({"event:*": ["read"], "env:*": ["read"], "run:*": ["read"]});
        let (manager, manager_token) = ManagementKey::create_for_user(
            &db,
            data.org_id,
            data.user_id,
            &CreateManagementKey {
                name: "provisioning integration test".into(),
                capabilities: vec![
                    "children:create".into(),
                    "children:read".into(),
                    "environments:create".into(),
                    "runtime-keys:create".into(),
                ],
                target_org_ids: Vec::new(),
                all_children: true,
                delegation_permissions: Some(permissions.clone()),
                expires_in_seconds: 3600,
            },
        )
        .await
        .unwrap();
        let storage_dir = tempfile::tempdir().unwrap();
        let state: ApiStateData = (
            db.clone(),
            Arc::new(Box::new(hot::storage::LocalBuildStorage::new(
                storage_dir.path().to_path_buf(),
            ))),
            Arc::new(hot::val!({})),
            None,
        );
        // The production management router and real runtime handlers/middleware
        // exercise token parsing, path extraction, lineage and scope checks.
        let runtime = Router::new()
            .route("/v1/org/usage", get(super::super::org::get_org_usage))
            .route("/v1/runs/{run_id}", get(super::super::runs::get_run))
            .route(
                "/v1/projects",
                get(super::super::projects::list_projects)
                    .post(super::super::projects::create_project),
            )
            .route(
                "/v1/projects/{project_id_or_slug}/context",
                get(super::super::context::list_context_variables)
                    .post(super::super::context::create_context_variable),
            )
            .route(
                "/v1/projects/{project_id_or_slug}/builds",
                post(super::super::builds::upload_build),
            )
            .route(
                "/v1/projects/{project_id_or_slug}/builds/{build_id}/deploy",
                post(super::super::builds::deploy_build),
            )
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                crate::auth::api_key_auth_middleware,
            ));
        let app = router(state.clone()).merge(runtime).with_state(state);
        let child_request = json!({
            "name": "First child",
            "slug": "first-child",
            "external_id": "customer-1",
            "idempotency_key": "create-customer-1"
        });
        let (status, child) = request(
            &app,
            "POST",
            "/v1/management/children",
            &manager_token,
            Some(child_request.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{child}");
        assert_eq!(child["data"]["created"], true);
        let child_id = child["data"]["org"]["org_id"].as_str().unwrap();
        let (status, repeated) = request(
            &app,
            "POST",
            "/v1/management/children",
            &manager_token,
            Some(child_request),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{repeated}");
        assert_eq!(repeated["data"]["created"], false);
        assert_eq!(repeated["data"]["org"]["org_id"], child_id);
        assert_eq!(repeated["data"]["env_id"], child["data"]["env_id"]);

        let env_path = format!("/v1/management/children/{child_id}/environments");
        let (status, environment) = request(
            &app,
            "POST",
            &env_path,
            &manager_token,
            Some(json!({"name": "production"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{environment}");
        let env_id = environment["data"]["env_id"].as_str().unwrap();
        let (status, repeated_env) = request(
            &app,
            "POST",
            &env_path,
            &manager_token,
            Some(json!({"name": "production"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{repeated_env}");
        assert_eq!(repeated_env["data"]["env_id"], env_id);

        let key_path = format!("{env_path}/{env_id}/api-keys");
        let (status, issued) = request(
            &app,
            "POST",
            &key_path,
            &manager_token,
            Some(json!({"description": "child runtime", "permissions": permissions})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let runtime_token = issued["data"]["token"].as_str().unwrap();
        let (status, usage) = request(&app, "GET", "/v1/org/usage", runtime_token, None).await;
        assert_eq!(status, StatusCode::OK, "{usage}");
        assert_eq!(usage["data"]["org_id"], child_id);
        assert_eq!(usage["data"]["env_id"], env_id);
        assert_eq!(usage["data"]["usage_scope"], "environment");
        assert_eq!(usage["data"]["limits_scope"], "billing_account");
        for (method, path, body) in [
            (
                "POST",
                "/v1/projects",
                Some(json!({"name":"not-authorized"})),
            ),
            ("GET", "/v1/projects/hidden/context", None),
            (
                "POST",
                "/v1/projects/hidden/context",
                Some(json!({"key":"secret","value":"no"})),
            ),
            ("POST", "/v1/projects/hidden/builds", None),
            (
                "POST",
                "/v1/projects/hidden/builds/00000000-0000-0000-0000-000000000000/deploy",
                None,
            ),
        ] {
            let (status, denied) = request(&app, method, path, runtime_token, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(denied["error"]["code"], "forbidden", "{method} {path}");
        }
        // Existing full-access user-issued keys retain administrative access.
        let legacy_id = Uuid::now_v7();
        let (legacy_token, legacy_hash) =
            hot::db::api_key::ApiKey::generate_api_key(&legacy_id).unwrap();
        hot::db::api_key::ApiKey::insert_api_key(
            &db,
            &legacy_id,
            &Uuid::parse_str(env_id).unwrap(),
            "legacy full access",
            &serde_json::from_str(&legacy_hash).unwrap(),
            &data.user_id,
            &json!({"*:*": ["*"]}),
        )
        .await
        .unwrap();
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/projects",
                &legacy_token,
                Some(json!({"name":"authorized"}))
            )
            .await
            .0,
            StatusCode::CREATED
        );
        assert_eq!(
            request(&app, "GET", "/v1/management/children", runtime_token, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", "/v1/org/usage", &manager_token, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );

        let (status, sibling) = request(
            &app,
            "POST",
            "/v1/management/children",
            &manager_token,
            Some(
                json!({"name": "Sibling", "slug": "sibling", "idempotency_key": "create-sibling"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{sibling}");
        let sibling_id = sibling["data"]["org"]["org_id"].as_str().unwrap();
        let sibling_env = sibling["data"]["env_id"].as_str().unwrap();
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("{env_path}/{sibling_env}/api-keys"),
                &manager_token,
                Some(json!({"description": "mismatched owner", "permissions": permissions})),
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        for selector in [
            format!("org_id={sibling_id}"),
            format!("env_id={sibling_env}"),
        ] {
            assert_eq!(
                request(
                    &app,
                    "GET",
                    &format!("/v1/org/usage?{selector}"),
                    runtime_token,
                    None
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
        let sibling_run_id = Uuid::now_v7();
        let sibling_env_id = Uuid::parse_str(sibling_env).unwrap();
        let sibling_db = db.with_scope(DatabaseScope::Environment {
            org_id: Uuid::parse_str(sibling_id).unwrap(),
            env_id: sibling_env_id,
        });
        let sibling_project_id = Uuid::now_v7();
        hot::db::Project::insert_project(
            &sibling_db,
            &sibling_project_id,
            &sibling_env_id,
            "sibling-project",
            &data.user_id,
        )
        .await
        .unwrap();
        let sibling_build_id = Uuid::now_v7();
        hot::db::Build::insert_build(
            &sibling_db,
            &sibling_build_id,
            &sibling_project_id,
            "sibling-build",
            0,
            hot::db::Build::BUILD_TYPE_LIVE,
            &data.user_id,
        )
        .await
        .unwrap();
        Run::insert_run(
            &sibling_db,
            &sibling_run_id,
            &sibling_env_id,
            &Uuid::now_v7(),
            Some(&sibling_build_id),
            hot::db::run::RunType::Run.as_id(),
            None,
            &data.user_id,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            request(
                &app,
                "GET",
                &format!("/v1/runs/{sibling_run_id}"),
                runtime_token,
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );

        sqlx::query(
            "UPDATE management_key SET delegation_permissions = ? WHERE management_key_id = ?",
        )
        .bind(json!({"event:*": ["read"]}).to_string())
        .bind(manager.management_key_id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(
            request(&app, "GET", "/v1/org/usage", runtime_token, None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&app, "GET", "/v1/projects", &legacy_token, None)
                .await
                .0,
            StatusCode::OK
        );

        ManagementKey::revoke_for_user(&db, data.org_id, data.user_id, manager.management_key_id)
            .await
            .unwrap();
        assert_eq!(
            request(&app, "GET", "/v1/org/usage", runtime_token, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[cfg(test)]
mod postgres_lifecycle;

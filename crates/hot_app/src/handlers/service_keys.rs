//! Service Keys dashboard handlers
//!
//! Manages service keys — long-lived, permission-scoped credentials
//! that Hot Dev customers issue to their own customers.

use crate::auth::Session;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect};
use hot::db::DatabasePool;
use hot::db::service_key::ServiceKey;
use std::sync::Arc;
use uuid::Uuid;

fn require_current_org_admin(session: &Session) -> Result<(), (StatusCode, String)> {
    if session.is_current_org_admin {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage service keys.".to_string(),
        ))
    }
}

fn service_keys_list_breadcrumbs(session: &Session) -> crate::templates::Breadcrumbs {
    let mut bc = crate::templates::build_base_breadcrumbs_with_env(session);
    bc.push(crate::templates::BreadcrumbItem::current(
        "Service Keys".to_string(),
    ));
    bc
}

fn service_keys_new_breadcrumbs(session: &Session) -> crate::templates::Breadcrumbs {
    let mut bc = crate::templates::build_base_breadcrumbs_with_env(session);
    bc.push(crate::templates::BreadcrumbItem::clickable(
        "Service Keys".to_string(),
        "/service-keys".to_string(),
    ));
    bc.push(crate::templates::BreadcrumbItem::current(
        "Create".to_string(),
    ));
    bc
}

fn service_keys_detail_breadcrumbs(
    session: &Session,
    label: &str,
) -> crate::templates::Breadcrumbs {
    let mut bc = crate::templates::build_base_breadcrumbs_with_env(session);
    bc.push(crate::templates::BreadcrumbItem::clickable(
        "Service Keys".to_string(),
        "/service-keys".to_string(),
    ));
    bc.push(crate::templates::BreadcrumbItem::current(label.to_string()));
    bc
}

/// GET /service-keys — list all service keys for the current environment
pub async fn service_keys_list_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    require_current_org_admin(&session)?;

    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let service_keys = ServiceKey::list_by_env(&db, &env.env_id)
        .await
        .unwrap_or_else(|_| vec![]);

    let template = crate::templates::ServiceKeysList {
        title: "Service Keys",
        page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
            "service_keys",
            &session,
            service_keys_list_breadcrumbs(&session),
        ),
        service_keys,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// GET /service-keys/new — show create service key form
pub async fn service_keys_new_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    require_current_org_admin(&session)?;

    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let (mcp_tools_json, webhooks_json) =
        super::build_permission_path_options(&db, &env.env_id).await;

    let template = crate::templates::ServiceKeysNew {
        title: "Create Service Key",
        page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
            "service_keys",
            &session,
            service_keys_new_breadcrumbs(&session),
        ),
        error_message: "",
        generated_key: None,
        service_key_id: None,
        mcp_tools_json,
        webhooks_json,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// POST /service-keys/new — create a new service key
pub async fn service_keys_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    axum::extract::Form(form): axum::extract::Form<CreateServiceKeyForm>,
) -> Result<Html<String>, (StatusCode, String)> {
    require_current_org_admin(&session)?;

    // Feature gate: service keys require Pro+ plan
    if !session.current_org_features.has_service_keys() {
        return Err((
            StatusCode::FORBIDDEN,
            "Service keys require a Pro or Scale plan.".to_string(),
        ));
    }

    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    // For dashboard-created service keys, we need an API key to associate with.
    // Use the most recent active API key for this environment.
    let api_key = hot::db::api_key::ApiKey::get_active_api_key_by_env(&db, &env.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get API keys: {}", e),
            )
        })?
        .ok_or((
            StatusCode::BAD_REQUEST,
            "No active API key found for this environment. Create or enable an API key first."
                .to_string(),
        ))?;

    // Parse and validate permissions from form (default to empty)
    let permissions = if form.permissions.is_empty() {
        hot::permission::Permissions::new()
    } else {
        let parsed: serde_json::Value = serde_json::from_str(&form.permissions).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid permissions JSON: {}", e),
            )
        })?;
        // Validate the permissions structure
        hot::permission::Permissions::from_json_validated(&parsed).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid permissions: {}", e),
            )
        })?
    };
    permissions
        .validate_resource_types(hot::permission::resource_types::SERVICE_KEY_ALLOWED)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid permissions: {}", e),
            )
        })?;
    let parent_permissions = api_key.get_permissions().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid parent API key permissions: {}", e),
        )
    })?;
    permissions
        .validate_subset_of(&parent_permissions)
        .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    let permissions = permissions.to_json();

    // Parse expiration
    let expires_at = if form.expires_in_days > 0 {
        Some(chrono::Utc::now() + chrono::Duration::days(form.expires_in_days))
    } else {
        None // No expiration
    };

    // Encrypt metadata if provided
    let encrypted_metadata = if let Some(ref meta_str) = form.metadata
        && !meta_str.trim().is_empty()
    {
        let meta_json: serde_json::Value = serde_json::from_str(meta_str).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid metadata JSON: {}", e),
            )
        })?;
        if let Ok(enc) = hot::context_encryption::ContextEncryption::from_env_or_existing_dev_key()
        {
            Some(
                ServiceKey::encrypt_metadata(&meta_json, &enc, &env.org_id).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to encrypt metadata: {}", e),
                    )
                })?,
            )
        } else {
            // No encryption configured — store raw JSON string (local dev)
            Some(meta_str.trim().to_string())
        }
    } else {
        None
    };

    match ServiceKey::create(
        &db,
        &api_key.api_key_id,
        &env.env_id,
        form.name.as_deref(),
        form.description.as_deref(),
        &permissions,
        encrypted_metadata.as_deref(),
        expires_at,
    )
    .await
    {
        Ok((service_key, token)) => {
            let template = crate::templates::ServiceKeysNew {
                title: "Service Key Created",
                page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
                    "service_keys",
                    &session,
                    service_keys_new_breadcrumbs(&session),
                ),
                error_message: "",
                generated_key: Some(token),
                service_key_id: Some(service_key.service_key_id),
                mcp_tools_json: "[]".into(),
                webhooks_json: "[]".into(),
            };

            Ok(Html(
                template
                    .render()
                    .unwrap_or_else(|_| "Template error".into()),
            ))
        }
        Err(e) => {
            let template = crate::templates::ServiceKeysNew {
                title: "Create Service Key",
                page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
                    "service_keys",
                    &session,
                    service_keys_new_breadcrumbs(&session),
                ),
                error_message: &format!("Failed to create service key: {}", e),
                generated_key: None,
                service_key_id: None,
                mcp_tools_json: "[]".into(),
                webhooks_json: "[]".into(),
            };

            Ok(Html(
                template
                    .render()
                    .unwrap_or_else(|_| "Template error".into()),
            ))
        }
    }
}

/// GET /service-keys/{key_id} — view service key detail
pub async fn service_keys_detail_handler(
    Path(key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    require_current_org_admin(&session)?;

    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    let service_key = ServiceKey::get_service_key(&db, &key_id)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Service key not found".to_string()))?;

    // Verify it belongs to the current environment
    if service_key.env_id != env.env_id {
        return Err((StatusCode::NOT_FOUND, "Service key not found".to_string()));
    }

    let permission_rows = crate::templates::parse_permissions_for_display(&service_key.permissions);

    // Decrypt metadata for display
    let decrypted_metadata = if let Ok(enc) =
        hot::context_encryption::ContextEncryption::from_env_or_existing_dev_key()
    {
        service_key
            .get_decrypted_metadata(&enc, &env.org_id)
            .ok()
            .flatten()
    } else {
        // No encryption — try to parse raw metadata (local dev)
        service_key
            .metadata
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    };

    let detail_label = service_key.name.as_deref().unwrap_or("Detail").to_string();

    let template = crate::templates::ServiceKeyDetail {
        title: "Service Key",
        page_context: crate::templates::PrivatePageContext::with_breadcrumbs(
            "service_keys",
            &session,
            service_keys_detail_breadcrumbs(&session, &detail_label),
        ),
        service_key,
        permission_rows,
        metadata_json: decrypted_metadata
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()),
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// POST /service-keys/{key_id}/revoke — revoke a service key
pub async fn service_keys_revoke_handler(
    Path(key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, (StatusCode, String)> {
    require_current_org_admin(&session)?;

    let env = session.current_env.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No environment selected".to_string(),
    ))?;

    // Verify the key exists and belongs to the current environment
    let service_key = ServiceKey::get_service_key(&db, &key_id)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Service key not found".to_string()))?;

    if service_key.env_id != env.env_id {
        return Err((StatusCode::NOT_FOUND, "Service key not found".to_string()));
    }

    ServiceKey::revoke(&db, &key_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to revoke service key: {}", e),
        )
    })?;

    Ok(Redirect::to("/service-keys"))
}

#[derive(serde::Deserialize)]
pub struct CreateServiceKeyForm {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub expires_in_days: i64,
    #[serde(default)]
    pub metadata: Option<String>,
}

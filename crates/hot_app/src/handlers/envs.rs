use crate::auth::{AppState, Session};
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect};
use futures::stream::Stream;
use hot::db::DatabasePool;
use hot::stream::{EnvEvent as PubSubEnvEvent, EnvSubscriber, EnvSubscriberFactory};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use uuid::Uuid;

// Form data structure for environment creation/editing
#[derive(Deserialize, Debug)]
pub struct EnvForm {
    pub name: String,
}

pub async fn envs_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> / Environments
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Environments".to_string(),
    ));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    const ENVS_PER_PAGE: i64 = 10;

    // Calculate offset
    let offset = (current_page_num - 1) * ENVS_PER_PAGE;

    // Get environments for current organization
    let (envs, total_envs) = if let Some(org) = &session.current_org {
        // Get all environments first to get total count
        let all_envs = hot::db::Env::get_envs_by_org(&db, &org.org_id)
            .await
            .unwrap_or_default();
        let total = all_envs.len() as i64;

        // Apply pagination manually
        let start_index = offset as usize;
        let end_index = std::cmp::min(start_index + ENVS_PER_PAGE as usize, all_envs.len());
        let paginated_envs = if start_index < all_envs.len() {
            all_envs[start_index..end_index].to_vec()
        } else {
            Vec::new()
        };

        (paginated_envs, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = if total_envs > 0 {
        (total_envs + ENVS_PER_PAGE - 1) / ENVS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::EnvsList {
        title: "Environments",
        page_context: templates::PrivatePageContext::for_org_page("envs", &session, breadcrumbs),
        envs,
        is_admin: session.is_current_org_admin,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_envs,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn envs_new_handler(
    State(_db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> / Environments / New
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Environments".to_string(),
        "/envs".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = templates::EnvsNew {
        title: "New Environment",
        page_context: templates::PrivatePageContext::for_org_page("envs", &session, breadcrumbs),
        error_message: "",
        name: "",
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn envs_edit_handler(
    Path(env_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get current org_id for access check
    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Redirect::to("/envs").into_response();
        }
    };

    // Get environment details
    match hot::db::Env::get_env(&db, &env_id).await {
        Ok(env) => {
            // SECURITY: Verify the environment belongs to the current organization
            if env.org_id != current_org.org_id {
                return Redirect::to("/envs").into_response();
            }

            // Build breadcrumbs: <org> / Environments / <env_name> / Edit
            let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Environments".to_string(),
                "/envs".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                env.name.clone(),
                format!("/envs/{}", env_id),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

            let template = templates::EnvsEdit {
                title: &format!("Edit Environment: {}", env.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "envs",
                    &session,
                    breadcrumbs,
                ),
                env,
                error_message: "",
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Environment not found, redirect to envs list
            Redirect::to("/envs").into_response()
        }
    }
}

pub async fn envs_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<EnvForm>,
) -> Result<Redirect, Html<String>> {
    // Local-dev experience is single-user oriented; self-host can create environments.
    if session.is_local_dev_experience() {
        return Err(render_envs_new_with_error(
            &session,
            "Creating environments is not available in local development.",
            &form.name,
        ));
    }

    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Err(Html("No organization selected".to_string()));
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Err(Html(
            "You must be an admin to create environments".to_string(),
        ));
    }

    // Validate form data
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err(render_envs_new_with_error(
            &session,
            "Environment name is required",
            &form.name,
        ));
    }

    // Validate URL-safe format (lowercase letters, numbers, hyphens only)
    if !is_valid_env_name(&name) {
        return Err(render_envs_new_with_error(
            &session,
            "Environment name can only contain lowercase letters, numbers, and hyphens",
            &form.name,
        ));
    }

    // Check if environment with this name already exists in the organization
    let existing_envs = match hot::db::Env::get_envs_by_org(&db, &current_org.org_id).await {
        Ok(envs) => envs,
        Err(_) => {
            return Err(render_envs_new_with_error(
                &session,
                "Failed to check existing environments",
                &form.name,
            ));
        }
    };

    if existing_envs.iter().any(|env| env.name == form.name) {
        return Err(render_envs_new_with_error(
            &session,
            "Environment with this name already exists",
            &form.name,
        ));
    }

    // Create the environment
    let env_id = uuid::Uuid::now_v7();
    match hot::db::Env::insert_env(
        &db,
        &env_id,
        &current_org.org_id,
        &form.name,
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Redirect to the environments list
            Ok(Redirect::to("/envs"))
        }
        Err(_) => Err(render_envs_new_with_error(
            &session,
            "Failed to create environment",
            &form.name,
        )),
    }
}

pub async fn envs_update_handler(
    Path(env_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<EnvForm>,
) -> Result<Redirect, Html<String>> {
    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Err(Html("No organization selected".to_string()));
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Err(Html(
            "You must be an admin to edit environments".to_string(),
        ));
    }

    // Get the environment
    let env = match hot::db::Env::get_env(&db, &env_id).await {
        Ok(env) => env,
        Err(_) => return Ok(Redirect::to("/envs")),
    };

    // Check if environment belongs to current organization
    if env.org_id != current_org.org_id {
        return Ok(Redirect::to("/envs"));
    }

    // Validate form data
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err(render_envs_edit_with_error(
            &session,
            &env,
            "Environment name is required",
        ));
    }

    // Validate URL-safe format (lowercase letters, numbers, hyphens only)
    if !is_valid_env_name(&name) {
        return Err(render_envs_edit_with_error(
            &session,
            &env,
            "Environment name can only contain lowercase letters, numbers, and hyphens",
        ));
    }

    // Check if environment with this name already exists in the organization (but allow current name)
    if form.name != env.name {
        let existing_envs = match hot::db::Env::get_envs_by_org(&db, &current_org.org_id).await {
            Ok(envs) => envs,
            Err(_) => {
                return Err(render_envs_edit_with_error(
                    &session,
                    &env,
                    "Failed to check existing environments",
                ));
            }
        };

        if existing_envs.iter().any(|e| e.name == form.name) {
            return Err(render_envs_edit_with_error(
                &session,
                &env,
                "Environment with this name already exists",
            ));
        }
    }

    // Update the environment
    match hot::db::Env::update_env(&db, &env_id, &form.name, &session.current_user_id()).await {
        Ok(_) => {
            // Redirect to the environments list
            Ok(Redirect::to("/envs"))
        }
        Err(_) => Err(render_envs_edit_with_error(
            &session,
            &env,
            "Failed to update environment",
        )),
    }
}

// Helper function to render envs new page with error
fn render_envs_new_with_error(session: &Session, error_message: &str, name: &str) -> Html<String> {
    // Build breadcrumbs: <org> / Environments / New
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Environments".to_string(),
        "/envs".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = templates::EnvsNew {
        title: "New Environment",
        page_context: templates::PrivatePageContext::for_org_page("envs", session, breadcrumbs),
        error_message,
        name,
    };

    Html(template.render().unwrap())
}

// Helper function to render envs edit page with error
fn render_envs_edit_with_error(
    session: &Session,
    env: &hot::db::env::Env,
    error_message: &str,
) -> Html<String> {
    // Build breadcrumbs: <org> / Environments / <env_name> / Edit
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Environments".to_string(),
        "/envs".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::new(env.name.clone(), None));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::EnvsEdit {
        title: &format!("Edit Environment: {}", env.name),
        page_context: templates::PrivatePageContext::for_org_page("envs", session, breadcrumbs),
        env: env.clone(),
        error_message,
    };

    Html(template.render().unwrap())
}

// ============================================================================
// Validation
// ============================================================================

/// Validate that an environment name is URL-safe.
/// Allows lowercase ASCII letters, digits, and hyphens (same rules as org slugs).
fn is_valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ============================================================================
// Environment SSE Subscription (Real-time Dashboard Updates)
// ============================================================================

/// SSE event types for environment subscription
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum EnvSseEvent {
    #[serde(rename = "run:start")]
    RunStart {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
    },
    #[serde(rename = "run:stop")]
    RunStop {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
        duration_ms: Option<i64>,
    },
    #[serde(rename = "run:fail")]
    RunFail {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
        duration_ms: Option<i64>,
        error: Option<String>,
    },
    #[serde(rename = "run:cancel")]
    RunCancel {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
        duration_ms: Option<i64>,
        reason: Option<String>,
    },
    #[serde(rename = "event:created")]
    EventCreated {
        event_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        project_id: Option<Uuid>,
    },
    #[serde(rename = "event:handled")]
    EventHandled {
        event_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        project_id: Option<Uuid>,
    },
    #[serde(rename = "stream:created")]
    StreamCreated {
        stream_id: Uuid,
        env_id: Uuid,
        project_id: Option<Uuid>,
    },
    #[serde(rename = "task:started")]
    TaskStarted {
        task_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        function_name: String,
        task_type: String,
    },
    #[serde(rename = "task:complete")]
    TaskComplete {
        task_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        function_name: String,
        status: String,
        duration_ms: Option<i64>,
        error: Option<serde_json::Value>,
    },
}

/// Subscribe to environment events via Server-Sent Events
///
/// This endpoint provides real-time updates for all runs, events, and streams
/// in the current environment. Used by the dashboard for live updates.
pub async fn env_subscribe_handler(
    State(app_state): State<AppState>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, (axum::http::StatusCode, String)>
{
    // Get current environment from session
    let env_id = match &session.current_env {
        Some(env) => env.env_id,
        None => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "No environment selected".to_string(),
            ));
        }
    };

    // Clone shutdown receiver for use in the stream
    let mut shutdown_rx = app_state.shutdown_rx.clone();

    // Try to subscribe to pub/sub for real-time updates
    let subscriber: Option<Box<dyn EnvSubscriber>> =
        if let Some(ref pubsub) = app_state.stream_pubsub {
            match pubsub.subscribe_env(env_id).await {
                Ok(sub) => {
                    tracing::debug!(
                        "Dashboard SSE handler subscribed to env pub/sub for {}",
                        env_id
                    );
                    Some(sub)
                }
                Err(e) => {
                    tracing::debug!(
                        "Dashboard SSE handler failed to subscribe to env {} (pub/sub error: {})",
                        env_id,
                        e
                    );
                    None
                }
            }
        } else {
            tracing::debug!(
                "Dashboard SSE handler has no pub/sub configured for env {}",
                env_id
            );
            None
        };

    // If no pub/sub available, return an error
    if subscriber.is_none() {
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Real-time updates not available".to_string(),
        ));
    }

    // Create the SSE stream with push from pub/sub
    // The stream will terminate when:
    // 1. The stream timeout is reached (5 minutes)
    // 2. The server is shutting down (shutdown signal received)
    // 3. The subscriber returns None (closed/error)
    let stream = async_stream::stream! {
        let mut subscriber = subscriber;
        let stream_timeout = tokio::time::Duration::from_secs(300); // 5 minute timeout
        let start_time = tokio::time::Instant::now();

        loop {
            // Check for timeout
            if start_time.elapsed() > stream_timeout {
                tracing::debug!("Dashboard SSE env subscription timed out for env {}", env_id);
                break;
            }

            // Receive push event from pub/sub, or shutdown signal
            if let Some(ref mut sub) = subscriber {
                tokio::select! {
                    // Check for shutdown signal
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            tracing::debug!("Dashboard SSE handler received shutdown signal for env {}", env_id);
                            break;
                        }
                    }
                    // Wait for next event from subscriber
                    event = sub.next() => {
                        match event {
                            Some(event) => {
                                // Convert pub/sub event to SSE event
                                let (event_type, sse_event) = convert_pubsub_to_sse(event);
                                if let Ok(json) = serde_json::to_string(&sse_event) {
                                    tracing::debug!("Dashboard SSE push: {} for env {}", event_type, env_id);
                                    yield Ok(SseEvent::default().event(event_type).data(json));
                                }
                            }
                            None => {
                                // Subscriber returned None - could be timeout or closed
                                tracing::trace!("Dashboard SSE env subscription poll returned None for env {}", env_id);
                                continue;
                            }
                        }
                    }
                }
            } else {
                break;
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Convert pub/sub EnvEvent to SSE event
fn convert_pubsub_to_sse(event: PubSubEnvEvent) -> (&'static str, EnvSseEvent) {
    match event {
        PubSubEnvEvent::RunStart {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
        } => (
            "run:start",
            EnvSseEvent::RunStart {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
            },
        ),
        PubSubEnvEvent::RunStop {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
            duration_ms,
        } => (
            "run:stop",
            EnvSseEvent::RunStop {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
                duration_ms,
            },
        ),
        PubSubEnvEvent::RunFail {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
            duration_ms,
            error,
        } => (
            "run:fail",
            EnvSseEvent::RunFail {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
                duration_ms,
                error,
            },
        ),
        PubSubEnvEvent::RunCancel {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
            duration_ms,
            reason,
        } => (
            "run:cancel",
            EnvSseEvent::RunCancel {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
                duration_ms,
                reason,
            },
        ),
        PubSubEnvEvent::EventCreated {
            event_id,
            env_id,
            stream_id,
            event_type,
            project_id,
        } => (
            "event:created",
            EnvSseEvent::EventCreated {
                event_id,
                env_id,
                stream_id,
                event_type,
                project_id,
            },
        ),
        PubSubEnvEvent::EventHandled {
            event_id,
            env_id,
            stream_id,
            event_type,
            project_id,
        } => (
            "event:handled",
            EnvSseEvent::EventHandled {
                event_id,
                env_id,
                stream_id,
                event_type,
                project_id,
            },
        ),
        PubSubEnvEvent::StreamCreated {
            stream_id,
            env_id,
            project_id,
        } => (
            "stream:created",
            EnvSseEvent::StreamCreated {
                stream_id,
                env_id,
                project_id,
            },
        ),
        PubSubEnvEvent::TaskStarted {
            task_id,
            env_id,
            stream_id,
            function_name,
            task_type,
        } => (
            "task:started",
            EnvSseEvent::TaskStarted {
                task_id,
                env_id,
                stream_id,
                function_name,
                task_type,
            },
        ),
        PubSubEnvEvent::TaskComplete {
            task_id,
            env_id,
            stream_id,
            function_name,
            status,
            duration_ms,
            error,
        } => (
            "task:complete",
            EnvSseEvent::TaskComplete {
                task_id,
                env_id,
                stream_id,
                function_name,
                status,
                duration_ms,
                error,
            },
        ),
    }
}

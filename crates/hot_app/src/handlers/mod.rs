// Module declarations for refactored handlers
pub mod account;
pub mod agent_graph;
pub mod agents;
pub mod alerts;
pub mod auth;
pub mod billing;
pub mod billing_form;
pub mod contexts;
pub mod dashboard;
pub mod data;
pub mod docs;
pub mod domains;
pub mod envs;
pub mod events;
pub mod files;
pub mod hierarchy;
pub mod invites;
pub mod keys;
pub mod mcp_tools;
pub mod oauth;
pub mod orgs;
pub mod projects;
pub mod runs;
pub mod schedules;
pub mod service_keys;
pub mod source_browser;
pub mod stream_graph;
pub mod streams;
pub mod tasks;
pub mod teams;
pub mod webhooks;

// Re-export handlers from modules for backward compatibility
pub use account::{
    account_handler, account_update_handler, notifications_handler, notifications_update_handler,
};
pub use agent_graph::{
    agent_graph_data_handler, agent_graph_detail_data_handler,
    unnamed_workflow_graph_detail_data_handler, workflow_graph_detail_data_handler,
};
pub use agents::{
    agents_detail_handler, agents_list_handler, unnamed_workflow_detail_handler,
    workflow_detail_handler,
};
pub use alerts::{
    channels_create_handler, channels_delete_handler, channels_edit_handler, channels_list_handler,
    channels_new_handler, channels_update_handler, destinations_create_handler,
    destinations_delete_handler, destinations_edit_handler, destinations_list_handler,
    destinations_new_handler, destinations_update_handler, history_detail_handler,
    history_list_handler, resend_destination_verification_handler, subscriptions_create_handler,
    subscriptions_delete_handler, subscriptions_edit_handler, subscriptions_list_handler,
    subscriptions_new_handler, subscriptions_update_handler, verify_alert_destination_handler,
};
pub use auth::{
    claim_handle_handler, claim_handle_post_handler, resend_verification_handler, signin_handler,
    signin_post_handler, signout_handler, signout_page_handler, signup_handler,
    signup_plans_handler, signup_post_handler, verify_email_handler,
};
pub use billing::{
    account_billing_handler, billing_webhook_handler, cancel_subscription_handler,
    checkout_cancel_handler, checkout_success_handler, create_checkout_handler,
    reactivate_subscription_handler, usage_stats_handler, view_billing_handler, view_usage_handler,
};
pub use billing_form::{
    checkout_form_handler, org_checkout_form_handler, org_create_checkout_handler,
};
pub use contexts::{
    contexts_create_handler, contexts_delete_handler, contexts_edit_handler,
    contexts_index_handler, contexts_list_handler, contexts_new_handler, contexts_update_handler,
};
pub use dashboard::{
    agent_health_widget_handler, cancelled_runs_widget_handler, dashboard_handler,
    failed_runs_widget_handler, failed_tasks_widget_handler, getting_started_widget_handler,
    recent_events_widget_handler, recent_runs_widget_handler, recent_streams_widget_handler,
    recent_tasks_widget_handler, unhandled_events_widget_handler,
};
pub use data::{
    event_run_relationships_handler, event_timeline_handler, filtered_type_summary_handler,
    run_type_data_handler, status_chart_data_handler, stream_flow_handler, stream_metrics_handler,
    stream_timeline_handler,
};
pub use docs::{
    docs_index_handler, docs_search_handler, pkg_route_handler, project_docs_index_handler,
    project_namespace_handler,
};
pub use envs::{
    env_subscribe_handler, envs_create_handler, envs_edit_handler, envs_list_handler,
    envs_new_handler, envs_update_handler,
};
pub use events::{
    event_detail_table_handler, event_json_handler, events_detail_handler, events_list_handler,
};
pub use files::{
    file_detail_handler, file_download_handler, files_list_handler, run_files_handler,
};
pub use hierarchy::get_hierarchy_handler;
pub use invites::{invite_accept_handler, invite_accept_post_handler};
pub use keys::{
    keys_create_handler, keys_edit_handler, keys_list_handler, keys_new_handler,
    keys_update_handler,
};
pub use mcp_tools::{mcp_service_detail_handler, mcp_services_list_handler};
pub use oauth::{
    github_auth_handler, github_callback_handler, google_auth_handler, google_callback_handler,
};
pub use orgs::{
    legacy_org_redirect, org_users_edit_handler, org_users_edit_post_handler,
    org_users_invite_handler, org_users_invite_post_handler, org_users_list_handler,
    orgs_create_handler, orgs_detail_handler, orgs_edit_handler, orgs_list_handler,
    orgs_new_handler, orgs_update_handler,
};
pub use runs::{
    run_detail_handler, run_json_handler, run_rerun_handler, run_retry_handler,
    run_tasks_tab_handler, runs_list_handler,
};
pub use schedules::{event_handlers_list_handler, schedule_detail_handler, schedules_list_handler};
pub use source_browser::{source_file_handler, source_search_handler, source_tree_handler};
pub use streams::{stream_detail_handler, streams_list_handler};
pub use tasks::{task_detail_handler, tasks_list_handler};
pub use teams::{
    team_users_add_handler, team_users_add_post_handler, team_users_edit_handler,
    team_users_edit_post_handler, team_users_list_handler, team_users_remove_post_handler,
    teams_create_handler, teams_detail_handler, teams_edit_handler, teams_list_handler,
    teams_new_handler, teams_update_handler,
};
pub use webhooks::{webhook_service_detail_handler, webhook_services_list_handler};

// Common imports that remain in this file
use crate::auth::Session;
use ahash::AHashMap;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, Json, Redirect};
use axum_extra::extract::CookieJar;
use hot::db::{DatabasePool, User, UserAuth, UserError};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use time;
use url::Url;
use uuid::Uuid;

fn is_safe_relative_redirect(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//")
}

// Helper function to get redirect URL from referrer header
fn get_redirect_url_from_referrer(headers: &HeaderMap) -> String {
    let host = headers.get("host").and_then(|h| h.to_str().ok());

    if let Some(referrer) = headers.get("referer").or_else(|| headers.get("referrer"))
        && let Ok(referrer_str) = referrer.to_str()
    {
        if is_safe_relative_redirect(referrer_str) {
            return referrer_str.to_string();
        }

        if let (Some(host), Ok(url)) = (host, Url::parse(referrer_str)) {
            let referrer_host = match url.port() {
                Some(port) => format!("{}:{}", url.host_str().unwrap_or_default(), port),
                None => url.host_str().unwrap_or_default().to_string(),
            };

            if referrer_host.eq_ignore_ascii_case(host) {
                let mut path = url.path().to_string();
                if let Some(query) = url.query() {
                    path.push('?');
                    path.push_str(query);
                }
                if is_safe_relative_redirect(&path) {
                    return path;
                }
            }
        }
    }

    // Default to dashboard if no safe referrer
    "/".to_string()
}

// Helper function to format value for display
pub fn format_value_for_display(value: &Option<serde_json::Value>, raw_mode: bool) -> String {
    match value {
        Some(val) => {
            if raw_mode {
                // Raw mode: show original JSON
                val.to_string()
            } else {
                // Pretty mode: Try to format using metadata formatter first
                format_value_with_metadata(val)
            }
        }
        None => "null".to_string(),
    }
}

// Helper function to format value with metadata formatter
fn format_value_with_metadata(json_val: &JsonValue) -> String {
    // Try to convert JSON to Val and format as Hot code
    if let Ok(hot_val) = serde_json::from_value::<hot::val::Val>(json_val.clone()) {
        // Check if this is an enriched value (has runtime_value and metadata)
        if let hot::val::Val::Map(ref map) = hot_val {
            // Check if it has the enriched structure
            if map.contains_key(&hot::val::Val::from("metadata"))
                && map.contains_key(&hot::val::Val::from("runtime_value"))
            {
                // Use the Hot display formatter.
                if let Some(formatted) = hot::lang::display::format_as_hot_code(&hot_val) {
                    return formatted;
                }
            }
        }
    }

    // Fall back to the old pretty formatting logic
    format_value_pretty(json_val)
}

// Helper function to format value in pretty mode
fn format_value_pretty(val: &JsonValue) -> String {
    // Try to unwrap and format functions
    let unwrapped = unwrap_value(val);

    // Check if it's a function after unwrapping
    if let Some(formatted_fn) = format_function_from_json(&unwrapped) {
        return formatted_fn;
    }

    // For other types, return formatted JSON
    unwrapped.to_string()
}

// Helper function to unwrap nested values (hot results, boxes, etc.)
fn unwrap_value(val: &JsonValue) -> JsonValue {
    let mut current = val.clone();

    // Keep unwrapping until we can't unwrap anymore
    loop {
        let next = unwrap_single_layer(&current);
        if next == current {
            break; // No more unwrapping possible
        }
        current = next;
    }

    current
}

// Helper function to unwrap a single layer
fn unwrap_single_layer(val: &JsonValue) -> JsonValue {
    // Try unwrapping hot result variant: { $type: "::hot::type/Result.Ok", $val: value }
    if let Some(obj) = val.as_object() {
        if let Some(JsonValue::String(type_str)) = obj.get("$type")
            && type_str == "::hot::type/Result.Ok"
            && let Some(val_inner) = obj.get("$val")
        {
            return val_inner.clone();
        }
        // Note: We don't unwrap Result.Err here - errors should be preserved

        // Try unwrapping box
        if let Some(box_val) = obj.get("$box") {
            return box_val.clone();
        }
    }

    val.clone()
}

// Helper function to format function from JSON
fn format_function_from_json(val: &JsonValue) -> Option<String> {
    if let Some(obj) = val.as_object()
        && let Some(fn_array) = obj.get("Fn").and_then(|v| v.as_array())
    {
        if fn_array.len() == 1 {
            if let Some(def) = fn_array[0].as_object()
                && let Some(args_array) = def.get("args").and_then(|v| v.as_array())
            {
                if args_array.is_empty() {
                    return Some("fn ()".to_string());
                } else {
                    let arg_names: Vec<String> = args_array.iter().map(extract_arg_name).collect();
                    return Some(format!("fn ({})", arg_names.join(", ")));
                }
            }
        } else if fn_array.len() > 1 {
            let signatures: Vec<String> = fn_array
                .iter()
                .filter_map(|def| def.as_object())
                .map(|def| {
                    if let Some(args_array) = def.get("args").and_then(|v| v.as_array()) {
                        if args_array.is_empty() {
                            "()".to_string()
                        } else {
                            let arg_names: Vec<String> =
                                args_array.iter().map(extract_arg_name).collect();
                            format!("({})", arg_names.join(", "))
                        }
                    } else {
                        "()".to_string()
                    }
                })
                .collect();
            return Some(format!("fn {}", signatures.join(", ")));
        }
    }
    None
}

// Helper function to extract argument name from complex structure
fn extract_arg_name(arg: &JsonValue) -> String {
    // Handle Ref -> VarRef -> Var -> sym -> String structure
    if let Some(obj) = arg.as_object() {
        if let Some(ref_obj) = obj.get("Ref").and_then(|v| v.as_object())
            && let Some(var_ref_obj) = ref_obj.get("VarRef").and_then(|v| v.as_object())
            && let Some(var_obj) = var_ref_obj.get("Var").and_then(|v| v.as_object())
        {
            if let Some(sym_obj) = var_obj.get("sym").and_then(|v| v.as_object())
                && let Some(name) = sym_obj.get("String").and_then(|v| v.as_str())
            {
                return name.to_string();
            }
            if let Some(sym_str) = var_obj.get("sym").and_then(|v| v.as_str()) {
                return sym_str.to_string();
            }
        }

        // Handle direct sym structure
        if let Some(sym_obj) = obj.get("sym").and_then(|v| v.as_object())
            && let Some(name) = sym_obj.get("String").and_then(|v| v.as_str())
        {
            return name.to_string();
        }
        if let Some(sym_str) = obj.get("sym").and_then(|v| v.as_str()) {
            return sym_str.to_string();
        }
    }

    // Handle simple string case
    if let Some(name) = arg.as_str() {
        return name.to_string();
    }

    // Fallback
    "arg".to_string()
}

// Helper function to authenticate user with email and password
pub async fn authenticate_user(
    db: &DatabasePool,
    email: &str,
    password: &str,
) -> Result<User, String> {
    tracing::debug!("Starting authentication for email: {}", email);

    // Get user authentication record
    let user_auth = match UserAuth::get_user_auth(db, "email_password", email).await {
        Ok(auth) => {
            tracing::debug!("Found user_auth record for {}", email);
            tracing::debug!("user_auth.user_id: {}", auth.user_id);
            tracing::debug!("user_auth.auth_data is_some: {}", auth.auth_data.is_some());
            auth
        }
        Err(e) => {
            tracing::error!("Failed to get user_auth for {}: {:?}", email, e);
            // Log more specific error details
            match &e {
                UserError::Database(db_err) => {
                    tracing::error!("Database error details: {}", db_err);
                }
                UserError::NotFound => {
                    tracing::warn!("User auth record not found for email: {}", email);
                }
            }
            return Err("Invalid email or password".to_string());
        }
    };

    // Get auth_data and verify password
    let auth_data = user_auth.auth_data.ok_or_else(|| {
        tracing::error!("No auth_data found for user: {}", email);
        "Invalid email or password".to_string()
    })?;

    // Convert JsonValue to string for password verification
    let auth_data_str = serde_json::to_string(&auth_data).map_err(|e| {
        tracing::error!("Failed to serialize auth_data for {}: {}", email, e);
        "Authentication error".to_string()
    })?;

    // Verify password
    let password_valid = hot::auth::verify_password(password, &auth_data_str).map_err(|e| {
        tracing::error!("Password verification error for {}: {}", email, e);
        "Authentication error".to_string()
    })?;

    if !password_valid {
        tracing::warn!("Invalid password for email: {}", email);
        return Err("Invalid email or password".to_string());
    }

    tracing::debug!("Password verified for email: {}", email);

    // Get the user record
    let user = match User::get_user(db, &user_auth.user_id).await {
        Ok(user) => {
            tracing::debug!("Found user record for {}: {}", email, user.user_id);
            user
        }
        Err(e) => {
            tracing::error!("Failed to get user record for {}: {:?}", email, e);
            return Err("Authentication error".to_string());
        }
    };

    tracing::info!("Authentication successful for email: {}", email);
    Ok(user)
}

/// Create an organization with the given type ("individual" or "organization").
///
/// This is **idempotent for owned slugs** and **self-healing across the three
/// sequential inserts** (org → org_user → env), both of which matter a lot in
/// practice because we don't have transactions plumbed through the DB layer:
///
///   1. If an org with `slug` already exists AND is owned by `user_id`, we
///      adopt it — ensure the org_user membership row exists and the default
///      env exists, then return its id as if we'd just created it. This turns
///      a retry of `verify_email_handler` or `claim_handle_post_handler` (e.g.
///      after a transient network failure mid-flow, or a mail-scanner
///      pre-fetch) into a no-op instead of a `duplicate key` error.
///
///   2. If `insert_org` succeeds but a subsequent step fails, we delete the
///      org we just created before returning the error. Without this,
///      transient failures leave behind orphan `org` rows that block the user
///      from ever claiming their own slug again.
///
/// If the slug is owned by a *different* user we surface `"slug already taken"`
/// so callers can render a "try an alternative" banner.
pub async fn create_org(
    db: &DatabasePool,
    user_id: &Uuid,
    name: &str,
    slug: &str,
    org_type: &str,
) -> Result<Uuid, String> {
    // Owned-slug recovery. Runs on every call so partial-failure retries
    // are idempotent.
    if let Ok(existing) = hot::db::org::Org::get_org_by_slug(db, slug).await {
        if existing.created_by_user_id == *user_id {
            ensure_org_membership_and_env(db, &existing.org_id, user_id).await;
            return Ok(existing.org_id);
        }
        // Owned by someone else — surface as a distinct error so callers can
        // render "try `slug-2`" instead of the generic failure message.
        return Err(format!(
            "Failed to create organization: slug {} already taken",
            slug
        ));
    }

    let org_id = uuid::Uuid::now_v7();

    hot::db::org::Org::insert_org(db, &org_id, name, slug, org_type, user_id)
        .await
        .map_err(|e| format!("Failed to create organization: {}", e))?;

    // Compensating-action rollback: if any post-insert step fails, delete
    // the `org` row we just created so we don't leave a user-invisible
    // orphan. This is the stand-in for a real transaction.
    let org_user_id = uuid::Uuid::now_v7();
    if let Err(e) =
        hot::db::org::OrgUser::insert_org_user(db, &org_user_id, &org_id, user_id, Some(2), user_id)
            .await
    {
        tracing::error!(
            "insert_org_user failed for org {} / user {}: {:?} — rolling back the org row",
            org_id,
            user_id,
            e
        );
        if let Err(del_err) = hot::db::org::Org::delete_by_id(db, &org_id).await {
            tracing::error!("Failed to roll back orphan org {}: {:?}", org_id, del_err);
        }
        return Err(format!("Failed to add user to organization: {}", e));
    }

    // Default env is best-effort. If it ever proves a must-have we should
    // include it in the rollback too.
    let env_id = uuid::Uuid::now_v7();
    let _ = hot::db::Env::insert_env(db, &env_id, &org_id, "development", user_id).await;

    Ok(org_id)
}

/// "Fix-forward" org recovery for an authenticated user with no current org.
///
/// Anchors the rule that **a user who completed signup must never be asked to
/// pick a handle they already chose**. This is the safety net for when the
/// normal verify→create_org→cookie path didn't leave them with a usable
/// session for any reason (mail-scanner pre-fetch, dropped cookies, partial
/// failure, transient DB error, you name it).
///
/// Resolution order:
///   1. If the user already has any org → return its slug (nothing to do).
///   2. If their most recent `email_verification` row carries an `org_slug` →
///      try to claim it via `create_org` (idempotent for owned slugs). On
///      success, return the slug.
///   3. If that slug is owned by a *different* user (lost a race), pick a
///      `suggest_alternative` slug and create that.
///   4. If there's no verification record with a slug at all → return None
///      so the caller can fall back to rendering the claim-handle form
///      (legitimate case for new OAuth users).
///
/// This intentionally returns the **slug** (not the org id) because every
/// caller wants to redirect to a `/@{slug}/...` URL anyway. The returned
/// slug is guaranteed to belong to the user.
pub async fn recover_or_create_org_for_user(
    db: &DatabasePool,
    user_id: &Uuid,
    user_email: &str,
    user_name: &str,
) -> Option<String> {
    if let Ok(orgs) = hot::db::org::Org::get_orgs_by_user(db, user_id).await
        && let Some(org) = orgs.first()
    {
        return Some(org.slug.clone());
    }

    let verification = match hot::db::EmailVerification::get_latest_by_email(db, user_email).await {
        Ok(Some(v)) => v,
        _ => {
            tracing::info!(
                "recover_or_create_org_for_user: no email_verification record for {} — \
                 user must claim a handle (typical OAuth signup)",
                user_email
            );
            return None;
        }
    };

    let Some(requested_slug) = verification.org_slug.as_deref() else {
        tracing::info!(
            "recover_or_create_org_for_user: latest email_verification for {} has no org_slug \
             — user must claim a handle",
            user_email
        );
        return None;
    };

    let account_type = verification.account_type.as_deref().unwrap_or("individual");
    let org_name = if account_type == "organization" {
        verification.org_name.as_deref().unwrap_or(user_name)
    } else {
        user_name
    };

    match create_org(db, user_id, org_name, requested_slug, account_type).await {
        Ok(_) => {
            tracing::info!(
                "recover_or_create_org_for_user: recovered org {} for user {} from \
                 pending verification",
                requested_slug,
                user_email
            );
            Some(requested_slug.to_string())
        }
        Err(e) => {
            tracing::warn!(
                "recover_or_create_org_for_user: requested slug {} unavailable for user {}: \
                 {} — trying an alternative",
                requested_slug,
                user_email,
                e
            );
            let alternative = crate::slug::suggest_alternative(db, requested_slug).await;
            match create_org(db, user_id, org_name, &alternative, account_type).await {
                Ok(_) => {
                    tracing::info!(
                        "recover_or_create_org_for_user: created alternative org {} for user {}",
                        alternative,
                        user_email
                    );
                    Some(alternative)
                }
                Err(e2) => {
                    tracing::error!(
                        "recover_or_create_org_for_user: alternative slug {} also failed for \
                         user {}: {} — falling back to claim-handle form",
                        alternative,
                        user_email,
                        e2
                    );
                    None
                }
            }
        }
    }
}

/// Ensure the given `user_id` is an admin member of `org_id`, and that the
/// org has at least one env. Both inserts are silently best-effort — a
/// unique-constraint failure here means the rows already exist, which is
/// exactly the state we want.
pub async fn ensure_org_membership_and_env(db: &DatabasePool, org_id: &Uuid, user_id: &Uuid) {
    if hot::db::org::OrgUser::get_org_user(db, org_id, user_id)
        .await
        .is_err()
    {
        let org_user_id = uuid::Uuid::now_v7();
        if let Err(e) = hot::db::org::OrgUser::insert_org_user(
            db,
            &org_user_id,
            org_id,
            user_id,
            Some(2),
            user_id,
        )
        .await
        {
            tracing::warn!(
                "ensure_org_membership_and_env: failed to add user {} to org {}: {:?}",
                user_id,
                org_id,
                e
            );
        }
    }

    if let Ok(envs) = hot::db::Env::get_envs_by_org(db, org_id).await
        && envs.is_empty()
    {
        let env_id = uuid::Uuid::now_v7();
        let _ = hot::db::Env::insert_env(db, &env_id, org_id, "development", user_id).await;
    }
}

// Helper function to process invite code after successful authentication
pub async fn process_invite_code(
    db: &DatabasePool,
    user_id: &Uuid,
    invite_code: &str,
) -> Result<(), String> {
    // Get invite by code
    let invite = hot::db::invite::Invite::get_invite_by_code(db, invite_code)
        .await
        .map_err(|_| "Invalid invite code")?;

    // Check if invite is valid
    invite
        .is_valid()
        .map_err(|e| format!("Invalid invite: {}", e))?;

    // Check if user is already a member of the organization
    if hot::db::org::OrgUser::get_org_user(db, &invite.org_id, user_id)
        .await
        .is_ok()
    {
        return Err("User is already a member of this organization".to_string());
    }

    // Enforce team_members plan limit
    let features = hot::db::Features::resolve_for_org(db, &invite.org_id).await;
    let max_members = features.team_members();
    if max_members >= 0 {
        let current_count = hot::db::org::OrgUser::count_active_members(db, &invite.org_id)
            .await
            .unwrap_or(0);
        if current_count >= max_members as i64 {
            return Err(format!(
                "This organization has reached its plan limit of {} team members. An admin must upgrade the plan.",
                max_members
            ));
        }
    }

    // Add user to organization
    let org_user_id = uuid::Uuid::now_v7();
    hot::db::org::OrgUser::insert_org_user(
        db,
        &org_user_id,
        &invite.org_id,
        user_id,
        Some(invite.intended_org_user_role_id),
        &invite.created_by_user_id,
    )
    .await
    .map_err(|e| format!("Failed to add user to organization: {}", e))?;

    // Update invite status to joined
    hot::db::invite::Invite::update_invite_status(
        db,
        &invite.invite_id,
        &hot::db::invite::InviteStatus::Joined,
        Some(&invite.created_by_user_id),
    )
    .await
    .map_err(|e| format!("Failed to update invite status: {}", e))?;

    Ok(())
}

// Helper function to check if user is admin of an organization
pub async fn is_user_org_admin(
    db: &DatabasePool,
    user_id: &uuid::Uuid,
    org_id: &uuid::Uuid,
) -> bool {
    match hot::db::org::OrgUser::get_org_user(db, org_id, user_id).await {
        Ok(org_user) => org_user.org_user_role_id == 2, // 2 = admin role
        Err(e) => {
            tracing::error!(
                "Failed to check org admin status for user {} in org {}: {}",
                user_id,
                org_id,
                e
            );
            false
        }
    }
}

/// Helper function to set default organization and environment cookies
/// Sets cookies for the earliest-created organization and environment that the user has access to
pub async fn set_default_org_env_cookies(
    db: &DatabasePool,
    user_id: &Uuid,
    cookies: CookieJar,
) -> Result<CookieJar, String> {
    // Get user's organizations (already ordered by created_at ascending - earliest first)
    let user_orgs = hot::db::org::Org::get_orgs_by_user(db, user_id)
        .await
        .map_err(|e| format!("Failed to get user organizations: {}", e))?;

    if user_orgs.is_empty() {
        return Ok(cookies); // No organizations available
    }

    // Get the earliest-created organization
    let earliest_org = &user_orgs[0];

    // Get environments for this organization (already ordered by created_at ascending - earliest first)
    let org_envs = hot::db::env::Env::get_envs_by_org(db, &earliest_org.org_id)
        .await
        .map_err(|e| format!("Failed to get organization environments: {}", e))?;

    // Set organization cookie
    let mut org_cookie = axum_extra::extract::cookie::Cookie::new(
        crate::auth::CURRENT_ORG_COOKIE_NAME,
        earliest_org.org_id.to_string(),
    );
    org_cookie.set_path("/");
    org_cookie.set_max_age(time::Duration::days(30));
    org_cookie.set_http_only(true);
    org_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
    org_cookie.set_secure(!hot::env::is_local_dev());

    let mut updated_cookies = cookies.add(org_cookie);

    // Set environment cookie if available
    if let Some(earliest_env) = org_envs.first() {
        let mut env_cookie = axum_extra::extract::cookie::Cookie::new(
            crate::auth::CURRENT_ENV_COOKIE_NAME,
            earliest_env.env_id.to_string(),
        );
        env_cookie.set_path("/");
        env_cookie.set_max_age(time::Duration::days(30));
        env_cookie.set_http_only(true);
        env_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
        env_cookie.set_secure(!hot::env::is_local_dev());

        updated_cookies = updated_cookies.add(env_cookie);
    }

    Ok(updated_cookies)
}

/// Cookie name for cross-subdomain presence indicator
/// This cookie is set on the shared product domain to indicate the user is signed in
pub const PRESENCE_COOKIE_NAME: &str = "hot_signed_in";

/// Add the cross-subdomain presence cookie to indicate the user is signed in
/// This cookie is readable by both app.hot.dev and hot.dev
pub fn add_presence_cookie(cookies: CookieJar) -> CookieJar {
    let mut cookie = axum_extra::extract::cookie::Cookie::new(PRESENCE_COOKIE_NAME, "1");
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::days(30));
    // Not HttpOnly - needs to be readable by JavaScript on hot.dev
    cookie.set_http_only(false);
    cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
    cookie.set_secure(!hot::env::is_local_dev());

    // Set domain for cross-subdomain sharing (e.g., .hot.dev)
    if let Some(domain) = hot::env::get_cookie_domain() {
        cookie.set_domain(domain);
    }

    cookies.add(cookie)
}

/// Remove the cross-subdomain presence cookie on sign out
pub fn remove_presence_cookie(cookies: CookieJar) -> CookieJar {
    let mut cookie = axum_extra::extract::cookie::Cookie::new(PRESENCE_COOKIE_NAME, "");
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::seconds(0));
    cookie.set_http_only(false);
    cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
    cookie.set_secure(!hot::env::is_local_dev());

    // Must use same domain to clear the cookie
    if let Some(domain) = hot::env::get_cookie_domain() {
        cookie.set_domain(domain);
    }

    cookies.add(cookie)
}

// Switch handlers for organization and environment navigation
pub async fn switch_org_handler(
    Path(org_id): Path<Uuid>,
    State(_db): State<Arc<DatabasePool>>,
    headers: HeaderMap,
    cookies: CookieJar,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<(CookieJar, Redirect), Html<String>> {
    // Check if the requested org ID is in the user's organizations
    if !session.user_orgs.iter().any(|org| org.org_id == org_id) {
        // User doesn't have access to this org, redirect to dashboard
        return Ok((cookies, Redirect::to("/")));
    }

    // Set the current organization cookie
    let mut cookie = axum_extra::extract::cookie::Cookie::new(
        crate::auth::CURRENT_ORG_COOKIE_NAME,
        org_id.to_string(),
    );
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::days(30));
    cookie.set_http_only(true);
    cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
    cookie.set_secure(!hot::env::is_local_dev());

    let updated_cookies = cookies.add(cookie);

    // Redirect back to the previous page or dashboard
    let redirect_url = get_redirect_url_from_referrer(&headers);
    Ok((updated_cookies, Redirect::to(&redirect_url)))
}

pub async fn switch_env_handler(
    Path(env_id): Path<Uuid>,
    Query(params): Query<AHashMap<String, String>>,
    State(_db): State<Arc<DatabasePool>>,
    headers: HeaderMap,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    cookies: CookieJar,
) -> Result<(CookieJar, Redirect), Html<String>> {
    // Check if user has access to this environment
    if !session.has_env_access(&env_id) {
        return Err(Html(
            "You don't have access to this environment".to_string(),
        ));
    }

    // Set the environment cookie
    let new_cookies = cookies.add(
        axum_extra::extract::cookie::Cookie::build((
            crate::auth::CURRENT_ENV_COOKIE_NAME,
            env_id.to_string(),
        ))
        .path("/")
        .max_age(time::Duration::days(30))
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .http_only(true)
        .secure(!hot::env::is_local_dev())
        .build(),
    );

    // Use explicit redirect param if provided (must start with /), otherwise use referrer
    let redirect_url = params
        .get("redirect")
        .filter(|r| is_safe_relative_redirect(r))
        .cloned()
        .unwrap_or_else(|| get_redirect_url_from_referrer(&headers));
    Ok((new_cookies, Redirect::to(&redirect_url)))
}

// Status handler for health checks
pub async fn status_handler() -> Json<JsonValue> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "hot.dev app server",
        "version": crate::build_info::VERSION,
        "git_sha": crate::build_info::git_sha_short(),
        "start_time": crate::build_info::start_time_iso()
    }))
}

/// Build grouped JSON for MCP tools and webhooks in an environment.
/// Returns `(mcp_tools_json, webhooks_json)` — each a JSON array of
/// `{"service":"...","items":["..."]}` objects, grouped by service.
pub async fn build_permission_path_options(
    db: &hot::db::DatabasePool,
    env_id: &uuid::Uuid,
) -> (String, String) {
    use std::collections::BTreeMap;

    // MCP tools grouped by service
    let mcp_json = match hot::db::mcp_tool::McpTool::get_mcp_tools_by_env_deployed(
        db,
        env_id,
        Some(500),
        Some(0),
    )
    .await
    {
        Ok(tools) => {
            let mut by_service: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for t in &tools {
                by_service
                    .entry(t.service.clone())
                    .or_default()
                    .push(t.name.clone());
            }
            let groups: Vec<JsonValue> = by_service
                .into_iter()
                .map(|(service, mut items)| {
                    items.sort();
                    items.dedup();
                    serde_json::json!({"service": service, "items": items})
                })
                .collect();
            serde_json::to_string(&groups).unwrap_or_else(|_| "[]".into())
        }
        Err(_) => "[]".into(),
    };

    // Webhooks grouped by service
    let webhooks_json = match hot::db::webhook::Webhook::get_by_env(db, env_id).await {
        Ok(webhooks) => {
            let mut by_service: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for w in &webhooks {
                let path = w.path.strip_prefix('/').unwrap_or(&w.path);
                by_service
                    .entry(w.service.clone())
                    .or_default()
                    .push(path.to_string());
            }
            let groups: Vec<JsonValue> = by_service
                .into_iter()
                .map(|(service, mut items)| {
                    items.sort();
                    items.dedup();
                    serde_json::json!({"service": service, "items": items})
                })
                .collect();
            serde_json::to_string(&groups).unwrap_or_else(|_| "[]".into())
        }
        Err(_) => "[]".into(),
    };

    (mcp_json, webhooks_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_referrer_redirect_accepts_same_host_path() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("app.hot.dev"));
        headers.insert(
            "referer",
            HeaderValue::from_static("https://app.hot.dev/runs?status=failed"),
        );

        assert_eq!(
            get_redirect_url_from_referrer(&headers),
            "/runs?status=failed"
        );
    }

    #[test]
    fn test_referrer_redirect_rejects_external_host() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("app.hot.dev"));
        headers.insert(
            "referer",
            HeaderValue::from_static("https://evil.example/runs"),
        );

        assert_eq!(get_redirect_url_from_referrer(&headers), "/");
    }
}

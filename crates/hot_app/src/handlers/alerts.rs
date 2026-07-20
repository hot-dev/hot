//! Alert settings handlers - manage alert destinations and subscriptions

use crate::auth::Session;
use crate::email::{AppEmailEnqueuer, AppEmailSender};
use crate::templates;
use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use chrono::Utc;
use hot::db::DatabasePool;
use hot::db::alert::{
    Alert, AlertChannel, AlertDelivery, AlertDestination, AlertSubscription, DeliveryStatus,
    DestinationType, EmailDestinationConfig, EmailTarget,
};
use hot::val::Val;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

// =============================================================================
// Form Data Structures
// =============================================================================

#[derive(Deserialize, Debug)]
pub struct DestinationForm {
    pub name: String,
    pub destination_type: String,
    #[serde(default)]
    pub enabled: Option<String>,
    // Email fields
    #[serde(default)]
    pub email_target: Option<String>, // "address", "org", "team", "user"
    #[serde(default)]
    pub email_address: Option<String>,
    #[serde(default)]
    pub email_team_id: Option<String>,
    #[serde(default)]
    pub email_user_id: Option<String>,
    // Slack fields
    #[serde(default)]
    pub slack_webhook_url: Option<String>,
    #[serde(default)]
    pub slack_channel: Option<String>,
    // PagerDuty fields
    #[serde(default)]
    pub pagerduty_routing_key: Option<String>,
    #[serde(default)]
    pub pagerduty_severity: Option<String>,
    // Webhook fields
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_headers: Option<String>,
}

impl DestinationForm {
    /// Validate an email address format
    fn is_valid_email(email: &str) -> bool {
        // Simple email validation: must have @ with text before and after, and a dot after @
        let parts: Vec<&str> = email.split('@').collect();
        if parts.len() != 2 {
            return false;
        }
        let local = parts[0];
        let domain = parts[1];

        // Local part must be non-empty
        if local.is_empty() {
            return false;
        }

        // Domain must have at least one dot and non-empty parts
        let domain_parts: Vec<&str> = domain.split('.').collect();
        if domain_parts.len() < 2 {
            return false;
        }

        // All domain parts must be non-empty
        domain_parts.iter().all(|p| !p.is_empty())
    }

    /// Validate a Slack webhook URL specifically
    fn is_valid_slack_webhook(url_str: &str) -> Result<(), String> {
        let url = url::Url::parse(url_str).map_err(|_| "Invalid URL format")?;

        // Must be HTTPS
        if url.scheme() != "https" {
            return Err("Slack webhook URL must use HTTPS".to_string());
        }

        // Should be a Slack webhook URL
        let host = url.host_str().unwrap_or("");
        if !host.contains("slack.com") && !host.contains("slack-gov.com") {
            return Err(
                "URL doesn't appear to be a Slack webhook (expected hooks.slack.com)".to_string(),
            );
        }

        Ok(())
    }

    /// Validate a webhook URL
    fn validate_webhook_url(url_str: &str) -> Result<(), String> {
        let url = url::Url::parse(url_str).map_err(|_| "Invalid URL format")?;

        // Must be HTTP or HTTPS
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err("Webhook URL must use HTTP or HTTPS".to_string());
        }

        Ok(())
    }

    /// Validate Slack channel format
    fn is_valid_slack_channel(channel: &str) -> bool {
        // Slack channels start with # or are channel IDs (alphanumeric)
        channel.starts_with('#')
            || channel
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }

    /// Validate PagerDuty severity
    fn is_valid_pagerduty_severity(severity: &str) -> bool {
        matches!(severity, "critical" | "error" | "warning" | "info")
    }

    /// Build JSON config from form fields based on destination type
    pub fn build_config(&self) -> Result<serde_json::Value, String> {
        match self.destination_type.as_str() {
            "email" => {
                let target = self.email_target.as_deref().unwrap_or("address");

                match target {
                    "address" => {
                        let address = self
                            .email_address
                            .as_ref()
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .ok_or("Email address is required")?;

                        if !Self::is_valid_email(address) {
                            return Err("Invalid email address format".to_string());
                        }

                        Ok(serde_json::json!({ "target": "address", "address": address }))
                    }
                    "org" => Ok(serde_json::json!({ "target": "org" })),
                    "team" => {
                        let team_id = self
                            .email_team_id
                            .as_ref()
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .ok_or("Team is required for team email destination")?;

                        // Validate UUID format
                        uuid::Uuid::parse_str(team_id).map_err(|_| "Invalid team ID format")?;

                        Ok(serde_json::json!({ "target": "team", "team_id": team_id }))
                    }
                    "user" => {
                        let user_id = self
                            .email_user_id
                            .as_ref()
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .ok_or("User is required for user email destination")?;

                        // Validate UUID format
                        uuid::Uuid::parse_str(user_id).map_err(|_| "Invalid user ID format")?;

                        Ok(serde_json::json!({ "target": "user", "user_id": user_id }))
                    }
                    _ => Err(format!("Unknown email target type: {}", target)),
                }
            }
            "slack" => {
                let webhook_url = self
                    .slack_webhook_url
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or("Slack webhook URL is required")?;

                Self::is_valid_slack_webhook(webhook_url)?;

                let mut config = serde_json::json!({ "webhook_url": webhook_url });
                if let Some(channel) = self
                    .slack_channel
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    if !Self::is_valid_slack_channel(channel) {
                        return Err(
                            "Invalid Slack channel format (should start with # or be a channel ID)"
                                .to_string(),
                        );
                    }
                    config["channel"] = serde_json::json!(channel);
                }
                Ok(config)
            }
            "pagerduty" => {
                let routing_key = self
                    .pagerduty_routing_key
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or("PagerDuty routing key is required")?;

                // Routing keys are typically 32 hex characters
                if routing_key.len() < 10 {
                    return Err("PagerDuty routing key seems too short".to_string());
                }

                let severity = self
                    .pagerduty_severity
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("error");

                if !Self::is_valid_pagerduty_severity(severity) {
                    return Err(
                        "Invalid PagerDuty severity (must be critical, error, warning, or info)"
                            .to_string(),
                    );
                }

                Ok(serde_json::json!({
                    "routing_key": routing_key,
                    "severity": severity
                }))
            }
            "webhook" => {
                let url = self
                    .webhook_url
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or("Webhook URL is required")?;

                Self::validate_webhook_url(url)?;

                let mut config = serde_json::json!({ "url": url });
                if let Some(headers_str) = self
                    .webhook_headers
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    match serde_json::from_str::<serde_json::Value>(headers_str) {
                        Ok(headers) => {
                            if !headers.is_object() {
                                return Err("Headers must be a JSON object".to_string());
                            }
                            config["headers"] = headers;
                        }
                        Err(e) => return Err(format!("Invalid JSON in headers: {}", e)),
                    }
                }
                Ok(config)
            }
            _ => Err(format!(
                "Unknown destination type: {}",
                self.destination_type
            )),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct SubscriptionForm {
    pub name: Option<String>,
    /// Comma-separated list of channel IDs
    pub channel_ids: String,
    /// Comma-separated list of destination IDs
    pub destination_ids: String,
    #[serde(default)]
    pub env_specific: Option<String>,
    #[serde(default)]
    pub enabled: Option<String>,
}

// =============================================================================
// Destinations Handlers
// =============================================================================

/// List all alert destinations for the current org
pub async fn destinations_list_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Alert Destinations".to_string(),
    ));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let destinations = AlertDestination::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    // Build human-readable detail strings for each destination
    let mut destination_details = std::collections::HashMap::new();
    // Pre-fetch teams and users for resolving names
    let teams = fetch_teams_for_dropdown(&db, &org_id).await;
    let users = fetch_users_for_dropdown(&db, &org_id).await;

    for dest in &destinations {
        let detail = match dest.destination_type_id {
            1 => {
                // Email - show target info
                if let Ok(config) =
                    hot::db::alert::EmailDestinationConfig::from_config(&dest.config)
                {
                    match &config.target {
                        hot::db::alert::EmailTarget::Address { address } => address.clone(),
                        hot::db::alert::EmailTarget::Org => "Everyone in Org".to_string(),
                        hot::db::alert::EmailTarget::Team { team_id } => {
                            let team_name = teams
                                .iter()
                                .find(|t| t.id == team_id.to_string())
                                .map(|t| t.name.as_str())
                                .unwrap_or("Unknown Team");
                            format!("Team: {}", team_name)
                        }
                        hot::db::alert::EmailTarget::User { user_id } => {
                            let user_name = users
                                .iter()
                                .find(|u| u.id == user_id.to_string())
                                .map(|u| u.name.as_str())
                                .unwrap_or("Unknown User");
                            format!("User: {}", user_name)
                        }
                    }
                } else {
                    String::new()
                }
            }
            2 => {
                // Slack
                dest.config
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            }
            _ => String::new(),
        };
        if !detail.is_empty() {
            destination_details.insert(dest.alert_destination_id, detail);
        }
    }

    // Map query param info/error to flash messages
    let (flash_type, flash_message) = if let Some(info) = params.get("info") {
        let msg = match info.as_str() {
            "verification_sent" => "Destination created. A verification email has been sent.",
            "verification_sent_org_user" => {
                "Destination created. A verification email has been sent. Note: this email belongs to an existing org member \u{2014} consider using \"Specific User\" instead."
            }
            "verification_resent" => "Verification email has been resent.",
            "already_verified" => "This destination is already verified.",
            _ => "",
        };
        ("info", msg)
    } else if let Some(error) = params.get("error") {
        let msg = match error.as_str() {
            "create_failed" => "Failed to create destination.",
            "unauthorized" => "You do not have permission to perform this action.",
            "resend_limit_reached" => {
                "Maximum verification resend attempts reached. Please delete and recreate the destination."
            }
            "resend_failed" => "Failed to resend verification email.",
            "not_found" => "Destination not found.",
            _ => "",
        };
        ("error", msg)
    } else {
        ("", "")
    };

    let template = templates::AlertDestinationsList {
        title: "Alert Destinations",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        destinations,
        destination_details,
        is_admin: session.is_current_org_admin,
        flash_type,
        flash_message,
    };

    Html(template.render().unwrap()).into_response()
}

/// Show form to create a new destination
pub async fn destinations_new_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert Destinations".to_string(),
        Some("/settings/alerts/destinations".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    // Fetch teams and users for email target dropdowns
    let teams = fetch_teams_for_dropdown(&db, &org_id).await;
    let users = fetch_users_for_dropdown(&db, &org_id).await;

    // Map query param errors to user-friendly messages
    let error_message = match params.get("error").map(|s| s.as_str()) {
        Some("invalid_type") => "Invalid destination type.",
        Some("invalid_config") => "Invalid destination configuration.",
        _ => "",
    };

    let template = templates::AlertDestinationNew {
        title: "New Alert Destination",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        teams,
        users,
        error_message,
    };

    Html(template.render().unwrap()).into_response()
}

/// Fetch teams for dropdown in destination forms
async fn fetch_teams_for_dropdown(
    db: &DatabasePool,
    org_id: &uuid::Uuid,
) -> Vec<templates::NamedItem> {
    hot::db::team::Team::get_teams_by_org(db, org_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| templates::NamedItem {
            id: t.team_id.to_string(),
            name: t.name,
        })
        .collect()
}

/// Fetch users for dropdown in destination forms
async fn fetch_users_for_dropdown(
    db: &DatabasePool,
    org_id: &uuid::Uuid,
) -> Vec<templates::NamedItem> {
    hot::db::org::OrgUser::get_users_with_roles_by_org(db, org_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u.active)
        .map(|u| templates::NamedItem {
            id: u.user_id.to_string(),
            name: if u.name.is_empty() {
                u.email.clone()
            } else {
                format!("{} ({})", u.name, u.email)
            },
        })
        .collect()
}

async fn destination_target_belongs_to_org(
    db: &DatabasePool,
    org_id: &Uuid,
    config: &serde_json::Value,
) -> bool {
    match EmailDestinationConfig::from_config(config)
        .ok()
        .map(|config| config.target)
    {
        Some(EmailTarget::Team { team_id }) => hot::db::Team::get_team_by_org(db, &team_id, org_id)
            .await
            .is_ok(),
        Some(EmailTarget::User { user_id }) => hot::db::OrgUser::get_org_user(db, org_id, &user_id)
            .await
            .is_ok(),
        _ => true,
    }
}

fn parse_selected_ids(ids: &str) -> Option<Vec<Uuid>> {
    let parsed: Result<Vec<_>, _> = ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(Uuid::parse_str)
        .collect();
    parsed.ok().filter(|ids| !ids.is_empty())
}

async fn subscription_resources_belong_to_org(
    db: &DatabasePool,
    org_id: &Uuid,
    channel_ids: &[Uuid],
    destination_ids: &[Uuid],
) -> bool {
    for channel_id in channel_ids {
        if AlertChannel::get_by_id_for_org(db, channel_id, org_id)
            .await
            .is_err()
        {
            return false;
        }
    }
    for destination_id in destination_ids {
        if AlertDestination::get_by_id_for_org(db, destination_id, org_id)
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Create a new destination
pub async fn destinations_create_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<DestinationForm>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized").into_response();
    }

    // Feature gate: alerts require Starter+ plan
    if !session.current_org_features.has_alerts() {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/settings/alerts/destinations").into_response();
        }
    };

    // Parse destination type
    let dest_type = match DestinationType::parse(&form.destination_type) {
        Some(dt) => dt,
        None => {
            return Redirect::to("/settings/alerts/destinations/new?error=invalid_type")
                .into_response();
        }
    };

    // Build config from form fields
    let config = match form.build_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Invalid destination config: {}", e);
            return Redirect::to("/settings/alerts/destinations/new?error=invalid_config")
                .into_response();
        }
    };
    if !destination_target_belongs_to_org(&db, &org_id, &config).await {
        return Redirect::to("/settings/alerts/destinations/new?error=invalid_config")
            .into_response();
    }

    // Determine if this is a specific-address email destination that needs verification
    let needs_verification = dest_type == DestinationType::Email
        && matches!(
            EmailDestinationConfig::from_config(&config)
                .ok()
                .map(|c| c.target),
            Some(EmailTarget::Address { .. })
        );

    if needs_verification {
        // Extract the email address from config
        let email_address = config
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Check if this email belongs to an existing org user — include a hint (non-blocking)
        let is_org_user_email = hot::db::org::OrgUser::get_users_with_roles_by_org(&db, &org_id)
            .await
            .map(|users| {
                users
                    .iter()
                    .any(|u| u.email.eq_ignore_ascii_case(&email_address))
            })
            .unwrap_or(false);

        // Generate verification token and set 24-hour expiry
        let token = AlertDestination::generate_verification_token();
        let expires_at = Utc::now() + chrono::Duration::hours(24);

        // Create destination as unverified
        match AlertDestination::create_with_verification(
            &db,
            &org_id,
            &form.name,
            dest_type,
            &config,
            &session.user.user_id,
            false,
            Some(&token),
            Some(expires_at),
        )
        .await
        {
            Ok(_dest) => {
                // Send verification email
                let org_name = session
                    .current_org
                    .as_ref()
                    .map(|o| o.name.as_str())
                    .unwrap_or("your organization");
                let email_enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
                if let Err(e) = email_enqueuer
                    .send_destination_verification_email(
                        &email_address,
                        org_name,
                        &form.name,
                        &token,
                    )
                    .await
                {
                    tracing::error!(
                        "Failed to send destination verification email to {}: {}",
                        email_address,
                        e
                    );
                }
                let redirect_url = if is_org_user_email {
                    "/settings/alerts/destinations?info=verification_sent_org_user"
                } else {
                    "/settings/alerts/destinations?info=verification_sent"
                };
                Redirect::to(redirect_url).into_response()
            }
            Err(e) => {
                tracing::error!("Failed to create destination: {}", e);
                Redirect::to("/settings/alerts/destinations?error=create_failed").into_response()
            }
        }
    } else {
        // Non-address targets (org/team/user) and non-email types are auto-verified
        match AlertDestination::create(
            &db,
            &org_id,
            &form.name,
            dest_type,
            &config,
            &session.user.user_id,
        )
        .await
        {
            Ok(_) => Redirect::to("/settings/alerts/destinations").into_response(),
            Err(e) => {
                tracing::error!("Failed to create destination: {}", e);
                Redirect::to("/settings/alerts/destinations?error=create_failed").into_response()
            }
        }
    }
}

/// Show form to edit a destination
pub async fn destinations_edit_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(destination_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized").into_response();
    }

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert Destinations".to_string(),
        Some("/settings/alerts/destinations".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    let destination = match AlertDestination::get_by_id_for_org(&db, &destination_id, &org_id).await
    {
        Ok(d) => d,
        Err(_) => {
            return Redirect::to("/settings/alerts/destinations").into_response();
        }
    };

    // Parse config into form fields
    let config_fields = templates::DestinationConfigFields::from_config(
        destination.destination_type_id,
        &destination.config,
    );

    // Fetch teams and users for email target dropdowns
    let teams = fetch_teams_for_dropdown(&db, &org_id).await;
    let users = fetch_users_for_dropdown(&db, &org_id).await;

    let template = templates::AlertDestinationEdit {
        title: "Edit Alert Destination",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        destination,
        config_fields,
        teams,
        users,
    };

    Html(template.render().unwrap()).into_response()
}

/// Update a destination
pub async fn destinations_update_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(destination_id): Path<Uuid>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<DestinationForm>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    // Fetch existing destination and verify it belongs to current org
    let existing_dest =
        match AlertDestination::get_by_id_for_org(&db, &destination_id, &org_id).await {
            Ok(d) => d,
            _ => {
                return Redirect::to("/settings/alerts/destinations?error=unauthorized")
                    .into_response();
            }
        };

    // Build config from form fields
    let config = match form.build_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Invalid destination config: {}", e);
            return Redirect::to(&format!(
                "/settings/alerts/destinations/{}/edit?error=invalid_config",
                destination_id
            ))
            .into_response();
        }
    };
    if !destination_target_belongs_to_org(&db, &org_id, &config).await {
        return Redirect::to(&format!(
            "/settings/alerts/destinations/{}/edit?error=invalid_config",
            destination_id
        ))
        .into_response();
    }

    let enabled = form.enabled.is_some();

    // Detect if the email address changed on an email Address destination
    let old_email = EmailDestinationConfig::from_config(&existing_dest.config)
        .ok()
        .and_then(|c| match c.target {
            EmailTarget::Address { address } => Some(address),
            _ => None,
        });
    let new_email = EmailDestinationConfig::from_config(&config)
        .ok()
        .and_then(|c| match c.target {
            EmailTarget::Address { address } => Some(address),
            _ => None,
        });

    let email_address_changed = match (&old_email, &new_email) {
        (Some(old), Some(new)) => !old.eq_ignore_ascii_case(new),
        _ => false,
    };

    match AlertDestination::update(
        &db,
        &destination_id,
        &form.name,
        &config,
        enabled,
        &session.user.user_id,
    )
    .await
    {
        Ok(_) => {
            // If the email address changed, reset verification
            if email_address_changed && let Some(new_addr) = &new_email {
                let token = AlertDestination::generate_verification_token();
                let expires_at = Utc::now() + chrono::Duration::hours(24);

                // Reset verification state: mark unverified and set new token
                if let Err(e) =
                    AlertDestination::reset_verification(&db, &destination_id, &token, expires_at)
                        .await
                {
                    tracing::error!(
                        "Failed to reset verification for destination {}: {}",
                        destination_id,
                        e
                    );
                } else {
                    // Send verification email to the new address
                    let org_name = session
                        .current_org
                        .as_ref()
                        .map(|o| o.name.as_str())
                        .unwrap_or("your organization");
                    let email_enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
                    if let Err(e) = email_enqueuer
                        .send_destination_verification_email(new_addr, org_name, &form.name, &token)
                        .await
                    {
                        tracing::error!(
                            "Failed to send destination verification email to {}: {}",
                            new_addr,
                            e
                        );
                    }
                }

                return Redirect::to("/settings/alerts/destinations?info=verification_sent")
                    .into_response();
            }
            Redirect::to("/settings/alerts/destinations").into_response()
        }
        Err(e) => {
            tracing::error!("Failed to update destination: {}", e);
            Redirect::to(&format!(
                "/settings/alerts/destinations/{}/edit?error=update_failed",
                destination_id
            ))
            .into_response()
        }
    }
}

/// Delete a destination
pub async fn destinations_delete_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(destination_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    // Verify destination belongs to current org
    match AlertDestination::get_by_id_for_org(&db, &destination_id, &org_id).await {
        Ok(_) => {}
        _ => {
            return Redirect::to("/settings/alerts/destinations?error=unauthorized")
                .into_response();
        }
    }

    match AlertDestination::delete(&db, &destination_id).await {
        Ok(_) => Redirect::to("/settings/alerts/destinations").into_response(),
        Err(e) => {
            tracing::error!("Failed to delete destination: {}", e);
            Redirect::to("/settings/alerts/destinations?error=delete_failed").into_response()
        }
    }
}

// =============================================================================
// Subscriptions Handlers
// =============================================================================

/// List all alert subscriptions for the current org
pub async fn subscriptions_list_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Alert Subscriptions".to_string(),
    ));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let subscriptions = AlertSubscription::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    // Get channels and destinations for reference lookups
    let channels = AlertChannel::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    let destinations = AlertDestination::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    // Build connection summaries for each subscription
    let mut connections = Vec::new();
    for sub in &subscriptions {
        let channel_ids = AlertSubscription::get_channel_ids(&db, &sub.alert_subscription_id)
            .await
            .unwrap_or_default();
        let destination_ids =
            AlertSubscription::get_destination_ids(&db, &sub.alert_subscription_id)
                .await
                .unwrap_or_default();

        let channel_names: Vec<String> = channel_ids
            .iter()
            .filter_map(|cid| {
                channels
                    .iter()
                    .find(|c| c.alert_channel_id == *cid)
                    .map(|c| c.name.clone())
            })
            .collect();
        let destination_names: Vec<String> = destination_ids
            .iter()
            .filter_map(|did| {
                destinations
                    .iter()
                    .find(|d| d.alert_destination_id == *did)
                    .map(|d| d.name.clone())
            })
            .collect();

        connections.push(templates::SubscriptionConnections {
            channel_names,
            destination_names,
        });
    }

    let template = templates::AlertSubscriptionsList {
        title: "Alert Subscriptions",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        subscriptions,
        connections,
        channels,
        destinations,
        is_admin: session.is_current_org_admin,
    };

    Html(template.render().unwrap()).into_response()
}

/// Show form to create a new subscription
pub async fn subscriptions_new_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/subscriptions?error=unauthorized").into_response();
    }

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert Subscriptions".to_string(),
        Some("/settings/alerts/subscriptions".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let channels = AlertChannel::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    let destinations = AlertDestination::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    let template = templates::AlertSubscriptionNew {
        title: "New Alert Subscription",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        channels,
        destinations,
    };

    Html(template.render().unwrap()).into_response()
}

/// Create a new subscription
pub async fn subscriptions_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<SubscriptionForm>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/subscriptions?error=unauthorized").into_response();
    }

    // Feature gate: alerts require Starter+ plan
    if !session.current_org_features.has_alerts() {
        return Redirect::to("/settings/alerts/subscriptions?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    // Parse channel IDs
    let Some(channel_ids) = parse_selected_ids(&form.channel_ids) else {
        return Redirect::to("/settings/alerts/subscriptions/new?error=missing_selection")
            .into_response();
    };
    let Some(destination_ids) = parse_selected_ids(&form.destination_ids) else {
        return Redirect::to("/settings/alerts/subscriptions/new?error=missing_selection")
            .into_response();
    };
    if !subscription_resources_belong_to_org(&db, &org_id, &channel_ids, &destination_ids).await {
        return Redirect::to("/settings/alerts/subscriptions/new?error=invalid_selection")
            .into_response();
    }

    // Check env_specific to determine if env_id should be set
    let env_id = if form.env_specific.is_some() {
        session.current_env.as_ref().map(|e| e.env_id)
    } else {
        None
    };

    match AlertSubscription::create(
        &db,
        &org_id,
        env_id.as_ref(),
        &channel_ids,
        &destination_ids,
        &session.user.user_id,
    )
    .await
    {
        Ok(_) => Redirect::to("/settings/alerts/subscriptions").into_response(),
        Err(e) => {
            tracing::error!("Failed to create subscription: {}", e);
            Redirect::to("/settings/alerts/subscriptions/new?error=create_failed").into_response()
        }
    }
}

/// Show form to edit a subscription
pub async fn subscriptions_edit_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(alert_subscription_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert Subscriptions".to_string(),
        Some("/settings/alerts/subscriptions".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let subscription =
        match AlertSubscription::get_by_id_for_org(&db, &alert_subscription_id, &org_id).await {
            Ok(s) => s,
            Err(_) => {
                return Redirect::to("/settings/alerts/subscriptions?error=not_found")
                    .into_response();
            }
        };

    let channels = AlertChannel::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    let destinations = AlertDestination::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    let selected_channel_ids = AlertSubscription::get_channel_ids(&db, &alert_subscription_id)
        .await
        .unwrap_or_default();

    let selected_destination_ids =
        AlertSubscription::get_destination_ids(&db, &alert_subscription_id)
            .await
            .unwrap_or_default();

    let template = templates::AlertSubscriptionEdit {
        title: "Edit Alert Subscription",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        subscription,
        channels,
        destinations,
        selected_channel_ids,
        selected_destination_ids,
    };

    Html(template.render().unwrap()).into_response()
}

/// Update a subscription
pub async fn subscriptions_update_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(alert_subscription_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<SubscriptionForm>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/subscriptions?error=unauthorized").into_response();
    }

    // Verify subscription belongs to current org
    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };
    match AlertSubscription::get_by_id_for_org(&db, &alert_subscription_id, &org_id).await {
        Ok(_) => {}
        _ => {
            return Redirect::to("/settings/alerts/subscriptions?error=unauthorized")
                .into_response();
        }
    }

    // Parse channel IDs
    let Some(channel_ids) = parse_selected_ids(&form.channel_ids) else {
        return Redirect::to(&format!(
            "/settings/alerts/subscriptions/{}/edit?error=missing_selection",
            alert_subscription_id
        ))
        .into_response();
    };
    let Some(destination_ids) = parse_selected_ids(&form.destination_ids) else {
        return Redirect::to(&format!(
            "/settings/alerts/subscriptions/{}/edit?error=missing_selection",
            alert_subscription_id
        ))
        .into_response();
    };
    if !subscription_resources_belong_to_org(&db, &org_id, &channel_ids, &destination_ids).await {
        return Redirect::to(&format!(
            "/settings/alerts/subscriptions/{}/edit?error=invalid_selection",
            alert_subscription_id
        ))
        .into_response();
    }

    // Check env_specific to determine if env_id should be set
    let env_id = if form.env_specific.is_some() {
        session.current_env.as_ref().map(|e| e.env_id)
    } else {
        None
    };

    let enabled = form.enabled.is_some();

    match AlertSubscription::update(
        &db,
        &alert_subscription_id,
        env_id.as_ref(),
        &channel_ids,
        &destination_ids,
        enabled,
        &session.user.user_id,
    )
    .await
    {
        Ok(_) => Redirect::to("/settings/alerts/subscriptions").into_response(),
        Err(e) => {
            tracing::error!("Failed to update subscription: {}", e);
            Redirect::to(&format!(
                "/settings/alerts/subscriptions/{}/edit?error=update_failed",
                alert_subscription_id
            ))
            .into_response()
        }
    }
}

/// Delete a subscription
pub async fn subscriptions_delete_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(alert_subscription_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/subscriptions?error=unauthorized").into_response();
    }

    // Verify subscription belongs to current org
    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };
    match AlertSubscription::get_by_id_for_org(&db, &alert_subscription_id, &org_id).await {
        Ok(_) => {}
        _ => {
            return Redirect::to("/settings/alerts/subscriptions?error=unauthorized")
                .into_response();
        }
    }

    match AlertSubscription::delete(&db, &alert_subscription_id).await {
        Ok(_) => Redirect::to("/settings/alerts/subscriptions").into_response(),
        Err(e) => {
            tracing::error!("Failed to delete subscription: {}", e);
            Redirect::to("/settings/alerts/subscriptions?error=delete_failed").into_response()
        }
    }
}

// =============================================================================
// Channels Handlers (read-only for system channels, CRUD for custom)
// =============================================================================

/// List all alert channels (system + custom)
pub async fn channels_list_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Alert Channels".to_string(),
    ));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let channels = AlertChannel::get_by_org(&db, &org_id)
        .await
        .unwrap_or_default();

    let template = templates::AlertChannelsList {
        title: "Alert Channels",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        channels,
        is_admin: session.is_current_org_admin,
    };

    Html(template.render().unwrap()).into_response()
}

/// Form for creating/updating channels
#[derive(Debug, Deserialize)]
pub struct ChannelForm {
    pub name: String,
    pub pattern: String,
    pub env_specific: Option<String>,
    #[serde(default)]
    pub enabled: Option<String>,
}

/// Show form to create a new custom channel
pub async fn channels_new_handler(
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/channels").into_response();
    }

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert Channels".to_string(),
        Some("/settings/alerts/channels".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "New Channel".to_string(),
    ));

    let template = templates::AlertChannelNew {
        title: "New Alert Channel",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
    };

    Html(template.render().unwrap()).into_response()
}

/// Create a new custom channel
pub async fn channels_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ChannelForm>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/channels?error=unauthorized").into_response();
    }

    // Feature gate: alerts require Starter+ plan
    if !session.current_org_features.has_alerts() {
        return Redirect::to("/settings/alerts/channels?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let user_id = session.user.user_id;
    let env_id = if form.env_specific.is_some() {
        session.current_env.as_ref().map(|e| e.env_id)
    } else {
        None
    };

    // Validate pattern
    if let Err(e) = AlertChannel::validate_pattern(&form.pattern) {
        return Redirect::to(&format!(
            "/settings/alerts/channels/new?error=invalid_pattern&msg={}",
            urlencoding::encode(&e.to_string())
        ))
        .into_response();
    }

    match AlertChannel::create(
        &db,
        &org_id,
        env_id.as_ref(),
        form.name.trim(),
        form.pattern.trim(),
        &user_id,
    )
    .await
    {
        Ok(_) => Redirect::to("/settings/alerts/channels").into_response(),
        Err(e) => {
            tracing::error!("Failed to create channel: {}", e);
            Redirect::to("/settings/alerts/channels?error=create_failed").into_response()
        }
    }
}

/// Show form to edit a custom channel
pub async fn channels_edit_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(channel_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/channels").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    let channel = match AlertChannel::get_by_id_for_org(&db, &channel_id, &org_id).await {
        Ok(c) => c,
        Err(_) => {
            return Redirect::to("/settings/alerts/channels?error=not_found").into_response();
        }
    };

    // Can't edit system channels
    if channel.is_system() {
        return Redirect::to("/settings/alerts/channels?error=system_channel").into_response();
    }

    // System channels are available to the org but remain read-only.
    if channel.org_id.is_none() {
        return Redirect::to("/settings/alerts/channels?error=unauthorized").into_response();
    }

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert Channels".to_string(),
        Some("/settings/alerts/channels".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Edit Channel".to_string(),
    ));

    let template = templates::AlertChannelEdit {
        title: "Edit Alert Channel",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        channel,
    };

    Html(template.render().unwrap()).into_response()
}

/// Update a custom channel
pub async fn channels_update_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(channel_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ChannelForm>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/channels?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    let channel = match AlertChannel::get_by_id_for_org(&db, &channel_id, &org_id).await {
        Ok(c) => c,
        Err(_) => {
            return Redirect::to("/settings/alerts/channels?error=not_found").into_response();
        }
    };

    // Can't update system channels
    if channel.is_system() {
        return Redirect::to("/settings/alerts/channels?error=system_channel").into_response();
    }

    let user_id = session.user.user_id;
    let env_id = if form.env_specific.is_some() {
        session.current_env.as_ref().map(|e| e.env_id)
    } else {
        None
    };

    // Validate pattern
    if let Err(e) = AlertChannel::validate_pattern(&form.pattern) {
        return Redirect::to(&format!(
            "/settings/alerts/channels/{}/edit?error=invalid_pattern&msg={}",
            channel_id,
            urlencoding::encode(&e.to_string())
        ))
        .into_response();
    }

    let enabled = form.enabled.is_some();

    match AlertChannel::update(
        &db,
        &channel_id,
        form.name.trim(),
        form.pattern.trim(),
        env_id.as_ref(),
        enabled,
        &user_id,
    )
    .await
    {
        Ok(_) => Redirect::to("/settings/alerts/channels").into_response(),
        Err(e) => {
            tracing::error!("Failed to update channel: {}", e);
            Redirect::to("/settings/alerts/channels?error=update_failed").into_response()
        }
    }
}

/// Delete a custom channel
pub async fn channels_delete_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(channel_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Verify user is admin
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/channels?error=unauthorized").into_response();
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    let channel = match AlertChannel::get_by_id_for_org(&db, &channel_id, &org_id).await {
        Ok(c) => c,
        Err(_) => {
            return Redirect::to("/settings/alerts/channels?error=not_found").into_response();
        }
    };

    // Can't delete system channels
    if channel.is_system() {
        return Redirect::to("/settings/alerts/channels?error=system_channel").into_response();
    }

    match AlertChannel::delete(&db, &channel_id).await {
        Ok(_) => Redirect::to("/settings/alerts/channels").into_response(),
        Err(e) => {
            tracing::error!("Failed to delete channel: {}", e);
            Redirect::to("/settings/alerts/channels?error=delete_failed").into_response()
        }
    }
}

// =============================================================================
// Alert History Handlers
// =============================================================================

/// Query params for history pagination
#[derive(Debug, Deserialize)]
pub struct HistoryQueryParams {
    pub page: Option<i64>,
}

/// List alert history
pub async fn history_list_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<HistoryQueryParams>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Alert History".to_string(),
    ));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/").into_response();
        }
    };

    let page = params.page.unwrap_or(1).max(1);
    let per_page: i64 = 50;
    let offset = (page - 1) * per_page;

    let alerts = Alert::get_by_org_paginated(&db, &org_id, per_page, offset)
        .await
        .unwrap_or_default();

    let total_count = Alert::count_by_org(&db, &org_id).await.unwrap_or(0);
    let total_pages = (total_count + per_page - 1) / per_page;

    // Get delivery counts for each alert
    let mut alert_delivery_summaries = Vec::new();
    for alert in &alerts {
        let deliveries = AlertDelivery::get_by_alert_id(&db, &alert.alert_id)
            .await
            .unwrap_or_default();

        let total = deliveries.len();
        let sent = deliveries
            .iter()
            .filter(|d| d.status_id == DeliveryStatus::Sent.as_i16())
            .count();
        let failed = deliveries
            .iter()
            .filter(|d| d.status_id == DeliveryStatus::Failed.as_i16())
            .count();
        let pending = deliveries
            .iter()
            .filter(|d| {
                d.status_id == DeliveryStatus::Pending.as_i16()
                    || d.status_id == DeliveryStatus::Retrying.as_i16()
            })
            .count();

        alert_delivery_summaries.push(templates::AlertDeliverySummary {
            total,
            sent,
            failed,
            pending,
        });
    }

    let template = templates::AlertHistoryList {
        title: "Alert History",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        alerts,
        delivery_summaries: alert_delivery_summaries,
        current_page: page,
        total_pages,
        total_count,
    };

    Html(template.render().unwrap()).into_response()
}

/// View alert detail
pub async fn history_detail_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(alert_id): Path<Uuid>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::new(
        "Alert History".to_string(),
        Some("/settings/alerts/history".to_string()),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Alert Detail".to_string(),
    ));

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/").into_response(),
    };

    let alert = match Alert::get_by_id(&db, &alert_id).await {
        Ok(a) => a,
        Err(_) => {
            return Redirect::to("/settings/alerts/history?error=not_found").into_response();
        }
    };

    // Verify alert belongs to current org
    if alert.org_id != org_id {
        return Redirect::to("/settings/alerts/history?error=unauthorized").into_response();
    }

    // Get all deliveries for this alert
    let deliveries = AlertDelivery::get_by_alert_id(&db, &alert_id)
        .await
        .unwrap_or_default();

    // Get destination info and resolved user email for each delivery
    let mut delivery_details = Vec::new();
    for delivery in deliveries {
        let destination =
            AlertDestination::get_by_id_for_org(&db, &delivery.destination_id, &org_id)
                .await
                .ok();

        // If this delivery has a resolved_user_id, look up their email
        let resolved_user_email = if let Some(user_id) = &delivery.resolved_user_id {
            if hot::db::OrgUser::get_org_user(&db, &org_id, user_id)
                .await
                .is_ok()
            {
                hot::db::user::User::get_user(&db, user_id)
                    .await
                    .ok()
                    .map(|u| u.email)
            } else {
                None
            }
        } else {
            None
        };

        delivery_details.push(templates::AlertDeliveryDetail {
            delivery,
            destination,
            resolved_user_email,
        });
    }

    // Format data as Hot literal (default display) and keep JSON for toggle
    let data_hot = {
        let val: hot::val::Val =
            serde_json::from_value(alert.data.clone()).unwrap_or(hot::val::Val::Null);
        val.format(hot::val::ValFormat::Hot)
    };
    let data_json = serde_json::to_string_pretty(&alert.data).unwrap_or_else(|_| "{}".to_string());

    // Extract run_id from data for linking to run detail
    let run_id_from_data = alert
        .data
        .get("run_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let template = templates::AlertHistoryDetail {
        title: "Alert Detail",
        page_context: templates::PrivatePageContext::for_org_page("alerts", &session, breadcrumbs),
        alert,
        data_hot,
        data_json,
        run_id_from_data,
        delivery_details,
    };

    Html(template.render().unwrap()).into_response()
}

// =============================================================================
// Alert Destination Email Verification
// =============================================================================

/// Public handler for verifying an alert destination email address.
/// No authentication required — the recipient just clicks the link from their email.
pub async fn verify_alert_destination_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let token = params.get("token").cloned().unwrap_or_default();

    if token.is_empty() {
        return render_destination_verification_result(
            false,
            "Invalid verification link",
            "The verification link is missing required information.",
        );
    }

    match AlertDestination::verify_by_token(&db, &token).await {
        Ok(_dest) => render_destination_verification_result(
            true,
            "Email verified",
            "Your email address has been verified for alert delivery. You will now receive alerts sent to this destination.",
        ),
        Err(hot::db::alert::AlertError::NotFound) => render_destination_verification_result(
            false,
            "Invalid verification link",
            "This verification link is invalid or has already been used.",
        ),
        Err(e) => {
            render_destination_verification_result(false, "Verification failed", &e.to_string())
        }
    }
}

/// Render the alert destination verification result page (public, no login needed)
fn render_destination_verification_result(
    success: bool,
    title: &str,
    message: &str,
) -> Html<String> {
    let template = templates::AlertDestinationVerification {
        title: "Alert Destination Verification",
        page_context: templates::PublicPageContext::new("verify"),
        success,
        result_title: title,
        result_message: message,
    };
    Html(template.render().unwrap())
}

/// Resend a verification email for an unverified alert destination.
/// Requires admin role. Rate-limited to 5 resend attempts.
pub async fn resend_destination_verification_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Path(destination_id): Path<Uuid>,
) -> impl IntoResponse {
    // Require admin role
    if !session.is_current_org_admin {
        return Redirect::to("/settings/alerts/destinations?error=unauthorized");
    }

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => return Redirect::to("/settings/alerts/destinations"),
    };

    // Get the destination
    let dest = match AlertDestination::get_by_id_for_org(&db, &destination_id, &org_id).await {
        Ok(d) => d,
        Err(_) => return Redirect::to("/settings/alerts/destinations?error=not_found"),
    };

    // Must be unverified
    if dest.verified {
        return Redirect::to("/settings/alerts/destinations?info=already_verified");
    }

    // Rate limit: max 5 resend attempts
    if dest.verification_attempts >= 5 {
        return Redirect::to("/settings/alerts/destinations?error=resend_limit_reached");
    }

    // Extract email address from config
    let email_address = match EmailDestinationConfig::from_config(&dest.config) {
        Ok(cfg) => match cfg.target {
            EmailTarget::Address { address } => address,
            _ => return Redirect::to("/settings/alerts/destinations?error=not_email_address"),
        },
        Err(_) => return Redirect::to("/settings/alerts/destinations?error=invalid_config"),
    };

    // Generate new token and refresh expiry
    let new_token = AlertDestination::generate_verification_token();
    let new_expires_at = Utc::now() + chrono::Duration::hours(24);

    if let Err(e) = AlertDestination::refresh_verification_token(
        &db,
        &destination_id,
        &new_token,
        new_expires_at,
    )
    .await
    {
        tracing::error!("Failed to refresh verification token: {}", e);
        return Redirect::to("/settings/alerts/destinations?error=resend_failed");
    }

    // Send new verification email
    let org_name = session
        .current_org
        .as_ref()
        .map(|o| o.name.as_str())
        .unwrap_or("your organization");

    let email_enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
    if let Err(e) = email_enqueuer
        .send_destination_verification_email(&email_address, org_name, &dest.name, &new_token)
        .await
    {
        tracing::error!(
            "Failed to resend destination verification email to {}: {}",
            email_address,
            e
        );
        return Redirect::to("/settings/alerts/destinations?error=resend_failed");
    }

    Redirect::to("/settings/alerts/destinations?info=verification_resent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_ids_reject_malformed_values() {
        let valid = Uuid::now_v7();
        assert_eq!(parse_selected_ids(&valid.to_string()), Some(vec![valid]));
        assert!(parse_selected_ids(&format!("{},not-a-uuid", valid)).is_none());
        assert!(parse_selected_ids("").is_none());
    }

    #[tokio::test]
    async fn destination_targets_must_belong_to_current_org() {
        let db = hot::db::test_db().await;
        let owner_org_id = Uuid::now_v7();
        let foreign_org_id = Uuid::now_v7();
        let team_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();
        hot::db::Team::insert_team(&db, &team_id, &owner_org_id, "Owner Team", &user_id)
            .await
            .unwrap();
        hot::db::OrgUser::insert_org_user(
            &db,
            &Uuid::now_v7(),
            &owner_org_id,
            &user_id,
            None,
            &user_id,
        )
        .await
        .unwrap();

        let team_config = serde_json::json!({
            "target": "team",
            "team_id": team_id,
        });
        let user_config = serde_json::json!({
            "target": "user",
            "user_id": user_id,
        });

        assert!(destination_target_belongs_to_org(&db, &owner_org_id, &team_config).await);
        assert!(!destination_target_belongs_to_org(&db, &foreign_org_id, &team_config).await);
        assert!(destination_target_belongs_to_org(&db, &owner_org_id, &user_config).await);
        assert!(!destination_target_belongs_to_org(&db, &foreign_org_id, &user_config).await);
    }

    #[tokio::test]
    async fn subscription_resources_must_belong_to_current_org() {
        let db = hot::db::test_db().await;
        let org_id = Uuid::now_v7();
        let foreign_org_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();
        let channel = AlertChannel::create(&db, &org_id, None, "Owner", "^run:", &user_id)
            .await
            .unwrap();
        let destination = AlertDestination::create(
            &db,
            &org_id,
            "Owner",
            DestinationType::Webhook,
            &serde_json::json!({"url": "https://example.test/hook"}),
            &user_id,
        )
        .await
        .unwrap();
        let foreign_destination = AlertDestination::create(
            &db,
            &foreign_org_id,
            "Foreign",
            DestinationType::Webhook,
            &serde_json::json!({"url": "https://example.test/hook"}),
            &user_id,
        )
        .await
        .unwrap();

        assert!(
            subscription_resources_belong_to_org(
                &db,
                &org_id,
                &[channel.alert_channel_id],
                &[destination.alert_destination_id],
            )
            .await
        );
        assert!(
            !subscription_resources_belong_to_org(
                &db,
                &org_id,
                &[channel.alert_channel_id],
                &[foreign_destination.alert_destination_id],
            )
            .await
        );
    }
}

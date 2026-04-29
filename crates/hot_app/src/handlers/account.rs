use crate::auth::Session;
use crate::templates::{Account, AccountNotifications, PrivatePageContext};
use askama::Template;
use axum::extract::Extension;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect};
use hot::db::{DatabasePool, User};
use hot::val::Val;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct AccountUpdateForm {
    pub name: Option<String>,
    pub display_timezone: Option<String>,
    pub value_format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NotificationUpdateForm {
    pub newsletter: Option<String>,
    pub product_updates: Option<String>,
    pub alerts: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NotificationPreferences {
    pub newsletter: bool,
    pub product_updates: bool,
    #[serde(default = "default_true")]
    pub alerts: bool,
}

fn default_true() -> bool {
    true
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self {
            newsletter: true,
            product_updates: true,
            alerts: true,
        }
    }
}

/// GET /account - Display account profile page
pub async fn account_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Html<String>, (StatusCode, String)> {
    let saved = params.contains_key("saved");
    // Get user from database
    let user = User::get_user(&db, &session.user.user_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get user: {}", e),
            )
        })?;

    let billing_enabled = hot::product::billing_enabled(&conf);

    // Get user's timezone setting
    let user_timezone = User::get_display_timezone(&user).unwrap_or_else(|| "UTC".to_string());

    // Get user's value format preference
    let value_format = user.get_value_format();

    // Build page context
    let page_context = PrivatePageContext::new("account", &session);

    let template = Account {
        title: "Account",
        page_context,
        user: &user,
        billing_enabled,
        user_timezone,
        value_format,
        saved,
    };

    template
        .render()
        .map(Html)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// POST /account - Update account profile
pub async fn account_update_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<AccountUpdateForm>,
) -> Result<Redirect, (StatusCode, String)> {
    // Update user name if provided
    if let Some(name) = &form.name {
        let name_value = if name.trim().is_empty() {
            None
        } else {
            Some(name.trim())
        };

        User::update_name(&db, &session.user.user_id, name_value)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to update name: {}", e),
                )
            })?;
    }

    // Update timezone if provided
    if let Some(timezone) = &form.display_timezone {
        // Validate the timezone before saving
        if crate::timezone::is_valid_timezone(timezone) {
            User::update_display_timezone(&db, &session.user.user_id, Some(timezone.as_str()))
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to update timezone: {}", e),
                    )
                })?;
        }
    }

    // Update value format if provided
    if let Some(value_format) = &form.value_format {
        // Validate the format (must be "hot" or "json")
        let valid_format = match value_format.to_lowercase().as_str() {
            "hot" | "json" => Some(value_format.to_lowercase()),
            _ => None,
        };

        if let Some(format) = valid_format {
            User::update_value_format(&db, &session.user.user_id, Some(format.as_str()))
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to update value format: {}", e),
                    )
                })?;
        }
    }

    Ok(Redirect::to("/account?saved"))
}

/// GET /account/notifications - Display notification preferences page
pub async fn notifications_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Html<String>, (StatusCode, String)> {
    let saved = params.contains_key("saved");
    // Get user from database to get notification preferences
    let user = User::get_user(&db, &session.user.user_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to get user: {}", e),
            )
        })?;

    // Get notification preferences from settings JSON
    let prefs_json = user.get_notification_preferences();
    let notification_prefs: NotificationPreferences =
        serde_json::from_value(prefs_json).unwrap_or_default();

    // Build page context
    let page_context = PrivatePageContext::new("account", &session);

    let template = AccountNotifications {
        title: "Notification Preferences",
        page_context,
        notification_prefs: &notification_prefs,
        saved,
    };

    template
        .render()
        .map(Html)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// POST /account/notifications - Update notification preferences
pub async fn notifications_update_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<NotificationUpdateForm>,
) -> Result<Redirect, (StatusCode, String)> {
    // Update notification preferences
    let newsletter = form.newsletter.as_deref() == Some("on");
    let product_updates = form.product_updates.as_deref() == Some("on");
    let alerts = form.alerts.as_deref() == Some("on");

    let notification_prefs = NotificationPreferences {
        newsletter,
        product_updates,
        alerts,
    };

    let prefs_json = serde_json::to_value(&notification_prefs).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to serialize preferences: {}", e),
        )
    })?;

    User::update_notification_preferences(&db, &session.user.user_id, &prefs_json)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to update notification preferences: {}", e),
            )
        })?;

    Ok(Redirect::to("/account/notifications?saved"))
}

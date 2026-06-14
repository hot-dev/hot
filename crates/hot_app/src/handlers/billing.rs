use crate::auth::Session;
use crate::billing_provider::{
    BillingCheckoutRequest, BillingCheckoutSuccessRequest, BillingSubscriptionActionRequest,
    BillingWebhookRequest, billing_provider,
};
use ahash::AHashMap;
use askama::Template;
use axum::extract::Extension;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, Redirect};
use hot::db::{DatabasePool, OrgPlan, OrgUser, Plan, PlanError};
use hot::val::Val;
use serde::Deserialize;
use std::sync::Arc;

/// Check if user is org admin (role_id 2 = admin)
async fn is_org_admin(db: &DatabasePool, user_id: &uuid::Uuid, org_id: &uuid::Uuid) -> bool {
    match OrgUser::get_org_user(db, org_id, user_id).await {
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

fn billing_unavailable() -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Hot Cloud billing is not enabled for this build or product mode".to_string(),
    )
}

fn billing_error(err: crate::billing_provider::BillingProviderError) -> (StatusCode, String) {
    err.into_status_message()
}

fn is_missing_subscription(err: &PlanError) -> bool {
    matches!(err, PlanError::NotFound)
}

async fn checkout_success_redirect(
    db: &DatabasePool,
    jar: axum_extra::extract::CookieJar,
    org_id: uuid::Uuid,
    org_slug: &str,
) -> (axum_extra::extract::CookieJar, Redirect) {
    let org_cookie = axum_extra::extract::cookie::Cookie::build((
        crate::auth::CURRENT_ORG_COOKIE_NAME,
        org_id.to_string(),
    ))
    .path("/")
    .max_age(time::Duration::days(30))
    .http_only(true)
    .same_site(axum_extra::extract::cookie::SameSite::Lax)
    .build();

    let mut jar = jar.add(org_cookie);

    if let Ok(envs) = hot::db::env::Env::get_envs_by_org(db, &org_id).await
        && let Some(first_env) = envs.first()
    {
        let env_cookie = axum_extra::extract::cookie::Cookie::build((
            crate::auth::CURRENT_ENV_COOKIE_NAME,
            first_env.env_id.to_string(),
        ))
        .path("/")
        .max_age(time::Duration::days(30))
        .http_only(true)
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .build();
        jar = jar.add(env_cookie);
    }

    (
        jar,
        Redirect::to(&format!("/@{}/billing?success=1", org_slug)),
    )
}

#[derive(Deserialize)]
pub struct CreateCheckoutForm {
    pub plan_id: String,
    pub billing_period: String,
}

/// Create billing checkout session.
pub async fn create_checkout_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<CreateCheckoutForm>,
) -> Result<Redirect, (StatusCode, String)> {
    if !hot::product::billing_enabled(&conf) {
        return Err(billing_unavailable());
    }

    let org = session.current_org.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No organization selected".to_string(),
    ))?;

    if !is_org_admin(&db, &session.user.user_id, &org.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }

    if let Err(retry_after) =
        crate::rate_limit::check_checkout(&conf, &session.user.user_id, &org.org_id, &form.plan_id)
    {
        tracing::warn!(
            key_type = "checkout",
            key_hash = crate::rate_limit::key_fingerprint(&format!(
                "{}:{}:{}",
                session.user.user_id, org.org_id, form.plan_id
            )),
            "Checkout rate limit hit"
        );
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Too many checkout attempts. Please try again in {} minutes.",
                retry_after.div_ceil(60).max(1)
            ),
        ));
    }

    let checkout_url = billing_provider()
        .create_checkout(BillingCheckoutRequest {
            db: &db,
            conf: &conf,
            org_id: org.org_id,
            org_slug: &org.slug,
            org_name: &org.name,
            user_id: session.user.user_id,
            user_email: &session.user.email,
            plan_id: &form.plan_id,
            billing_period: &form.billing_period,
        })
        .await
        .map_err(billing_error)?;

    Ok(Redirect::to(&checkout_url))
}

/// Success page after billing checkout - redirects to dashboard.
pub async fn checkout_success_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(_session): axum::extract::Extension<Session>,
    Query(params): Query<AHashMap<String, String>>,
    jar: axum_extra::extract::CookieJar,
) -> Result<(axum_extra::extract::CookieJar, Redirect), Redirect> {
    if let Some(session_id) = params.get("session_id") {
        match billing_provider()
            .checkout_success(BillingCheckoutSuccessRequest {
                db: &db,
                conf: &conf,
                session_id,
            })
            .await
        {
            Ok(Some(success)) => {
                return Ok(
                    checkout_success_redirect(&db, jar, success.org_id, &success.org_slug).await,
                );
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!("Billing checkout success provider failed: {}", err.message);
            }
        }
    }

    Err(Redirect::to("/?success=1"))
}

/// Cancel page if user cancels billing checkout.
pub async fn checkout_cancel_handler(
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Redirect {
    // Redirect back to current org's billing page
    if let Some(ref org) = session.current_org {
        Redirect::to(&format!("/@{}/billing", org.slug))
    } else {
        Redirect::to("/")
    }
}

/// View subscription/billing page for account overview
pub async fn account_billing_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    use crate::templates;

    // Get all organizations the user belongs to
    let user_orgs_data = match hot::db::org::Org::get_orgs_by_user(&db, &session.user.user_id).await
    {
        Ok(orgs) => orgs,
        Err(e) => {
            tracing::error!(
                "Failed to get organizations for user {}: {}",
                session.user.user_id,
                e
            );
            Vec::new()
        }
    };

    // For each org, get the subscription and plan
    let mut user_orgs = Vec::new();
    for org in user_orgs_data {
        let subscription = match OrgPlan::get_by_org_id(&db, &org.org_id).await {
            Ok(sub) => Some(sub),
            Err(e) => {
                // Not finding a subscription is expected, but log other errors
                if !is_missing_subscription(&e) {
                    tracing::error!("Failed to get subscription for org {}: {}", org.org_id, e);
                }
                None
            }
        };
        let plan = if let Some(ref sub) = subscription {
            match Plan::get_by_id(&db, &sub.plan_uuid).await {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::error!("Failed to get subscription plan {}: {}", sub.plan_uuid, e);
                    None
                }
            }
        } else {
            None
        };

        user_orgs.push(templates::OrgWithBilling {
            org,
            subscription,
            plan,
        });
    }

    let template = templates::Billing {
        title: "Billing",
        page_context: templates::PrivatePageContext::new("billing", &session),
        user_orgs,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// View subscription/billing page for organization
pub async fn view_billing_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    use crate::templates;

    // Get current org
    let org = session.current_org.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No organization selected".to_string(),
    ))?;

    // Check if user is org admin
    if !is_org_admin(&db, &session.user.user_id, &org.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can view billing".to_string(),
        ));
    }

    // Get current subscription
    let subscription = match OrgPlan::get_by_org_id(&db, &org.org_id).await {
        Ok(sub) => Some(sub),
        Err(e) => {
            // Not finding a subscription is expected, but log other errors
            if !is_missing_subscription(&e) {
                tracing::error!("Failed to get subscription for org {}: {}", org.org_id, e);
            }
            None
        }
    };

    let plan = if let Some(ref sub) = subscription {
        match Plan::get_by_id(&db, &sub.plan_uuid).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!("Failed to get subscription plan {}: {}", sub.plan_uuid, e);
                None
            }
        }
    } else {
        None
    };

    // Check for success/cancel/reactivated messages
    // Only show full success message if subscription is actually active
    let subscription_is_active = subscription.as_ref().is_some_and(|s| s.is_active());
    let success_message = if params.contains_key("success") {
        if subscription_is_active {
            "Your subscription is now active."
        } else {
            // Payment completed but subscription not yet active - show processing message
            // The template will handle showing a refresh state
            "processing"
        }
    } else if params.contains_key("cancelled") {
        "Your subscription has been cancelled and will remain active until the end of the current billing period."
    } else if params.contains_key("reactivated") {
        "🎉 Your subscription has been reactivated!"
    } else {
        ""
    };

    let billing_provider_configured =
        hot::product::billing_enabled(&conf) && billing_provider().is_configured(&conf);

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Billing".to_string()));

    let template = templates::OrgBilling {
        title: "Billing",
        page_context: templates::PrivatePageContext::for_org_page("billing", &session, breadcrumbs),
        org,
        subscription,
        plan,
        success_message,
        billing_provider_configured,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// View usage page for organization (lightweight shell with lazy-loaded stats)
pub async fn view_usage_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    use crate::templates;
    use chrono::{Datelike, NaiveDate, Utc};
    use hot::db::{Features, Plan};

    // Get current org
    let org = session.current_org.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No organization selected".to_string(),
    ))?;

    // Get current subscription and plan (lightweight queries)
    let subscription = OrgPlan::get_by_org_id(&db, &org.org_id).await.ok();

    let plan = if let Some(ref sub) = subscription {
        Plan::get_by_id(&db, &sub.plan_uuid).await.ok()
    } else {
        None
    };

    // Resolve features (hosted billing must not grant unlimited on missing org_plan)
    let features = if hot::product::billing_enabled(&conf) {
        Features::resolve_for_hosted_org(&db, &org.org_id).await
    } else {
        Features::resolve_for_org(&db, &org.org_id).await
    };

    // Use calendar month start for display
    let now = Utc::now();
    let month_start = now
        .date_naive()
        .with_day(1)
        .and_then(|d: NaiveDate| d.and_hms_opt(0, 0, 0))
        .map(|dt| chrono::DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc))
        .unwrap_or(now);

    // Determine if user can upgrade (false for Scale plan)
    let can_upgrade = if hot::product::is_no_nag(&conf) {
        false
    } else if hot::product::billing_enabled(&conf) {
        plan.as_ref()
            .map(|p| !p.plan_name.contains("Scale") && !p.plan_name.contains("Self-Host"))
            .unwrap_or(true)
    } else {
        hot::product::should_show_cloud_upsells(&conf)
    };

    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Usage".to_string()));

    let template = templates::OrgUsage {
        title: "Usage",
        page_context: templates::PrivatePageContext::for_org_page("usage", &session, breadcrumbs),
        org,
        plan,
        features,
        month_start,
        can_upgrade,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// Fetch usage stats via HTMX (returns partial HTML)
pub async fn usage_stats_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    use crate::templates;
    use chrono::{Datelike, NaiveDate, Utc};
    use hot::db::{Features, OrgUsageStats, Plan};

    // Get current org
    let org = session.current_org.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No organization selected".to_string(),
    ))?;

    // Get current subscription and plan
    let subscription = OrgPlan::get_by_org_id(&db, &org.org_id).await.ok();

    let plan = if let Some(ref sub) = subscription {
        Plan::get_by_id(&db, &sub.plan_uuid).await.ok()
    } else {
        None
    };

    // Resolve features (hosted billing must not grant unlimited on missing org_plan)
    let features = if session.billing_enabled {
        Features::resolve_for_hosted_org(&db, &org.org_id).await
    } else {
        Features::resolve_for_org(&db, &org.org_id).await
    };

    // Use calendar month start for run limits (runs_per_month resets on 1st of each month)
    let now = Utc::now();
    let month_start = now
        .date_naive()
        .with_day(1)
        .and_then(|d: NaiveDate| d.and_hms_opt(0, 0, 0))
        .map(|dt| chrono::DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc))
        .unwrap_or(now);

    // Get call retention days from features
    let retention_days = features.call_retention_days();

    // Calculate real-time usage stats
    let usage = OrgUsageStats::calculate(&db, &org.org_id, month_start, retention_days)
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to calculate usage stats for org {}: {}",
                org.org_id,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to calculate usage".to_string(),
            )
        })?;

    // Determine if user can upgrade (false for Scale plan and no-nag self-host).
    let can_upgrade = if session.is_self_host_experience() {
        false
    } else if session.billing_enabled {
        plan.as_ref()
            .map(|p| !p.plan_name.contains("Scale") && !p.plan_name.contains("Self-Host"))
            .unwrap_or(true)
    } else {
        session.is_local_dev_experience()
    };

    let template = templates::OrgUsageStats {
        org,
        plan,
        features,
        usage,
        month_start,
        can_upgrade,
        is_local_dev: !session.billing_enabled,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    ))
}

/// Cancel subscription (at period end)
pub async fn cancel_subscription_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, (StatusCode, String)> {
    if !hot::product::billing_enabled(&conf) {
        return Err(billing_unavailable());
    }

    let org = session.current_org.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No organization selected".to_string(),
    ))?;

    if !is_org_admin(&db, &session.user.user_id, &org.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }

    let subscription = OrgPlan::get_by_org_id(&db, &org.org_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to get subscription for org {} during cancel: {}",
                org.org_id,
                e
            );
            (StatusCode::NOT_FOUND, "No subscription found".to_string())
        })?;

    billing_provider()
        .cancel_subscription(BillingSubscriptionActionRequest {
            db: &db,
            conf: &conf,
            org_plan: &subscription,
        })
        .await
        .map_err(billing_error)?;

    OrgPlan::mark_cancel_at_period_end(&db, &subscription.org_plan_id, &session.user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let plan_name = if let Ok(plan) = Plan::get_by_id(&db, &subscription.plan_uuid).await {
        plan.plan_name
    } else {
        "Hot Cloud".to_string()
    };
    let period_end = subscription
        .current_period_end
        .map(|d| d.format("%B %d, %Y").to_string())
        .unwrap_or_else(|| "the end of your billing period".to_string());
    send_subscription_email(
        &db,
        &conf,
        &org.org_id,
        &org.name,
        &plan_name,
        Some(&period_end),
        false,
    )
    .await;

    Ok(Redirect::to(&format!(
        "/@{}/billing?cancelled=true",
        org.slug
    )))
}

/// Reactivate a subscription that was scheduled to cancel
pub async fn reactivate_subscription_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, (StatusCode, String)> {
    if !hot::product::billing_enabled(&conf) {
        return Err(billing_unavailable());
    }

    let org = session.current_org.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        "No organization selected".to_string(),
    ))?;

    if !is_org_admin(&db, &session.user.user_id, &org.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }

    let subscription = OrgPlan::get_by_org_id(&db, &org.org_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to get subscription for org {} during reactivate: {}",
                org.org_id,
                e
            );
            (StatusCode::NOT_FOUND, "No subscription found".to_string())
        })?;

    billing_provider()
        .reactivate_subscription(BillingSubscriptionActionRequest {
            db: &db,
            conf: &conf,
            org_plan: &subscription,
        })
        .await
        .map_err(billing_error)?;

    OrgPlan::reactivate(&db, &subscription.org_plan_id, &session.user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let plan_name = if let Ok(plan) = Plan::get_by_id(&db, &subscription.plan_uuid).await {
        plan.plan_name
    } else {
        "Hot Cloud".to_string()
    };
    send_subscription_email(&db, &conf, &org.org_id, &org.name, &plan_name, None, true).await;

    Ok(Redirect::to(&format!(
        "/@{}/billing?reactivated=true",
        org.slug
    )))
}

/// Billing provider webhook handler.
pub async fn billing_webhook_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    headers: HeaderMap,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    if !hot::product::billing_enabled(&conf) {
        return Err(billing_unavailable());
    }

    billing_provider()
        .handle_webhook(BillingWebhookRequest {
            db: &db,
            conf: &conf,
            headers: &headers,
            body: &body,
        })
        .await
        .map_err(billing_error)?;

    Ok(StatusCode::OK)
}

/// Send welcome email after successful checkout (best effort)
pub async fn send_welcome_email_for_checkout(
    db: &DatabasePool,
    conf: &Val,
    org_id: &uuid::Uuid,
    plan_uuid: &uuid::Uuid,
) {
    use crate::email::{AppEmailEnqueuer, AppEmailSender};

    // Get org details
    let org = match hot::db::org::Org::get_org(db, org_id).await {
        Ok(org) => org,
        Err(e) => {
            tracing::warn!("Could not get org {} for welcome email: {}", org_id, e);
            return;
        }
    };

    let plan = match Plan::get_by_id(db, plan_uuid).await {
        Ok(plan) => plan,
        Err(e) => {
            tracing::warn!("Could not get plan {} for welcome email: {}", plan_uuid, e);
            return;
        }
    };

    // Get an admin user for this org to send the email to
    let org_users = match OrgUser::get_admins_for_org(db, org_id).await {
        Ok(users) => users,
        Err(e) => {
            tracing::warn!("Could not get org admins for welcome email: {}", e);
            return;
        }
    };

    if org_users.is_empty() {
        tracing::warn!(
            "No admin users found for org {} - skipping welcome email",
            org_id
        );
        return;
    }

    // Enqueue welcome email to all admins
    let email_enqueuer = AppEmailEnqueuer::from_conf(std::sync::Arc::new(db.clone()), conf);

    for org_user in org_users {
        // Get user details
        let user = match hot::db::User::get_user(db, &org_user.user_id).await {
            Ok(user) => user,
            Err(e) => {
                tracing::warn!(
                    "Could not get user {} for welcome email: {}",
                    org_user.user_id,
                    e
                );
                continue;
            }
        };

        if let Err(e) = email_enqueuer
            .send_welcome_email(
                &user.email,
                user.name.as_deref(),
                &org.name,
                &plan.plan_name,
            )
            .await
        {
            tracing::warn!("Failed to enqueue welcome email to {}: {}", user.email, e);
        }
    }
}

/// Send subscription cancellation or reactivation email to all org admins (best effort)
pub async fn send_subscription_email(
    db: &DatabasePool,
    conf: &Val,
    org_id: &uuid::Uuid,
    org_name: &str,
    plan_name: &str,
    period_end: Option<&str>,
    is_reactivation: bool,
) {
    use crate::email::{AppEmailEnqueuer, AppEmailSender};

    // Get admin users for this org
    let org_users = match OrgUser::get_admins_for_org(db, org_id).await {
        Ok(users) => users,
        Err(e) => {
            tracing::warn!("Could not get org admins for subscription email: {}", e);
            return;
        }
    };

    if org_users.is_empty() {
        tracing::warn!(
            "No admin users found for org {} - skipping subscription email",
            org_id
        );
        return;
    }

    let email_enqueuer = AppEmailEnqueuer::from_conf(std::sync::Arc::new(db.clone()), conf);

    let email_type = if is_reactivation {
        "reactivation"
    } else {
        "cancellation"
    };

    for org_user in org_users {
        // Get user details
        let user = match hot::db::User::get_user(db, &org_user.user_id).await {
            Ok(user) => user,
            Err(e) => {
                tracing::warn!(
                    "Could not get user {} for {} email: {}",
                    org_user.user_id,
                    email_type,
                    e
                );
                continue;
            }
        };

        let result = if is_reactivation {
            email_enqueuer
                .send_reactivation_email(&user.email, user.name.as_deref(), org_name, plan_name)
                .await
        } else {
            email_enqueuer
                .send_cancellation_email(
                    &user.email,
                    user.name.as_deref(),
                    org_name,
                    plan_name,
                    period_end.unwrap_or("the end of your billing period"),
                )
                .await
        };

        if let Err(e) = result {
            tracing::warn!(
                "Failed to enqueue {} email to {}: {}",
                email_type,
                user.email,
                e
            );
        }
    }
}

use crate::auth::Session;
use crate::billing_provider::{BillingCheckoutRequest, billing_provider};
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::{DatabasePool, Plan};
use hot::val::Val;
use std::sync::Arc;

/// Display checkout form page before redirecting to the billing provider.
/// This uses a minimal public layout to prevent users from navigating
/// around the app before completing their subscription.
pub async fn checkout_form_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<crate::auth::Session>,
    Query(params): Query<AHashMap<String, String>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;

    let plan_id = params.get("plan").ok_or((
        StatusCode::BAD_REQUEST,
        "Missing plan parameter".to_string(),
    ))?;

    let billing_period = match params.get("billing").map(String::as_str) {
        Some("annual") => "annual",
        _ => "monthly",
    };

    // Resolve the org to bill against. Prefer `session.current_org` (set by
    // the cookie) when present, with a fresh DB read as fallback — the
    // session snapshot can be a beat behind on a freshly-created org. Users
    // with no org yet are sent to /claim-handle to pick a handle first.
    let org_slug: Option<String> = if let Some(org) = session.current_org.as_ref() {
        if org.is_child() {
            let lookup = db.with_scope(hot::db::DatabaseScope::Bootstrap(
                hot::db::BootstrapOperation::Membership,
            ));
            hot::db::Org::get_org_uncached(&lookup, &org.billing_owner_id())
                .await
                .ok()
                .map(|root| root.slug)
        } else {
            Some(org.slug.clone())
        }
    } else if let Ok(orgs) =
        hot::db::org::Org::get_orgs_by_user(&db, &session.current_user_id()).await
    {
        let mut resolved = None;
        // Prefer a root org where the user is an administrator.
        for org in orgs.iter().filter(|o| !o.is_child()) {
            if super::billing::is_org_admin(&db, &session.current_user_id(), &org.org_id).await {
                resolved = Some(org.slug.clone());
                break;
            }
        }
        // Otherwise fall back to the first root org, or resolve the first child's root.
        if resolved.is_none() {
            if let Some(root) = orgs.iter().find(|o| !o.is_child()) {
                resolved = Some(root.slug.clone());
            } else if let Some(first) = orgs.first() {
                let lookup = db.with_scope(hot::db::DatabaseScope::Bootstrap(
                    hot::db::BootstrapOperation::Membership,
                ));
                resolved = hot::db::Org::get_org_uncached(&lookup, &first.billing_owner_id())
                    .await
                    .ok()
                    .map(|root| root.slug);
            }
        }
        resolved
    } else {
        None
    };

    let encoded_plan = urlencoding::encode(plan_id);
    if let Some(slug) = org_slug {
        Ok(axum::response::Redirect::to(&format!(
            "/@{}/billing/checkout?plan={}&billing={}",
            slug, encoded_plan, billing_period
        ))
        .into_response())
    } else {
        tracing::info!(
            "checkout_form_handler: no org for user {}; redirecting to /claim-handle",
            session.current_user_id()
        );
        Ok(axum::response::Redirect::to(&format!(
            "/claim-handle?plan={}&billing={}",
            encoded_plan, billing_period
        ))
        .into_response())
    }
}

/// Display checkout form for a specific organization
/// This is used when an existing user creates a new organization
/// If no plan is specified, shows a plan selection page
pub async fn org_checkout_form_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<AHashMap<String, String>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    if !hot::product::billing_enabled(&conf) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Hot Cloud billing is not enabled for this build or product mode".to_string(),
        ));
    }

    let org = hot::db::org::Org::get_org_by_slug(&db, &org_slug)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Organization not found".to_string()))?;

    // Check via a direct membership query rather than `session.user_orgs`
    // because the session snapshot can be a beat behind on freshly-created
    // orgs. Billing management still requires an org admin.
    if !super::billing::is_org_admin(&db, &session.current_user_id(), &org.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }
    let db = Arc::new(db.with_scope(hot::db::DatabaseScope::Maintenance {
        operation: hot::db::MaintenanceOperation::Billing,
        org_id: Some(org.org_id),
    }));

    // Get all active plans for plan selection
    let all_plans: Vec<Plan> = Plan::get_all_active(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.plan_id.is_some())
        .filter(|p| !p.plan_name.contains("Self-Host"))
        .collect();

    // Load authoritative current org plan state under billing scope.
    let current_org_plan = hot::db::OrgPlan::get_by_org_id(&db, &org.org_id).await.ok();
    let current_subscription_active = current_org_plan.as_ref().is_some_and(|s| s.is_active());
    let current_billing_period = current_org_plan.as_ref().map(|s| s.billing_period.clone());
    let current_plan_obj = if let Some(ref sub) = current_org_plan {
        all_plans
            .iter()
            .find(|p| p.plan_uuid == sub.plan_uuid)
            .cloned()
    } else {
        None
    };
    let current_plan_name = current_plan_obj
        .as_ref()
        .map(|p| p.plan_name.clone())
        .or_else(|| session.current_org_plan_name.clone());
    let current_plan_sort_order = current_plan_obj.as_ref().map(|p| p.sort_order);

    // If no plan specified, show plan selection page
    let plan_id = match params.get("plan") {
        Some(id) if !id.is_empty() => id,
        _ => {
            let template = templates::OrgPlanSelect {
                title: "Choose a Plan",
                page_context: templates::PrivatePageContext::new("billing", &session),
                org: &org,
                plans: all_plans,
                current_plan_name,
                current_plan_sort_order,
                current_billing_period,
                current_subscription_active,
            };
            return Ok(Html(
                template
                    .render()
                    .unwrap_or_else(|_| "Template error".into()),
            )
            .into_response());
        }
    };

    let billing_period = match params.get("billing").map(String::as_str) {
        Some("annual") => "annual",
        _ => "monthly",
    };

    // Get plan details by plan_id
    let plan = Plan::get_by_plan_id(&db, plan_id)
        .await
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid plan".to_string()))?;

    let has_active_paid_subscription =
        current_subscription_active && current_plan_obj.as_ref().is_some_and(|p| !p.is_free_plan());

    // The free plan renders the same form (worded as card verification). A GET never
    // creates a provider checkout session; the form's POST does.
    let template = templates::OrgCheckoutForm {
        title: if has_active_paid_subscription {
            "Change Subscription Plan"
        } else {
            "Complete Subscription"
        },
        page_context: templates::PublicPageContext::new("checkout"),
        org,
        plan,
        billing_period,
        all_plans,
        has_active_paid_subscription,
    };

    Ok(Html(
        template
            .render()
            .unwrap_or_else(|_| "Template error".into()),
    )
    .into_response())
}

/// Handle the POST from the org checkout form to create a provider checkout session.
pub async fn org_create_checkout_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    axum::extract::Form(form): axum::extract::Form<AHashMap<String, String>>,
) -> Result<Redirect, (StatusCode, String)> {
    if !hot::product::billing_enabled(&conf) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Hot Cloud billing is not enabled for this build or product mode".to_string(),
        ));
    }

    // Verify org exists and user is an admin. As with the GET handler, check
    // membership directly rather than via `session.user_orgs` so a stale
    // snapshot doesn't lock a freshly-onboarded user out of their own checkout.
    let org = hot::db::org::Org::get_org_by_slug(&db, &org_slug)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Organization not found".to_string()))?;

    if !super::billing::is_org_admin(&db, &session.current_user_id(), &org.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }
    let db = Arc::new(db.with_scope(hot::db::DatabaseScope::Maintenance {
        operation: hot::db::MaintenanceOperation::Billing,
        org_id: Some(org.org_id),
    }));

    let plan_id = form
        .get("plan_id")
        .ok_or((StatusCode::BAD_REQUEST, "Missing plan_id".to_string()))?;

    let raw_billing_period = form
        .get("billing_period")
        .map(String::as_str)
        .unwrap_or("monthly");
    let billing_period = super::billing::validate_billing_period(raw_billing_period)?;

    if let Err(retry_after) =
        crate::rate_limit::check_checkout(&conf, &session.user.user_id, &org.org_id, plan_id)
    {
        tracing::warn!(
            key_type = "checkout",
            key_hash = crate::rate_limit::key_fingerprint(&format!(
                "{}:{}:{}",
                session.user.user_id, org.org_id, plan_id
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
            plan_id,
            billing_period,
        })
        .await
        .map_err(|err| err.into_status_message())?;

    Ok(Redirect::to(&checkout_url))
}

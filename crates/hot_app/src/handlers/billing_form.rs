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

    let billing_period = params
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

    // Resolve the org to bill against. Prefer `session.current_org` (set by
    // the cookie) when present, with a fresh DB read as fallback — the
    // session snapshot can be a beat behind on a freshly-created org. Users
    // with no org yet are sent to /claim-handle to pick a handle first.
    let org_slug: Option<String> = if let Some(org) = session.current_org.as_ref() {
        Some(org.slug.clone())
    } else {
        hot::db::org::Org::get_orgs_by_user(&db, &session.current_user_id())
            .await
            .ok()
            .and_then(|orgs| orgs.first().map(|o| o.slug.clone()))
    };

    if let Some(slug) = org_slug {
        Ok(axum::response::Redirect::to(&format!(
            "/@{}/billing/checkout?plan={}&billing={}",
            slug, plan_id, billing_period
        ))
        .into_response())
    } else {
        tracing::info!(
            "checkout_form_handler: no org for user {}; redirecting to /claim-handle",
            session.current_user_id()
        );
        Ok(axum::response::Redirect::to(&format!(
            "/claim-handle?plan={}&billing={}",
            plan_id, billing_period
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
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Query(params): Query<AHashMap<String, String>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let org = hot::db::org::Org::get_org_by_slug(&db, &org_slug)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Organization not found".to_string()))?;

    // Check via a direct membership query rather than `session.user_orgs`
    // because the session snapshot can be a beat behind on freshly-created
    // orgs. Billing management still requires an org admin.
    let org_user =
        hot::db::org::OrgUser::get_org_user(&db, &org.org_id, &session.current_user_id())
            .await
            .map_err(|_| (StatusCode::FORBIDDEN, "Access denied".to_string()))?;
    if org_user.org_user_role_id != 2 {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }

    // Get all active plans for plan selection
    let all_plans: Vec<Plan> = Plan::get_all_active(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.plan_id.is_some())
        .filter(|p| !p.plan_name.contains("Self-Host"))
        .collect();

    // If no plan specified, show plan selection page
    let plan_id = match params.get("plan") {
        Some(id) if !id.is_empty() => id,
        _ => {
            let current_plan_sort_order = session.current_org_plan_name.as_ref().and_then(|name| {
                all_plans
                    .iter()
                    .find(|plan| plan.plan_name == *name)
                    .map(|plan| plan.sort_order)
            });
            let template = templates::OrgPlanSelect {
                title: "Choose a Plan",
                page_context: templates::PrivatePageContext::new("billing", &session),
                org: &org,
                plans: all_plans,
                current_plan_name: session.current_org_plan_name.clone(),
                current_plan_sort_order,
            };
            return Ok(Html(
                template
                    .render()
                    .unwrap_or_else(|_| "Template error".into()),
            )
            .into_response());
        }
    };

    let billing_period = params
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

    // Get plan details by plan_id
    let plan = Plan::get_by_plan_id(&db, plan_id)
        .await
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid plan".to_string()))?;

    let template = templates::OrgCheckoutForm {
        title: "Complete Subscription",
        page_context: templates::PublicPageContext::new("checkout"),
        org,
        plan,
        billing_period: billing_period.as_str(),
        all_plans,
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

    let org_user =
        hot::db::org::OrgUser::get_org_user(&db, &org.org_id, &session.current_user_id())
            .await
            .map_err(|_| (StatusCode::FORBIDDEN, "Access denied".to_string()))?;
    if org_user.org_user_role_id != 2 {
        return Err((
            StatusCode::FORBIDDEN,
            "Only organization admins can manage billing".to_string(),
        ));
    }

    let plan_id = form
        .get("plan_id")
        .ok_or((StatusCode::BAD_REQUEST, "Missing plan_id".to_string()))?;

    let billing_period = form
        .get("billing_period")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

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
            billing_period: &billing_period,
        })
        .await
        .map_err(|err| err.into_status_message())?;

    Ok(Redirect::to(&checkout_url))
}

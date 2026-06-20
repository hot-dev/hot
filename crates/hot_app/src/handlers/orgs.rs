use crate::auth::Session;
use crate::billing_provider::{BillingCheckoutRequest, billing_provider};
use crate::email::{AppEmailEnqueuer, AppEmailSender};
use crate::handlers::list_query;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::{DatabasePool, Plan};
use hot::val::Val;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

// Form data structure for organization creation/editing
#[derive(Deserialize, Debug)]
pub struct OrgForm {
    pub name: String,
    pub slug: String,
    pub account_type: Option<String>,
    pub plan_id: Option<String>,
    pub billing_period: Option<String>,
    pub display_timezone: Option<String>,
}

// Form data structure for inviting users
#[derive(Deserialize, Debug)]
pub struct InviteForm {
    pub email: String,
    pub role_id: i16,
}

// Form data structure for editing org users
#[derive(Deserialize, Debug)]
pub struct OrgUserEditForm {
    pub role_id: i16,
    pub active: bool,
}

pub async fn orgs_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(_params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get the user's organizations
    let orgs = hot::db::org::Org::get_orgs_by_user(&db, &session.user.user_id)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get organizations: {}", e);
            Vec::new()
        });

    let total_orgs = orgs.len() as i64;

    // No pagination needed for user's orgs (typically small list)
    let current_page_num = 1i64;
    let total_pages = 1i64;
    let has_next_page = false;
    let has_prev_page = false;
    let start_page = 1i64;
    let end_page = 1i64;

    // Build breadcrumbs: Organizations
    let breadcrumbs = vec![templates::BreadcrumbItem::current(
        "Organizations".to_string(),
    )];

    let template = templates::OrgsList {
        title: "Organizations",
        page_context: templates::PrivatePageContext::for_org_page("orgs", &session, breadcrumbs),
        orgs,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_orgs,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn orgs_new_handler(
    State(pool): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: Organizations / New
    let breadcrumbs = vec![
        templates::BreadcrumbItem::clickable("Organizations".to_string(), "/orgs".to_string()),
        templates::BreadcrumbItem::current("New".to_string()),
    ];

    // Get available plans for plan selection (Hot Cloud only)
    let plans = if hot::product::billing_enabled(&conf) {
        hot::db::Plan::get_all_active(&pool)
            .await
            .unwrap_or_default()
    } else {
        vec![]
    };

    let template = templates::OrgsNew {
        title: "New Organization",
        page_context: templates::PrivatePageContext::for_org_page("orgs", &session, breadcrumbs),
        error_message: "",
        org_name: "",
        org_slug: "",
        account_type: "organization",
        plans,
        selected_plan: "",
        selected_billing: "monthly",
        is_local_dev: !hot::product::billing_enabled(&conf),
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn orgs_detail_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => {
            // SECURITY: Verify user has access to this organization
            if !session.has_org_access(&org.org_id) {
                let template = templates::OrgsNotFound {
                    title: "Organization Not Found",
                    page_context: templates::PrivatePageContext::new("orgs", &session),
                    org_slug,
                };
                return Html(template.render().unwrap()).into_response();
            }

            // Build breadcrumbs: Organizations / <org_name>
            let breadcrumbs = vec![
                templates::BreadcrumbItem::clickable(
                    "Organizations".to_string(),
                    "/orgs".to_string(),
                ),
                templates::BreadcrumbItem::current(org.name.clone()),
            ];

            let template = templates::OrgsDetail {
                title: &format!("{} - Organization", org.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "organization",
                    &session,
                    breadcrumbs,
                ),
                org,
                active_page: "organization",
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Organization not found
            let template = templates::OrgsNotFound {
                title: "Organization Not Found",
                page_context: templates::PrivatePageContext::new("orgs", &session),
                org_slug,
            };

            Html(template.render().unwrap()).into_response()
        }
    }
}

pub async fn orgs_edit_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => {
            // SECURITY: Verify user has access to this organization
            if !session.has_org_access(&org.org_id) {
                return Redirect::to("/orgs").into_response();
            }

            // Build breadcrumbs: Organizations / <org_name> / Edit
            let breadcrumbs = vec![
                templates::BreadcrumbItem::clickable(
                    "Organizations".to_string(),
                    "/orgs".to_string(),
                ),
                templates::BreadcrumbItem::new(org.name.clone(), None),
                templates::BreadcrumbItem::current("Edit".to_string()),
            ];

            // Get org timezone setting
            let org_timezone =
                hot::db::org::Org::get_display_timezone(&org).unwrap_or_else(|| "UTC".to_string());

            let template = templates::OrgsEdit {
                title: &format!("Edit {} - Organization", org.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "organization",
                    &session,
                    breadcrumbs,
                ),
                org,
                error_message: "",
                org_timezone,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Organization not found, redirect to orgs list
            Redirect::to("/orgs").into_response()
        }
    }
}

pub async fn orgs_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    jar: axum_extra::extract::CookieJar,
    Form(form): Form<OrgForm>,
) -> Result<(axum_extra::extract::CookieJar, Redirect), Html<String>> {
    let billing_enabled = hot::product::billing_enabled(&conf);

    // Get plans for error rendering
    let plans = if billing_enabled {
        Plan::get_all_active(&db).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let selected_plan = form.plan_id.as_deref().unwrap_or("");
    let selected_billing = form.billing_period.as_deref().unwrap_or("monthly");
    let account_type = form.account_type.as_deref().unwrap_or("organization");

    // Local-dev experience is single-user oriented; self-host creates orgs directly.
    if session.is_local_dev_experience() {
        return Err(render_orgs_new_with_error(
            &session,
            "Creating organizations is not available in local development.",
            &form.name,
            &form.slug,
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    // Validate form data
    if form.name.trim().is_empty() {
        return Err(render_orgs_new_with_error(
            &session,
            "Organization name is required",
            "",
            "",
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    if form.slug.trim().is_empty() {
        return Err(render_orgs_new_with_error(
            &session,
            "Organization slug is required",
            &form.name,
            "",
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    // Require plan selection in Hot Cloud
    if billing_enabled && (form.plan_id.is_none() || form.plan_id.as_deref() == Some("")) {
        return Err(render_orgs_new_with_error(
            &session,
            "Please select a plan",
            &form.name,
            &form.slug,
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    // Limit one individual org per user
    if account_type == "individual"
        && let Ok(Some(_)) =
            hot::db::org::Org::get_individual_org_by_user(&db, &session.current_user_id()).await
    {
        return Err(render_orgs_new_with_error(
            &session,
            "You already have an individual organization. Choose \"Organization\" to create a team org.",
            &form.name,
            &form.slug,
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    // Validate slug format + reserved-word rules (baked-in + the
    // deployment-supplied `hot.org.reserved-slugs` list) before hitting the DB.
    let extra_reserved = crate::slug::extra_reserved_from_conf(&conf);
    if let Err(e) = crate::slug::validate_format_with_extra(&form.slug, &extra_reserved) {
        return Err(render_orgs_new_with_error(
            &session,
            e.message(),
            &form.name,
            &form.slug,
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    // Back-button recovery: if the user already owns this slug, forward to checkout
    // instead of showing a "taken" error.
    if let Ok(existing_org) = hot::db::org::Org::get_org_by_slug(&db, &form.slug).await
        && existing_org.created_by_user_id == session.current_user_id()
    {
        if !billing_enabled {
            return Ok((jar, Redirect::to(&format!("/@{}", form.slug))));
        }

        let plan_id = form.plan_id.clone().unwrap_or_default();
        let billing = form
            .billing_period
            .clone()
            .unwrap_or_else(|| "monthly".to_string());
        return Ok((
            jar,
            Redirect::to(&format!(
                "/@{}/billing/checkout?plan={}&billing={}",
                form.slug, plan_id, billing
            )),
        ));
    }

    // Availability check against existing orgs + non-expired pending verifications.
    // On conflict, suggest an alternative so the user has a one-click retry.
    if !crate::slug::is_available(&db, &form.slug).await {
        let suggestion = crate::slug::suggest_alternative(&db, &form.slug).await;
        return Err(render_orgs_new_with_error(
            &session,
            &format!(
                "{} Try \u{201c}{}\u{201d}.",
                crate::slug::SlugError::Taken.message(),
                suggestion
            ),
            &form.name,
            &suggestion,
            account_type,
            plans,
            selected_plan,
            selected_billing,
        ));
    }

    // Generate new org ID
    let org_id = uuid::Uuid::now_v7();
    let org_user_id = uuid::Uuid::now_v7();

    // Create organization
    match hot::db::org::Org::insert_org(
        &db,
        &org_id,
        &form.name,
        &form.slug,
        account_type,
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Add the creator as an admin of the organization
            let _ = hot::db::org::OrgUser::insert_org_user(
                &db,
                &org_user_id,
                &org_id,
                &session.current_user_id(),
                Some(2), // admin role
                &session.current_user_id(),
            )
            .await;

            // Create default "development" environment
            let env_id = uuid::Uuid::now_v7();
            let _ = hot::db::Env::insert_env(
                &db,
                &env_id,
                &org_id,
                "development",
                &session.current_user_id(),
            )
            .await;

            // Set the current org cookie so user is on this org after checkout
            let org_cookie = axum_extra::extract::cookie::Cookie::build((
                crate::auth::CURRENT_ORG_COOKIE_NAME,
                org_id.to_string(),
            ))
            .path("/")
            .max_age(time::Duration::days(30))
            .http_only(true)
            .same_site(axum_extra::extract::cookie::SameSite::Lax)
            .build();

            // Set the environment cookie to the new development env
            let env_cookie = axum_extra::extract::cookie::Cookie::build((
                crate::auth::CURRENT_ENV_COOKIE_NAME,
                env_id.to_string(),
            ))
            .path("/")
            .max_age(time::Duration::days(30))
            .http_only(true)
            .same_site(axum_extra::extract::cookie::SameSite::Lax)
            .build();

            let jar = jar.add(org_cookie).add(env_cookie);

            if !billing_enabled {
                return Ok((jar, Redirect::to(&format!("/@{}", form.slug))));
            }

            let plan_id_str = form.plan_id.clone().unwrap_or_default();
            let billing_str = form
                .billing_period
                .clone()
                .unwrap_or_else(|| "monthly".to_string());

            if let Err(retry_after) = crate::rate_limit::check_checkout(
                &conf,
                &session.user.user_id,
                &org_id,
                &plan_id_str,
            ) {
                tracing::warn!(
                    key_type = "checkout",
                    key_hash = crate::rate_limit::key_fingerprint(&format!(
                        "{}:{}:{}",
                        session.user.user_id, org_id, plan_id_str
                    )),
                    "Checkout rate limit hit"
                );
                tracing::info!(
                    "Checkout throttled after org creation; retry_after_secs={}",
                    retry_after
                );
                return Ok((jar, Redirect::to(&format!("/@{}/billing", form.slug))));
            }

            let checkout_url = match billing_provider()
                .create_checkout(BillingCheckoutRequest {
                    db: &db,
                    conf: &conf,
                    org_id,
                    org_slug: &form.slug,
                    org_name: &form.name,
                    user_id: session.user.user_id,
                    user_email: &session.user.email,
                    plan_id: &plan_id_str,
                    billing_period: &billing_str,
                })
                .await
            {
                Ok(url) => url,
                Err(err) => {
                    tracing::error!(
                        "Failed to start billing checkout for org '{}' ({}): {}",
                        form.slug,
                        org_id,
                        err.message
                    );
                    return Ok((jar, Redirect::to(&format!("/@{}/billing", form.slug))));
                }
            };

            Ok((jar, Redirect::to(&checkout_url)))
        }
        Err(_) => Err(render_orgs_new_with_error(
            &session,
            "Failed to create organization",
            &form.name,
            &form.slug,
            account_type,
            plans,
            selected_plan,
            selected_billing,
        )),
    }
}

pub async fn orgs_update_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<OrgForm>,
) -> Result<Redirect, Html<String>> {
    // Get the organization
    let org = match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => org,
        Err(_) => return Ok(Redirect::to("/orgs")),
    };

    // Check if user is admin of this organization
    if !session.is_org_admin(&db, &org.org_id).await {
        return Err(render_orgs_edit_with_error(
            &session,
            &org,
            "You must be an admin to edit organizations",
        ));
    }

    // Validate form data
    if form.name.trim().is_empty() {
        return Err(render_orgs_edit_with_error(
            &session,
            &org,
            "Organization name is required",
        ));
    }

    if form.slug.trim().is_empty() {
        return Err(render_orgs_edit_with_error(
            &session,
            &org,
            "Organization slug is required",
        ));
    }

    // If the slug is changing, run the same full validation we apply at
    // creation time: format rules, reserved-word list, and availability
    // (existing orgs + non-expired pending verifications). When the slug is
    // unchanged we skip these checks so orgs grandfathered in before a name
    // was reserved aren't blocked from editing other fields.
    if form.slug != org.slug {
        let extra_reserved = crate::slug::extra_reserved_from_conf(&conf);
        if let Err(e) = crate::slug::validate_format_with_extra(&form.slug, &extra_reserved) {
            return Err(render_orgs_edit_with_error(&session, &org, e.message()));
        }

        if !crate::slug::is_available(&db, &form.slug).await {
            let suggestion = crate::slug::suggest_alternative(&db, &form.slug).await;
            return Err(render_orgs_edit_with_error(
                &session,
                &org,
                &format!(
                    "{} Try \u{201c}{}\u{201d}.",
                    crate::slug::SlugError::Taken.message(),
                    suggestion
                ),
            ));
        }
    }

    // Update organization
    match hot::db::org::Org::update_org(
        &db,
        &org.org_id,
        &form.name,
        &form.slug,
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Update timezone if provided
            if let Some(ref timezone) = form.display_timezone
                && crate::timezone::is_valid_timezone(timezone)
            {
                let _ = hot::db::org::Org::update_display_timezone(
                    &db,
                    &org.org_id,
                    Some(timezone.as_str()),
                    &session.current_user_id(),
                )
                .await;
            }

            // Redirect to the updated organization's detail page
            Ok(Redirect::to(&format!("/@{}", form.slug)))
        }
        Err(_) => Err(render_orgs_edit_with_error(
            &session,
            &org,
            "Failed to update organization",
        )),
    }
}

pub async fn org_users_list_handler(
    Path(org_slug): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Parse query parameters
    const USERS_PER_PAGE: i64 = 10;
    let page = list_query::PageParams::parse(&params, USERS_PER_PAGE);
    let current_page_num = page.current_page_num;
    let offset = page.offset;

    // Get the organization
    let org = match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => org,
        Err(_) => return Redirect::to("/orgs").into_response(),
    };

    // Check if user has access to this organization
    if !session.has_org_access(&org.org_id) {
        return Redirect::to("/orgs").into_response();
    }

    // Get org users with roles
    let all_users = match hot::db::org::OrgUser::get_users_with_roles_by_org(&db, &org.org_id).await
    {
        Ok(users) => users
            .into_iter()
            .map(|user| templates::OrgUserDisplay {
                user_id: user.user_id.to_string(),
                email: user.email,
                name: Some(user.name),
                role_name: user.role,
                org_user_role_id: user.org_user_role_id,
                active: user.active,
                created_at_formatted: format!(
                    "{} {}",
                    crate::timezone::format_in_timezone(
                        &user.created_at,
                        &session.display_timezone,
                        "%Y-%m-%d %H:%M:%S"
                    ),
                    &session.timezone_abbreviation
                ),
            })
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    let total_users = all_users.len() as i64;

    // Apply pagination manually
    let start_index = offset as usize;
    let end_index = std::cmp::min(start_index + USERS_PER_PAGE as usize, all_users.len());
    let org_users = if start_index < all_users.len() {
        all_users[start_index..end_index].to_vec()
    } else {
        Vec::new()
    };

    // Calculate pagination info
    let pagination = list_query::PaginationWindow::new(total_users, &page);
    let total_pages = pagination.total_pages;
    let has_next_page = pagination.has_next_page;
    let has_prev_page = pagination.has_prev_page;
    let start_page = pagination.start_page;
    let end_page = pagination.end_page;

    // Get pending invites
    let pending_invites =
        match hot::db::invite::Invite::get_invites_by_org(&db, &org.org_id, None, None).await {
            Ok(invites) => invites
                .into_iter()
                .filter(|invite| invite.get_status() == hot::db::invite::InviteStatus::Invited)
                .map(|invite| {
                    let status = invite.get_status().to_string();
                    let is_expired = invite.expires_at < chrono::Utc::now();
                    templates::InviteDisplay {
                        invite_id: invite.invite_id.to_string(),
                        invite_code: invite.invite_code,
                        email: invite.email,
                        role_name: match invite.intended_org_user_role_id {
                            2 => "Admin".to_string(),
                            _ => "Member".to_string(),
                        },
                        status,
                        created_at_formatted: format!(
                            "{} {}",
                            crate::timezone::format_in_timezone(
                                &invite.created_at,
                                &session.display_timezone,
                                "%Y-%m-%d %H:%M:%S"
                            ),
                            &session.timezone_abbreviation
                        ),
                        expires_at_formatted: format!(
                            "{} {}",
                            crate::timezone::format_in_timezone(
                                &invite.expires_at,
                                &session.display_timezone,
                                "%Y-%m-%d %H:%M:%S"
                            ),
                            &session.timezone_abbreviation
                        ),
                        is_expired,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        };

    // Check if user is admin
    let is_admin = session.is_org_admin(&db, &org.org_id).await;

    // Resolve team_members plan limit and current seat usage so the
    // template can show "X / Y members" and swap the Invite button for
    // an Upgrade CTA when at capacity. Seats-used = active members +
    // pending invites (every pending invite is a promised seat).
    let features = hot::db::Features::resolve_for_org(&db, &org.org_id).await;
    let raw_limit = features.team_members();
    let active_count = hot::db::org::OrgUser::count_active_members(&db, &org.org_id)
        .await
        .unwrap_or(0);
    let pending_count = hot::db::invite::Invite::count_pending_by_org(&db, &org.org_id)
        .await
        .unwrap_or(0);
    let team_members_used = active_count + pending_count;
    let team_members_limit = if raw_limit >= 0 {
        Some(raw_limit as i64)
    } else {
        None
    };
    let team_members_at_limit = team_members_limit
        .map(|lim| team_members_used >= lim)
        .unwrap_or(false);

    // Build breadcrumbs: <org> / Users
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Users".to_string()));

    let template = templates::OrgUsersList {
        title: &format!("Users - {}", org.name),
        page_context: templates::PrivatePageContext::for_org_page("users", &session, breadcrumbs),
        org,
        org_users,
        pending_invites,
        is_admin,
        active_page: "users",
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_users,
        team_members_used,
        team_members_limit,
        team_members_at_limit,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn org_users_invite_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get the organization
    let org = match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => org,
        Err(_) => return Redirect::to("/orgs").into_response(),
    };

    // Check if user is admin of this organization
    if !session.is_org_admin(&db, &org.org_id).await {
        return Redirect::to(&format!("/@{}/users", org_slug)).into_response();
    }

    // Belt-and-suspenders: if the org is already at its team_members
    // limit (active members + pending invites), don't render the empty
    // invite form — bounce back to /users where the at-limit CTA
    // explains the situation. The button on /users is also disabled in
    // this state, but a user could still navigate here by URL.
    let features = hot::db::Features::resolve_for_org(&db, &org.org_id).await;
    let max_members = features.team_members();
    if max_members >= 0 {
        let active_count = hot::db::org::OrgUser::count_active_members(&db, &org.org_id)
            .await
            .unwrap_or(0);
        let pending_count = hot::db::invite::Invite::count_pending_by_org(&db, &org.org_id)
            .await
            .unwrap_or(0);
        if active_count + pending_count >= max_members as i64 {
            return Redirect::to(&format!("/@{}/users", org_slug)).into_response();
        }
    }

    // Build breadcrumbs: <org> / Users / Invite
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Users".to_string(),
        format!("/@{}/users", org_slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Invite".to_string()));

    let template = templates::OrgUsersInvite {
        title: &format!("Invite User - {}", org.name),
        page_context: templates::PrivatePageContext::for_org_page("users", &session, breadcrumbs),
        org,
        error_message: "",
        success_message: "",
        email: "",
        role_id: 1, // Default to member role
        active_page: "users",
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn org_users_invite_post_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<InviteForm>,
) -> Result<Redirect, Html<String>> {
    // Local-dev experience is single-user oriented; self-host can invite users.
    if session.is_local_dev_experience() {
        let org = hot::db::org::Org::get_org_by_slug(&db, &org_slug)
            .await
            .map_err(|_| Html("Organization not found".to_string()))?;
        return Err(render_org_users_invite_with_error(
            &session,
            &org,
            "Inviting users is not available in local development.",
            &form.email,
            form.role_id,
        ));
    }

    // Get the organization
    let org = match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => org,
        Err(_) => return Ok(Redirect::to("/orgs")),
    };

    // Check if user is admin of this organization
    if !session.is_org_admin(&db, &org.org_id).await {
        return Ok(Redirect::to(&format!("/@{}/users", org_slug)));
    }

    // Validate form data
    if form.email.trim().is_empty() {
        return Err(render_org_users_invite_with_error(
            &session,
            &org,
            "Email is required",
            &form.email,
            form.role_id,
        ));
    }

    // Validate email format
    if !form.email.contains('@') {
        return Err(render_org_users_invite_with_error(
            &session,
            &org,
            "Invalid email format",
            &form.email,
            form.role_id,
        ));
    }

    // Check if user is already a member of the organization
    if let Ok(user) = hot::db::user::User::get_user_by_email(&db, &form.email).await
        && hot::db::org::OrgUser::get_org_user(&db, &org.org_id, &user.user_id)
            .await
            .is_ok()
    {
        return Err(render_org_users_invite_with_error(
            &session,
            &org,
            "User is already a member of this organization",
            &form.email,
            form.role_id,
        ));
    }

    // Check if there's already a pending invite for this email
    if let Ok(existing_invites) =
        hot::db::invite::Invite::get_invites_by_email(&db, &form.email).await
        && existing_invites.iter().any(|invite| {
            invite.org_id == org.org_id
                && invite.get_status() == hot::db::invite::InviteStatus::Invited
        })
    {
        return Err(render_org_users_invite_with_error(
            &session,
            &org,
            "An invite is already pending for this email",
            &form.email,
            form.role_id,
        ));
    }

    // Enforce team_members plan limit. We count both currently-active
    // members AND pending invites against the limit — every pending invite
    // is a seat we've already promised, so allowing more would result in
    // confusing acceptance failures for the invitee instead of an
    // upfront refusal here.
    let features = hot::db::Features::resolve_for_org(&db, &org.org_id).await;
    let max_members = features.team_members();
    if max_members >= 0 {
        let active_count = hot::db::org::OrgUser::count_active_members(&db, &org.org_id)
            .await
            .unwrap_or(0);
        let pending_count = hot::db::invite::Invite::count_pending_by_org(&db, &org.org_id)
            .await
            .unwrap_or(0);
        let seats_used = active_count + pending_count;
        if seats_used >= max_members as i64 {
            return Err(render_org_users_invite_with_error(
                &session,
                &org,
                &format!(
                    "Your plan allows up to {} team members and you've used {} \
                     (active members + pending invites). Please upgrade to add more.",
                    max_members, seats_used
                ),
                &form.email,
                form.role_id,
            ));
        }
    }

    // Create the invite
    let invite_id = uuid::Uuid::now_v7();
    let invite_code = hot::db::invite::Invite::generate_invite_code();
    let expires_at = chrono::Utc::now() + chrono::Duration::days(7); // 7 days from now

    match hot::db::invite::Invite::insert_invite(
        &db,
        &invite_id,
        &invite_code,
        &form.email,
        &org.org_id,
        form.role_id,
        &session.current_user_id(),
        expires_at,
    )
    .await
    {
        Ok(_) => {
            // Enqueue the invitation email for sending by the worker
            let email_enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
            if let Err(e) = email_enqueuer
                .send_invitation_email(&form.email, &org.name, &session.user_name, &invite_code)
                .await
            {
                tracing::error!("Failed to enqueue invitation email: {:?}", e);
                // Don't fail the invite, email is a nice-to-have
            }

            // Success - redirect to users list
            Ok(Redirect::to(&format!("/@{}/users", org_slug)))
        }
        Err(_) => Err(render_org_users_invite_with_error(
            &session,
            &org,
            "Failed to create invite",
            &form.email,
            form.role_id,
        )),
    }
}

pub async fn org_users_edit_handler(
    Path((org_slug, user_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get the organization
    let org = match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => org,
        Err(_) => return Redirect::to("/orgs").into_response(),
    };

    // Check if user is admin of this organization
    if !session.is_org_admin(&db, &org.org_id).await {
        return Redirect::to(&format!("/@{}/users", org_slug)).into_response();
    }

    // Get the user details
    let user = match hot::db::user::User::get_user(&db, &user_id).await {
        Ok(user) => user,
        Err(_) => return Redirect::to(&format!("/@{}/users", org_slug)).into_response(),
    };

    // Get the org user relationship
    let org_user = match hot::db::org::OrgUser::get_org_user(&db, &org.org_id, &user_id).await {
        Ok(org_user) => org_user,
        Err(_) => return Redirect::to(&format!("/@{}/users", org_slug)).into_response(),
    };

    let org_user_display = templates::OrgUserDisplay {
        user_id: user.user_id.to_string(),
        email: user.email,
        name: user.name,
        role_name: match org_user.org_user_role_id {
            2 => "Admin".to_string(),
            _ => "Member".to_string(),
        },
        org_user_role_id: org_user.org_user_role_id,
        active: org_user.active,
        created_at_formatted: format!(
            "{} {}",
            crate::timezone::format_in_timezone(
                &org_user.created_at,
                &session.display_timezone,
                "%Y-%m-%d %H:%M:%S"
            ),
            &session.timezone_abbreviation
        ),
    };

    // Build breadcrumbs: <org> / Users / <user_name> / Edit
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Users".to_string(),
        format!("/@{}/users", org_slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        org_user_display
            .name
            .clone()
            .unwrap_or_else(|| "User".to_string()),
        format!("/@{}/users/{}", org_slug, user_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::OrgUsersEdit {
        title: &format!("Edit User - {}", org.name),
        page_context: templates::PrivatePageContext::for_org_page("users", &session, breadcrumbs),
        org,
        org_user: org_user_display,
        error_message: "",
        active_page: "users",
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn org_users_edit_post_handler(
    Path((org_slug, user_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<OrgUserEditForm>,
) -> Result<Redirect, Html<String>> {
    // Local-dev experience is single-user oriented; self-host can manage users.
    if session.is_local_dev_experience() {
        return Ok(Redirect::to(&format!("/@{}/users", org_slug)));
    }

    // Get the organization
    let org = match hot::db::org::Org::get_org_by_slug(&db, &org_slug).await {
        Ok(org) => org,
        Err(_) => return Ok(Redirect::to("/orgs")),
    };

    // Check if user is admin of this organization
    if !session.is_org_admin(&db, &org.org_id).await {
        return Ok(Redirect::to(&format!("/@{}/users", org_slug)));
    }

    // Get the user details
    let user = match hot::db::user::User::get_user(&db, &user_id).await {
        Ok(user) => user,
        Err(_) => return Ok(Redirect::to(&format!("/@{}/users", org_slug))),
    };

    // Get the org user relationship
    let org_user = match hot::db::org::OrgUser::get_org_user(&db, &org.org_id, &user_id).await {
        Ok(org_user) => org_user,
        Err(_) => return Ok(Redirect::to(&format!("/@{}/users", org_slug))),
    };

    // Prevent user from changing their own role/status
    if user_id == session.current_user_id() {
        let org_user_display = templates::OrgUserDisplay {
            user_id: user.user_id.to_string(),
            email: user.email,
            name: user.name,
            role_name: match org_user.org_user_role_id {
                2 => "Admin".to_string(),
                _ => "Member".to_string(),
            },
            org_user_role_id: org_user.org_user_role_id,
            active: org_user.active,
            created_at_formatted: format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &org_user.created_at,
                    &session.display_timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                &session.timezone_abbreviation
            ),
        };

        return Err(render_org_users_edit_with_error(
            &session,
            &org,
            &org_user_display,
            "You cannot change your own role or status",
        ));
    }

    // Update the org user
    match hot::db::org::OrgUser::update_org_user(
        &db,
        &org.org_id,
        &user_id,
        form.role_id,
        form.active,
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Success - redirect to users list
            Ok(Redirect::to(&format!("/@{}/users", org_slug)))
        }
        Err(_) => {
            let org_user_display = templates::OrgUserDisplay {
                user_id: user.user_id.to_string(),
                email: user.email,
                name: user.name,
                role_name: match org_user.org_user_role_id {
                    2 => "Admin".to_string(),
                    _ => "Member".to_string(),
                },
                org_user_role_id: org_user.org_user_role_id,
                active: org_user.active,
                created_at_formatted: format!(
                    "{} {}",
                    crate::timezone::format_in_timezone(
                        &org_user.created_at,
                        &session.display_timezone,
                        "%Y-%m-%d %H:%M:%S"
                    ),
                    &session.timezone_abbreviation
                ),
            };

            Err(render_org_users_edit_with_error(
                &session,
                &org,
                &org_user_display,
                "Failed to update user",
            ))
        }
    }
}

// Helper function to render orgs new page with error
#[allow(clippy::too_many_arguments)]
fn render_orgs_new_with_error(
    session: &Session,
    error_message: &str,
    org_name: &str,
    org_slug: &str,
    account_type: &str,
    plans: Vec<hot::db::Plan>,
    selected_plan: &str,
    selected_billing: &str,
) -> Html<String> {
    let breadcrumbs = vec![
        templates::BreadcrumbItem::clickable("Organizations".to_string(), "/orgs".to_string()),
        templates::BreadcrumbItem::current("New".to_string()),
    ];

    let template = templates::OrgsNew {
        title: "New Organization",
        page_context: templates::PrivatePageContext::for_org_page("orgs", session, breadcrumbs),
        error_message,
        org_name,
        org_slug,
        account_type,
        plans,
        selected_plan,
        selected_billing,
        is_local_dev: !session.billing_enabled,
    };
    Html(template.render().unwrap())
}

// Helper function to render orgs edit page with error
fn render_orgs_edit_with_error(
    session: &Session,
    org: &hot::db::org::Org,
    error_message: &str,
) -> Html<String> {
    // Build breadcrumbs: Organizations / <org_name> / Edit
    let breadcrumbs = vec![
        templates::BreadcrumbItem::clickable("Organizations".to_string(), "/orgs".to_string()),
        templates::BreadcrumbItem::new(org.name.clone(), None),
        templates::BreadcrumbItem::current("Edit".to_string()),
    ];

    // Get org timezone setting
    let org_timezone =
        hot::db::org::Org::get_display_timezone(org).unwrap_or_else(|| "UTC".to_string());

    let template = templates::OrgsEdit {
        title: &format!("Edit {} - Organization", org.name),
        page_context: templates::PrivatePageContext::for_org_page(
            "organization",
            session,
            breadcrumbs,
        ),
        org: org.clone(),
        error_message,
        org_timezone,
    };
    Html(template.render().unwrap())
}

// Helper function to render org users invite page with error
fn render_org_users_invite_with_error(
    session: &Session,
    org: &hot::db::org::Org,
    error_message: &str,
    email: &str,
    role_id: i16,
) -> Html<String> {
    // Build breadcrumbs: <org> / Users / Invite
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Users".to_string(),
        format!("/@{}/users", org.slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Invite".to_string()));

    let template = templates::OrgUsersInvite {
        title: &format!("Invite User - {}", org.name),
        page_context: templates::PrivatePageContext::for_org_page("users", session, breadcrumbs),
        org: org.clone(),
        error_message,
        success_message: "",
        email,
        role_id,
        active_page: "users",
    };

    Html(template.render().unwrap())
}

// Helper function to render org users edit page with error
fn render_org_users_edit_with_error(
    session: &Session,
    org: &hot::db::org::Org,
    org_user: &templates::OrgUserDisplay,
    error_message: &str,
) -> Html<String> {
    // Build breadcrumbs: <org> / Users / <user_name> / Edit
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Users".to_string(),
        format!("/@{}/users", org.slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        org_user.name.clone().unwrap_or_else(|| "User".to_string()),
        format!("/@{}/users/{}", org.slug, org_user.user_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::OrgUsersEdit {
        title: &format!("Edit User - {}", org.name),
        page_context: templates::PrivatePageContext::for_org_page("users", session, breadcrumbs),
        org: org.clone(),
        org_user: org_user.clone(),
        error_message,
        active_page: "users",
    };

    Html(template.render().unwrap())
}

/// 301 redirect from legacy `/orgs/{slug}` paths to `/@{slug}`.
pub async fn legacy_org_redirect(
    axum::extract::Path(rest): axum::extract::Path<String>,
) -> impl IntoResponse {
    Redirect::permanent(&format!("/@{}", rest))
}

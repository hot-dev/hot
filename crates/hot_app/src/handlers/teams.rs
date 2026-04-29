use crate::auth::Session;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::DatabasePool;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

// Form data structure for team creation/editing
#[derive(Deserialize, Debug)]
pub struct TeamForm {
    pub name: String,
}

// Form data structure for adding team users
#[derive(Deserialize, Debug)]
pub struct TeamUserAddForm {
    pub user_id: Uuid,
    pub role_id: i16,
}

// Form data structure for editing team users
#[derive(Deserialize, Debug)]
pub struct TeamUserEditForm {
    pub role_id: i16,
    pub active: bool,
}

fn team_role_name(role_id: i16) -> String {
    match role_id {
        2 => "Admin",
        _ => "Member",
    }
    .to_string()
}

async fn can_manage_team_users(db: &DatabasePool, session: &Session, team_id: &Uuid) -> bool {
    if session.is_current_org_admin {
        return true;
    }

    hot::db::TeamUser::is_team_admin(db, team_id, &session.current_user_id())
        .await
        .unwrap_or(false)
}

pub async fn teams_list_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let _ = org_slug; // Used in route, org comes from session
    // Build breadcrumbs: <org> / Teams
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Teams".to_string()));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    const TEAMS_PER_PAGE: i64 = 10;

    // Calculate offset
    let offset = (current_page_num - 1) * TEAMS_PER_PAGE;

    // Get teams for current organization
    let (teams, total_teams) = if let Some(org) = &session.current_org {
        // Get all teams first to get total count
        let all_teams = hot::db::Team::get_teams_by_org(&db, &org.org_id)
            .await
            .unwrap_or_default();
        let total = all_teams.len() as i64;

        // Apply pagination manually
        let start_index = offset as usize;
        let end_index = std::cmp::min(start_index + TEAMS_PER_PAGE as usize, all_teams.len());
        let paginated_teams = if start_index < all_teams.len() {
            all_teams[start_index..end_index].to_vec()
        } else {
            Vec::new()
        };

        (paginated_teams, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = if total_teams > 0 {
        (total_teams + TEAMS_PER_PAGE - 1) / TEAMS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::TeamsList {
        title: "Teams",
        page_context: templates::PrivatePageContext::for_org_page("teams", &session, breadcrumbs),
        teams,
        is_admin: session.is_current_org_admin,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_teams,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn teams_new_handler(
    Path(org_slug): Path<String>,
    State(_db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> / Teams / New
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Teams".to_string(),
        format!("/@{}/teams", org_slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = templates::TeamsNew {
        title: "New Team",
        page_context: templates::PrivatePageContext::for_org_page("teams", &session, breadcrumbs),
        error_message: "",
        name: "",
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn teams_detail_handler(
    Path((org_slug, team_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get team details
    match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => {
            // Build breadcrumbs: <org> / Teams / <team_name>
            let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Teams".to_string(),
                format!("/@{}/teams", org_slug),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current(team.name.clone()));

            let template = templates::TeamsDetail {
                title: &format!("Team: {}", team.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "teams",
                    &session,
                    breadcrumbs,
                ),
                team,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Team not found
            let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Teams".to_string(),
                format!("/@{}/teams", org_slug),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("Not Found".to_string()));

            let template = templates::TeamsNotFound {
                title: "Team Not Found",
                page_context: templates::PrivatePageContext::for_org_page(
                    "teams",
                    &session,
                    breadcrumbs,
                ),
                team_id: team_id.to_string(),
                is_admin: session.is_current_org_admin,
            };

            Html(template.render().unwrap()).into_response()
        }
    }
}

pub async fn teams_edit_handler(
    Path((org_slug, team_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get team details
    match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => {
            // Build breadcrumbs: <org> / Teams / <team_name> / Edit
            let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Teams".to_string(),
                format!("/@{}/teams", org_slug),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                team.name.clone(),
                format!("/@{}/teams/{}", org_slug, team_id),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

            let template = templates::TeamsEdit {
                title: &format!("Edit Team: {}", team.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "teams",
                    &session,
                    breadcrumbs,
                ),
                team,
                error_message: "",
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Team not found, redirect to teams list
            Redirect::to(&format!("/@{}/teams", org_slug)).into_response()
        }
    }
}

pub async fn team_users_list_handler(
    Path((org_slug, team_id)): Path<(String, Uuid)>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    const USERS_PER_PAGE: i64 = 10;

    // Calculate offset
    let offset = (current_page_num - 1) * USERS_PER_PAGE;

    // Get team details
    match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => {
            // Get team users with roles
            let all_team_users = hot::db::TeamUser::get_users_with_roles_by_team(&db, &team_id)
                .await
                .unwrap_or_default();

            let total_team_users = all_team_users.len() as i64;

            // Apply pagination manually
            let start_index = offset as usize;
            let end_index =
                std::cmp::min(start_index + USERS_PER_PAGE as usize, all_team_users.len());
            let team_users = if start_index < all_team_users.len() {
                all_team_users[start_index..end_index].to_vec()
            } else {
                Vec::new()
            };

            // Calculate pagination info
            let total_pages = if total_team_users > 0 {
                (total_team_users + USERS_PER_PAGE - 1) / USERS_PER_PAGE
            } else {
                1
            };
            let has_next_page = current_page_num < total_pages;
            let has_prev_page = current_page_num > 1;

            // Calculate pagination window
            let start_page = std::cmp::max(1, current_page_num - 2);
            let end_page = std::cmp::min(total_pages, current_page_num + 2);

            let can_manage = can_manage_team_users(&db, &session, &team_id).await;

            // Build breadcrumbs: <org> / Teams / <team_name> / Users
            let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Teams".to_string(),
                format!("/@{}/teams", org_slug),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                team.name.clone(),
                format!("/@{}/teams/{}", org_slug, team_id),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("Users".to_string()));

            let template = templates::TeamUsersList {
                title: &format!("Team Users: {}", team.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "teams",
                    &session,
                    breadcrumbs,
                ),
                team,
                team_users,
                can_manage,
                current_page_num,
                total_pages,
                start_page,
                end_page,
                has_next_page,
                has_prev_page,
                total_team_users,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Team not found, redirect to teams list
            Redirect::to(&format!("/@{}/teams", org_slug)).into_response()
        }
    }
}

pub async fn team_users_add_handler(
    Path((org_slug, team_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get team details
    match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => {
            // Get available users from the organization
            let available_users = if let Some(org) = &session.current_org {
                hot::db::OrgUser::get_users_with_roles_by_org(&db, &org.org_id)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            // Build breadcrumbs: <org> / Teams / <team_name> / Users / Add
            let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Teams".to_string(),
                format!("/@{}/teams", org_slug),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                team.name.clone(),
                format!("/@{}/teams/{}", org_slug, team_id),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Users".to_string(),
                format!("/@{}/teams/{}/users", org_slug, team_id),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("Add".to_string()));

            let template = templates::TeamUsersAdd {
                title: &format!("Add User to Team: {}", team.name),
                page_context: templates::PrivatePageContext::for_org_page(
                    "teams",
                    &session,
                    breadcrumbs,
                ),
                team,
                available_users,
                error_message: "",
                selected_user_id: None,
                selected_role_id: 1, // Default to member role
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Team not found, redirect to teams list
            Redirect::to(&format!("/@{}/teams", org_slug)).into_response()
        }
    }
}

pub async fn team_users_edit_handler(
    Path((org_slug, team_id, user_id)): Path<(String, Uuid, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get team details
    match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => {
            // Get team user details
            match hot::db::TeamUser::get_team_user(&db, &team_id, &user_id).await {
                Ok(team_user) => {
                    // Get user details for display
                    if let Ok(user) = hot::db::User::get_user(&db, &user_id).await {
                        let team_user_display = templates::TeamUserDisplay {
                            user_id: user.user_id,
                            email: user.email,
                            name: user.name.unwrap_or_else(|| "Unknown".to_string()),
                            role_name: team_role_name(team_user.team_user_role_id),
                            team_user_role_id: team_user.team_user_role_id,
                            active: team_user.active,
                            created_at_formatted: format!(
                                "{} {}",
                                crate::timezone::format_in_timezone(
                                    &team_user.created_at,
                                    &session.display_timezone,
                                    "%Y-%m-%d %H:%M:%S"
                                ),
                                &session.timezone_abbreviation
                            ),
                        };

                        // Build breadcrumbs: <org> / Teams / <team_name> / Users / <user_name> / Edit
                        let mut breadcrumbs =
                            templates::build_base_breadcrumbs_without_env(&session);
                        breadcrumbs.push(templates::BreadcrumbItem::clickable(
                            "Teams".to_string(),
                            format!("/@{}/teams", org_slug),
                        ));
                        breadcrumbs.push(templates::BreadcrumbItem::clickable(
                            team.name.clone(),
                            format!("/@{}/teams/{}", org_slug, team_id),
                        ));
                        breadcrumbs.push(templates::BreadcrumbItem::clickable(
                            "Users".to_string(),
                            format!("/@{}/teams/{}/users", org_slug, team_id),
                        ));
                        breadcrumbs.push(templates::BreadcrumbItem::clickable(
                            team_user_display.name.clone(),
                            format!("/@{}/teams/{}/users/{}", org_slug, team_id, user_id),
                        ));
                        breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

                        let template = templates::TeamUsersEdit {
                            title: &format!("Edit Team User: {}", team_user_display.name),
                            page_context: templates::PrivatePageContext::for_org_page(
                                "teams",
                                &session,
                                breadcrumbs,
                            ),
                            team,
                            team_user: team_user_display,
                            error_message: "",
                        };

                        Html(template.render().unwrap()).into_response()
                    } else {
                        // User not found, redirect to team users list
                        Redirect::to(&format!("/@{}/teams/{}/users", org_slug, team_id))
                            .into_response()
                    }
                }
                Err(_) => {
                    // Team user not found, redirect to team users list
                    Redirect::to(&format!("/@{}/teams/{}/users", org_slug, team_id)).into_response()
                }
            }
        }
        Err(_) => {
            // Team not found, redirect to teams list
            Redirect::to(&format!("/@{}/teams", org_slug)).into_response()
        }
    }
}

pub async fn teams_create_handler(
    Path(org_slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<TeamForm>,
) -> Result<Redirect, Html<String>> {
    let _ = org_slug; // Used in route, org comes from session
    // Local-dev experience is single-user oriented; self-host can create teams.
    if session.is_local_dev_experience() {
        return Err(render_teams_new_with_error(
            &session,
            "Creating teams is not available in local development.",
            &form.name,
        ));
    }

    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Err(Html("No organization selected".to_string()));
        }
    };

    // Check if user is admin of this organization
    if !session.is_current_org_admin {
        return Err(render_teams_new_with_error(
            &session,
            "You must be an admin to create teams",
            &form.name,
        ));
    }

    // Validate form
    if form.name.trim().is_empty() {
        return Err(render_teams_new_with_error(
            &session,
            "Team name is required",
            &form.name,
        ));
    }

    // Check if team name already exists in this org
    match hot::db::team::Team::get_teams_by_org(&db, &current_org.org_id).await {
        Ok(teams) => {
            if teams.iter().any(|t| t.name == form.name) {
                return Err(render_teams_new_with_error(
                    &session,
                    "Team name already exists",
                    &form.name,
                ));
            }
        }
        Err(_) => {
            return Err(render_teams_new_with_error(
                &session,
                "Failed to check existing teams",
                &form.name,
            ));
        }
    }

    // Generate new team ID
    let team_id = uuid::Uuid::now_v7();

    // Create team
    match hot::db::team::Team::insert_team(
        &db,
        &team_id,
        &current_org.org_id,
        &form.name,
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Add the creator as an admin of the team
            let team_user_id = uuid::Uuid::now_v7();
            if (hot::db::team::TeamUser::insert_team_user(
                &db,
                &team_user_id,
                &team_id,
                &session.current_user_id(),
                Some(2),
                &session.current_user_id(),
            )
            .await)
                .is_ok()
            {
                // Use org slug from session for redirect
                if let Some(ref org) = session.current_org {
                    Ok(Redirect::to(&format!("/@{}/teams", org.slug)))
                } else {
                    Ok(Redirect::to("/orgs"))
                }
            } else {
                Err(render_teams_new_with_error(
                    &session,
                    "Failed to add you as team admin",
                    &form.name,
                ))
            }
        }
        Err(_) => Err(render_teams_new_with_error(
            &session,
            "Failed to create team",
            &form.name,
        )),
    }
}

pub async fn teams_update_handler(
    Path((org_slug, team_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<TeamForm>,
) -> Result<Redirect, Html<String>> {
    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Err(Html("No organization selected".to_string()));
        }
    };

    // Get the team
    let team = match hot::db::team::Team::get_team(&db, &team_id).await {
        Ok(team) => team,
        Err(_) => return Ok(Redirect::to(&format!("/@{}/teams", org_slug))),
    };

    // Check if team belongs to current organization
    if team.org_id != current_org.org_id {
        return Ok(Redirect::to(&format!("/@{}/teams", org_slug)));
    }

    // Check if user is admin of this organization
    if !session.is_current_org_admin {
        return Err(render_teams_edit_with_error(
            &session,
            &team,
            "You must be an admin to edit teams",
        ));
    }

    // Validate form
    if form.name.trim().is_empty() {
        return Err(render_teams_edit_with_error(
            &session,
            &team,
            "Team name is required",
        ));
    }

    // Check if team name already exists in this org (but allow current name)
    if form.name != team.name {
        match hot::db::team::Team::get_teams_by_org(&db, &current_org.org_id).await {
            Ok(teams) => {
                if teams.iter().any(|t| t.name == form.name) {
                    return Err(render_teams_edit_with_error(
                        &session,
                        &team,
                        "Team name already exists",
                    ));
                }
            }
            Err(_) => {
                return Err(render_teams_edit_with_error(
                    &session,
                    &team,
                    "Failed to check existing teams",
                ));
            }
        }
    }

    // Update team
    match hot::db::team::Team::update_name(&db, &team_id, &form.name).await {
        Ok(_) => {
            // Use org slug from session for redirect
            if let Some(ref org) = session.current_org {
                Ok(Redirect::to(&format!("/@{}/teams", org.slug)))
            } else {
                Ok(Redirect::to("/orgs"))
            }
        }
        Err(_) => Err(render_teams_edit_with_error(
            &session,
            &team,
            "Failed to update team",
        )),
    }
}

pub async fn team_users_add_post_handler(
    Path((org_slug, team_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<TeamUserAddForm>,
) -> Result<Redirect, Html<String>> {
    // Local-dev experience is single-user oriented; self-host can manage teams.
    if session.is_local_dev_experience() {
        return Ok(Redirect::to(&format!(
            "/@{}/teams/{}/users",
            org_slug, team_id
        )));
    }

    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Err(Html("No organization selected".to_string()));
        }
    };

    // Get the team
    let team = match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => team,
        Err(_) => return Ok(Redirect::to(&format!("/@{}/teams", org_slug))),
    };

    // Check if team belongs to current organization
    if team.org_id != current_org.org_id {
        return Ok(Redirect::to(&format!("/@{}/teams", org_slug)));
    }

    // Check if user can manage team users.
    if !can_manage_team_users(&db, &session, &team_id).await {
        return Err(Html("You must be an admin to add team users".to_string()));
    }

    // Check if user is already a team member
    if hot::db::TeamUser::get_team_user(&db, &team_id, &form.user_id)
        .await
        .is_ok()
    {
        return Err(render_team_users_add_with_error(
            &db,
            &session,
            &team,
            "User is already a member of this team",
            Some(form.user_id),
            form.role_id,
        )
        .await);
    }

    // Add user to team
    let team_user_id = uuid::Uuid::now_v7();
    match hot::db::TeamUser::insert_team_user(
        &db,
        &team_user_id,
        &team_id,
        &form.user_id,
        Some(form.role_id),
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Success - redirect to team users list
            Ok(Redirect::to(&format!(
                "/@{}/teams/{}/users",
                current_org.slug, team_id
            )))
        }
        Err(_) => Err(render_team_users_add_with_error(
            &db,
            &session,
            &team,
            "Failed to add user to team",
            Some(form.user_id),
            form.role_id,
        )
        .await),
    }
}

pub async fn team_users_edit_post_handler(
    Path((org_slug, team_id, user_id)): Path<(String, Uuid, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<TeamUserEditForm>,
) -> Result<Redirect, Html<String>> {
    // Local-dev experience is single-user oriented; self-host can manage teams.
    if session.is_local_dev_experience() {
        return Ok(Redirect::to(&format!(
            "/@{}/teams/{}/users",
            org_slug, team_id
        )));
    }

    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Err(Html("No organization selected".to_string()));
        }
    };

    // Get the team
    let team = match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => team,
        Err(_) => return Ok(Redirect::to(&format!("/@{}/teams", org_slug))),
    };

    // Check if team belongs to current organization
    if team.org_id != current_org.org_id {
        return Ok(Redirect::to(&format!("/@{}/teams", org_slug)));
    }

    // Check if user can manage team users.
    if !can_manage_team_users(&db, &session, &team_id).await {
        return Err(Html("You must be an admin to edit team users".to_string()));
    }

    // Get the team user
    let team_user = match hot::db::TeamUser::get_team_user(&db, &team_id, &user_id).await {
        Ok(team_user) => team_user,
        Err(_) => {
            return Ok(Redirect::to(&format!(
                "/@{}/teams/{}/users",
                org_slug, team_id
            )));
        }
    };

    // Get user details for display
    let user = match hot::db::User::get_user(&db, &user_id).await {
        Ok(user) => user,
        Err(_) => {
            return Ok(Redirect::to(&format!(
                "/@{}/teams/{}/users",
                org_slug, team_id
            )));
        }
    };

    let team_user_display = templates::TeamUserDisplay {
        user_id: user.user_id,
        email: user.email,
        name: user.name.unwrap_or_else(|| "Unknown".to_string()),
        role_name: team_role_name(team_user.team_user_role_id),
        team_user_role_id: team_user.team_user_role_id,
        active: team_user.active,
        created_at_formatted: format!(
            "{} {}",
            crate::timezone::format_in_timezone(
                &team_user.created_at,
                &session.display_timezone,
                "%Y-%m-%d %H:%M:%S"
            ),
            &session.timezone_abbreviation
        ),
    };

    // Prevent user from changing their own role/status
    if user_id == session.current_user_id() {
        return Err(render_team_users_edit_with_error(
            &session,
            &team,
            &team_user_display,
            "You cannot change your own role or status",
        ));
    }

    // Update the team user
    match hot::db::TeamUser::update_team_user(
        &db,
        &team_id,
        &user_id,
        form.role_id,
        form.active,
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => {
            // Success - redirect to team users list
            Ok(Redirect::to(&format!(
                "/@{}/teams/{}/users",
                current_org.slug, team_id
            )))
        }
        Err(_) => Err(render_team_users_edit_with_error(
            &session,
            &team,
            &team_user_display,
            "Failed to update team user",
        )),
    }
}

pub async fn team_users_remove_post_handler(
    Path((org_slug, team_id, user_id)): Path<(String, Uuid, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Local-dev experience is single-user oriented; self-host can manage teams.
    if session.is_local_dev_experience() {
        return Redirect::to(&format!("/@{}/teams/{}/users", org_slug, team_id));
    }

    let current_org = match &session.current_org {
        Some(org) => org,
        None => {
            return Redirect::to(&format!("/@{}/teams", org_slug));
        }
    };

    // Get the team
    let team = match hot::db::Team::get_team(&db, &team_id).await {
        Ok(team) => team,
        Err(_) => return Redirect::to(&format!("/@{}/teams", org_slug)),
    };

    // Check if team belongs to current organization
    if team.org_id != current_org.org_id {
        return Redirect::to(&format!("/@{}/teams", org_slug));
    }

    // Check if user can manage team users.
    if !can_manage_team_users(&db, &session, &team_id).await {
        return Redirect::to(&format!("/@{}/teams/{}/users", org_slug, team_id));
    }

    // Prevent user from removing themselves
    if user_id == session.current_user_id() {
        return Redirect::to(&format!("/@{}/teams/{}/users", org_slug, team_id));
    }

    // Remove the team user
    let _ = hot::db::TeamUser::remove_team_user(&db, &team_id, &user_id).await;

    // Redirect back to team users list
    Redirect::to(&format!("/@{}/teams/{}/users", org_slug, team_id))
}

// Helper function to render teams new page with error
fn render_teams_new_with_error(session: &Session, error_message: &str, name: &str) -> Html<String> {
    // Build breadcrumbs: <org> / Teams / New
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    let teams_url = session
        .current_org
        .as_ref()
        .map(|o| format!("/@{}/teams", o.slug))
        .unwrap_or_else(|| "/orgs".to_string());
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Teams".to_string(),
        teams_url,
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = templates::TeamsNew {
        title: "New Team",
        page_context: templates::PrivatePageContext::for_org_page("teams", session, breadcrumbs),
        error_message,
        name,
    };

    Html(template.render().unwrap())
}

// Helper function to render teams edit page with error
fn render_teams_edit_with_error(
    session: &Session,
    team: &hot::db::team::Team,
    error_message: &str,
) -> Html<String> {
    // Build breadcrumbs: <org> / Teams / <team_name> / Edit
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    let org_slug = session
        .current_org
        .as_ref()
        .map(|o| o.slug.clone())
        .unwrap_or_default();
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Teams".to_string(),
        format!("/@{}/teams", org_slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::new(team.name.clone(), None));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::TeamsEdit {
        title: &format!("Edit Team: {}", team.name),
        page_context: templates::PrivatePageContext::for_org_page("teams", session, breadcrumbs),
        team: team.clone(),
        error_message,
    };

    Html(template.render().unwrap())
}

// Helper function to render team users add page with error
async fn render_team_users_add_with_error(
    db: &DatabasePool,
    session: &Session,
    team: &hot::db::team::Team,
    error_message: &str,
    selected_user_id: Option<Uuid>,
    selected_role_id: i16,
) -> Html<String> {
    // Get available users from the organization
    let available_users = if let Some(org) = &session.current_org {
        hot::db::OrgUser::get_users_with_roles_by_org(db, &org.org_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Build breadcrumbs: <org> / Teams / <team_name> / Users / Add
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    let org_slug = session
        .current_org
        .as_ref()
        .map(|o| o.slug.clone())
        .unwrap_or_default();
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Teams".to_string(),
        format!("/@{}/teams", org_slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        team.name.clone(),
        format!("/@{}/teams/{}", org_slug, team.team_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Users".to_string(),
        format!("/@{}/teams/{}/users", org_slug, team.team_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Add".to_string()));

    let template = templates::TeamUsersAdd {
        title: &format!("Add User to Team: {}", team.name),
        page_context: templates::PrivatePageContext::for_org_page("teams", session, breadcrumbs),
        team: team.clone(),
        available_users,
        error_message,
        selected_user_id,
        selected_role_id,
    };

    Html(template.render().unwrap())
}

// Helper function to render team users edit page with error
fn render_team_users_edit_with_error(
    session: &Session,
    team: &hot::db::team::Team,
    team_user: &templates::TeamUserDisplay,
    error_message: &str,
) -> Html<String> {
    // Build breadcrumbs: <org> / Teams / <team_name> / Users / <user_name> / Edit
    let mut breadcrumbs = templates::build_base_breadcrumbs_without_env(session);
    let org_slug = session
        .current_org
        .as_ref()
        .map(|o| o.slug.clone())
        .unwrap_or_default();
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Teams".to_string(),
        format!("/@{}/teams", org_slug),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        team.name.clone(),
        format!("/@{}/teams/{}", org_slug, team.team_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Users".to_string(),
        format!("/@{}/teams/{}/users", org_slug, team.team_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        team_user.name.clone(),
        format!(
            "/@{}/teams/{}/users/{}",
            org_slug, team.team_id, team_user.user_id
        ),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::TeamUsersEdit {
        title: &format!("Edit Team User: {}", team_user.name),
        page_context: templates::PrivatePageContext::for_org_page("teams", session, breadcrumbs),
        team: team.clone(),
        team_user: team_user.clone(),
        error_message,
    };

    Html(template.render().unwrap())
}

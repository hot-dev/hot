use crate::auth::get_user_id_from_cookies;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use axum_extra::extract::CookieJar;
use hot::db::DatabasePool;
use serde::Deserialize;
use std::sync::Arc;

// Import common functions from parent module
use super::process_invite_code;

#[derive(Deserialize, Debug)]
pub struct InviteAcceptForm {
    pub code: String,
}

// Invite acceptance handler (GET) - shows invite details / confirmation page
pub async fn invite_accept_handler(
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    cookies: CookieJar,
) -> impl IntoResponse {
    let invite_code = match params.get("code") {
        Some(code) => code,
        None => {
            return Html(
                r#"
                <html>
                <head><title>Invalid Invite Link</title></head>
                <body>
                <h1>Invalid Invite Link</h1>
                <p>The invite link is invalid or missing the invite code.</p>
                </body>
                </html>
                "#
                .to_string(),
            )
            .into_response();
        }
    };

    // Get invite by code
    let invite = match hot::db::invite::Invite::get_invite_by_code(&db, invite_code).await {
        Ok(invite) => invite,
        Err(_) => {
            return Html(
                r#"
                <html>
                <head><title>Invalid Invite</title></head>
                <body>
                <h1>Invalid Invite</h1>
                <p>The invite link is invalid or has expired.</p>
                </body>
                </html>
                "#
                .to_string(),
            )
            .into_response();
        }
    };

    // Check if invite is valid
    if invite.is_valid().is_err() {
        return Html(
            r#"
            <html>
            <head><title>Invalid Invite</title></head>
            <body>
            <h1>Invalid Invite</h1>
            <p>The invite link is invalid or has expired.</p>
            </body>
            </html>
            "#
            .to_string(),
        )
        .into_response();
    }

    // Check if user is already authenticated
    let is_authenticated = if let Some(user_id) = get_user_id_from_cookies(&cookies) {
        match hot::db::user::User::get_user(&db, &user_id).await {
            Ok(user) => {
                if user.email == invite.email {
                    true
                } else {
                    // Email doesn't match, show error
                    let page_context = templates::PublicPageContext::new("invite");
                    let template = templates::InviteAccept {
                        title: "Accept Invitation",
                        page_context,
                        invite_code,
                        email: &invite.email,
                        org_name: "",
                        role_name: "",
                        invited_by_name: "",
                        error_message: &format!(
                            "This invite is for {} but you are signed in as {}. \
                             Please sign out and sign in with the correct email, \
                             or create a new account.",
                            invite.email, user.email
                        ),
                        is_authenticated: false,
                    };
                    return Html(template.render().unwrap()).into_response();
                }
            }
            Err(_) => false,
        }
    } else {
        false
    };

    // Get organization details
    let org = match hot::db::org::Org::get_org(&db, &invite.org_id).await {
        Ok(org) => org,
        Err(_) => {
            return Html(
                r#"
                <html>
                <head><title>Organization Not Found</title></head>
                <body>
                <h1>Organization Not Found</h1>
                <p>The organization associated with this invite no longer exists.</p>
                </body>
                </html>
                "#
                .to_string(),
            )
            .into_response();
        }
    };

    // Get role name
    let role_name = match invite.intended_org_user_role_id {
        1 => "Member",
        2 => "Admin",
        _ => "Unknown",
    };

    // Get invited by user info
    let invited_by_name = match hot::db::user::User::get_user(&db, &invite.created_by_user_id).await
    {
        Ok(user) => user.name.unwrap_or_else(|| user.email.clone()),
        Err(_) => "Unknown".to_string(),
    };

    let page_context = templates::PublicPageContext::new("invite");

    let template = templates::InviteAccept {
        title: "Accept Invitation",
        page_context,
        invite_code,
        email: &invite.email,
        org_name: &org.name,
        role_name,
        invited_by_name: &invited_by_name,
        error_message: "",
        is_authenticated,
    };

    Html(template.render().unwrap()).into_response()
}

// Invite acceptance handler (POST) - actually processes the invite
pub async fn invite_accept_post_handler(
    State(db): State<Arc<DatabasePool>>,
    cookies: CookieJar,
    Form(form): Form<InviteAcceptForm>,
) -> impl IntoResponse {
    let invite_code = &form.code;

    // Must be authenticated
    let user_id = match get_user_id_from_cookies(&cookies) {
        Some(id) => id,
        None => {
            return Redirect::to(&format!("/signin?invite_code={}", invite_code)).into_response();
        }
    };

    // Verify user exists
    let user = match hot::db::user::User::get_user(&db, &user_id).await {
        Ok(user) => user,
        Err(_) => {
            return Redirect::to(&format!("/signin?invite_code={}", invite_code)).into_response();
        }
    };

    // Verify invite exists and is valid
    let invite = match hot::db::invite::Invite::get_invite_by_code(&db, invite_code).await {
        Ok(invite) => invite,
        Err(_) => {
            return Html(
                r#"<html><head><title>Invalid Invite</title></head>
                <body><h1>Invalid Invite</h1>
                <p>The invite link is invalid or has expired.</p></body></html>"#
                    .to_string(),
            )
            .into_response();
        }
    };

    if invite.is_valid().is_err() {
        return Html(
            r#"<html><head><title>Invalid Invite</title></head>
            <body><h1>Invalid Invite</h1>
            <p>The invite link is invalid or has expired.</p></body></html>"#
                .to_string(),
        )
        .into_response();
    }

    // Verify email matches
    if user.email != invite.email {
        return Html(format!(
            r#"<html><head><title>Email Mismatch</title></head>
            <body><h1>Email Mismatch</h1>
            <p>This invite is for {} but you are signed in as {}.</p>
            <p><a href="/">Go to Dashboard</a></p></body></html>"#,
            invite.email, user.email
        ))
        .into_response();
    }

    // Process the invite
    match process_invite_code(&db, &user_id, invite_code).await {
        Ok(_) => Redirect::to("/?invite_accepted=1").into_response(),
        Err(error) => {
            let page_context = templates::PublicPageContext::new("invite");
            let template = templates::InviteAccept {
                title: "Accept Invitation",
                page_context,
                invite_code,
                email: &invite.email,
                org_name: "",
                role_name: "",
                invited_by_name: "",
                error_message: &error,
                is_authenticated: true,
            };
            Html(template.render().unwrap()).into_response()
        }
    }
}
